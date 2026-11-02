// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The streaming protocol over `postMessage`: a geometry server in a Web
//! Worker, and a [`GeometryStream`] to it on the page.
//!
//! ```text
//!   page                                      worker
//!   WorkerStream ── ClientMessage bytes ──▶   serve(InProcessStream)
//!                ◀── ServerMessage bytes ──   GeometryServer::run
//! ```
//!
//! Each protocol message crosses as one `ArrayBuffer`, transferred rather than
//! copied (see [`crate::wire`]). Anything else posted on the same port — an
//! object, a string — is not the protocol's and is handed to the host's own
//! handler, so a host can speak to its worker beside the stream without a
//! second channel.
//!
//! The page drains the stream with a poll from its frame loop, exactly as a
//! desktop drains the in-process binding: a worker mid-decode cannot stall a
//! frame, and a frame that skips cannot lose a message.

use std::cell::RefCell;
use std::rc::Rc;
use std::task::{Context, Poll};

use futures_channel::mpsc;
use futures_util::StreamExt;
use tuile_core::protocol::{
    ClientMessage, GeometryStream, InProcessStream, ServerMessage, StreamError,
};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};

use crate::wire;

/// One end of a worker's message channel: the page's handle on the worker, or
/// the worker's own global scope.
#[derive(Clone)]
enum Port {
    Worker(web_sys::Worker),
    Scope(web_sys::DedicatedWorkerGlobalScope),
}

impl Port {
    /// Transfers `bytes` to the other side as one `ArrayBuffer`.
    fn post(&self, bytes: &[u8]) {
        let buffer = js_sys::Uint8Array::from(bytes).buffer();
        let transfer = js_sys::Array::of1(&buffer);
        let posted = match self {
            Port::Worker(w) => w.post_message_with_transfer(&buffer, &transfer),
            Port::Scope(s) => s.post_message_with_transfer(&buffer, &transfer),
        };
        if let Err(e) = posted {
            tracing::error!("a protocol message could not be posted: {e:?}");
        }
    }

    fn listen(&self, handler: Option<&js_sys::Function>) {
        match self {
            Port::Worker(w) => w.set_onmessage(handler),
            Port::Scope(s) => s.set_onmessage(handler),
        }
    }
}

/// Keeps a port's message handler alive; dropping it stops the listening.
///
/// The handler is a Rust closure the browser calls, and it lives exactly as
/// long as this value — so a stream torn down by its host leaves no handler
/// behind to fire into freed state.
pub struct Connection {
    port: Port,
    _handler: Closure<dyn FnMut(web_sys::MessageEvent)>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.port.listen(None);
    }
}

/// Installs `on_frame` for protocol buffers and `host` for everything else.
fn connect(
    port: Port,
    mut on_frame: impl FnMut(Vec<u8>) + 'static,
    mut host: impl FnMut(JsValue) + 'static,
) -> Connection {
    let handler =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            let data = event.data();
            match data.dyn_ref::<js_sys::ArrayBuffer>() {
                Some(buffer) => on_frame(js_sys::Uint8Array::new(buffer).to_vec()),
                None => host(data),
            }
        });
    port.listen(Some(handler.as_ref().unchecked_ref()));
    Connection {
        port,
        _handler: handler,
    }
}

/// The page's [`GeometryStream`] to a server running in `worker`.
///
/// `Send`, as the trait requires, because it holds only channels: the worker
/// handle, which is a JavaScript object, stays in the task that posts.
pub struct WorkerStream {
    outbox: mpsc::UnboundedSender<ClientMessage>,
    inbox: mpsc::UnboundedReceiver<ServerMessage>,
}

impl WorkerStream {
    /// Connects to `worker`. Protocol messages feed the stream; whatever
    /// else the worker posts goes to `host`.
    ///
    /// The stream is open while both the stream and the returned
    /// [`Connection`] are alive.
    pub fn connect(
        worker: &web_sys::Worker,
        host: impl FnMut(JsValue) + 'static,
    ) -> (Self, Connection) {
        let port = Port::Worker(worker.clone());
        let (to_page, inbox) = mpsc::unbounded();
        let connection = connect(
            port.clone(),
            move |bytes| match wire::unframe(&bytes) {
                Ok(messages) => {
                    for bytes in messages {
                        match wire::decode_server(bytes) {
                            // A closed inbox is a stream its host has dropped.
                            Ok(message) => {
                                let _ = to_page.unbounded_send(message);
                            }
                            Err(e) => {
                                tracing::error!("a message from the worker did not read back: {e}")
                            }
                        }
                    }
                }
                Err(e) => tracing::error!("a frame from the worker did not read back: {e}"),
            },
            host,
        );
        let (outbox, mut outgoing) = mpsc::unbounded::<ClientMessage>();
        // Posted from a task rather than from `send`, which must be `Send` and
        // so cannot hold the worker. It runs as soon as the frame that sent
        // returns to the browser, and ends with the stream.
        wasm_bindgen_futures::spawn_local(async move {
            while let Some(message) = outgoing.next().await {
                // Everything sent by now goes in one post: a frame's
                // acknowledgements and its camera, decided on once (see
                // `wire::frame`).
                let mut batch = vec![wire::encode_client(&message)];
                while let Ok(more) = outgoing.try_recv() {
                    batch.push(wire::encode_client(&more));
                }
                port.post(&wire::frame(&batch));
            }
        });
        (Self { outbox, inbox }, connection)
    }
}

impl GeometryStream for WorkerStream {
    fn send(&self, msg: ClientMessage) -> Result<(), StreamError> {
        self.outbox
            .unbounded_send(msg)
            .map_err(|_| StreamError::Closed)
    }

    fn poll_message(&mut self, cx: &mut Context<'_>) -> Poll<Option<ServerMessage>> {
        self.inbox.poll_next_unpin(cx)
    }
}

/// Serves `stream` — the consumer end of a geometry server running in this
/// worker — to the page that started the worker.
///
/// Client messages from the page are sent on the stream; everything the
/// server produces is posted back. Whatever else the page posts goes to
/// `host`. Runs until the server closes the stream.
pub fn serve(stream: InProcessStream, host: impl FnMut(JsValue) + 'static) -> Connection {
    let scope: web_sys::DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
    let port = Port::Scope(scope);
    // Shared between the handler that sends and the task that receives. Each
    // borrow lasts one call: the task borrows only inside a poll, never across
    // an await, so the handler always finds the stream free.
    let stream = Rc::new(RefCell::new(stream));
    let sending = Rc::clone(&stream);
    let connection = connect(
        port.clone(),
        move |bytes| match wire::unframe(&bytes) {
            Ok(messages) => {
                for bytes in messages {
                    match wire::decode_client(bytes) {
                        Ok(message) => {
                            if sending.borrow().send(message).is_err() {
                                tracing::warn!(
                                    "the geometry server has stopped; a message was dropped"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::error!("a message from the page did not read back: {e}")
                        }
                    }
                }
            }
            Err(e) => tracing::error!("a frame from the page did not read back: {e}"),
        },
        host,
    );
    wasm_bindgen_futures::spawn_local(async move {
        while let Some(message) =
            futures_util::future::poll_fn(|cx| stream.borrow_mut().poll_message(cx)).await
        {
            // What the server has sent by now goes in one post.
            let mut batch = vec![wire::encode_server(&message)];
            let waker = futures_util::task::noop_waker();
            let mut cx = Context::from_waker(&waker);
            while let Poll::Ready(Some(more)) = stream.borrow_mut().poll_message(&mut cx) {
                batch.push(wire::encode_server(&more));
            }
            port.post(&wire::frame(&batch));
        }
    });
    connection
}
