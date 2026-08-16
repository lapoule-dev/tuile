// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The streaming mode IS this module: the protocol of the geometry server,
//! defined as a trait, independent of any transport.
//!
//! Design decisions (`docs/01-architecture.md`):
//! - **Receiving is async** — [`GeometryStream::poll_message`] is a stream
//!   in poll form; consumers `.await` messages.
//! - **Sending is synchronous and non-blocking** — `send` is called from
//!   render loops (a `ViewerState` per frame) which must never await.
//! - Consumer-side `Content` is always [`TileContent::Decoded`]; raw glb is
//!   what network bindings put on the wire (M2).
//!
//! Bindings: [`InProcessStream`] here (channels, zero serialization); the
//! WebSocket binding lives in `tuile-server`/clients (M2). The static HTTP
//! mode is this same in-process binding running over an HTTP fetcher.

use crate::content::TileContent;
use crate::source::TileId;
use crate::traversal::{TraversalStats, ViewState};
use futures_channel::mpsc;
use futures_core::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Consumer → geometry server.
#[derive(Debug, Clone)]
pub enum ClientMessage {
    /// Camera update; one [`ViewState`] per view (stereo = two).
    /// Send one per frame: the server coalesces bursts, only the latest
    /// matters.
    ViewerState { views: Vec<ViewState> },
    /// The consumer materialized this tile (GPU upload done).
    Ack { tile: TileId },
    /// The consumer no longer wants this in-flight tile.
    Cancel { tile: TileId },
}

/// Geometry server → consumer.
#[derive(Debug, Clone)]
pub enum ServerMessage {
    /// Full selection for the current viewer state (tile + current SSE),
    /// plus traversal stats for overlays.
    Select {
        tiles: Vec<(TileId, f64)>,
        stats: TraversalStats,
    },
    /// Content for a tile. Always `Decoded` on the consumer side.
    Content { tile: TileId, content: TileContent },
    /// A **stand-in** surface for a tile the consumer has not got yet.
    ///
    /// Its own variant rather than a flag on [`ServerMessage::Content`], for two
    /// reasons. A consumer must be able to *refuse* it — a stand-in racing the
    /// real content it was covering for would otherwise overwrite it, since both
    /// are filed under the same tile — and a variant makes that decision
    /// unavoidable at the match rather than optional at a boolean. And a backend
    /// that would rather draw an ancestor can ignore the message entirely and
    /// lose nothing but sharpness.
    ///
    /// Occupies exactly the tile's own rectangle, which is the point: the
    /// alternative, drawing the nearest ready ancestor, also covers the siblings
    /// that did arrive, and two surfaces over one patch of ground fight in the
    /// depth buffer.
    Fill {
        tile: TileId,
        content: crate::content::DecodedTileContent,
    },
    /// These tiles left residency; release their resources.
    Evict { tiles: Vec<TileId> },
    /// Non-fatal failure (a tile failed to fetch or decode).
    Error {
        tile: Option<TileId>,
        message: String,
    },
    /// How the coarse pyramid is going. Sent whenever the count changes, so a
    /// host holding its first frame back learns the shape of the wait on the
    /// session's first pass and every change after it.
    Priming(Priming),
}

/// The state of the coarse pyramid a session primes at startup.
///
/// A host that holds its first frame back until the pyramid is on the GPU needs
/// a condition it can actually **reach**, and "every primed tile is on the GPU"
/// is not one: a source does not serve every tile of a global grid — there is no
/// terrain over most of the ocean at coarse levels — and a tile that will never
/// be served can never be uploaded. Waiting for it is waiting for ever, which is
/// exactly what a viewer did: 618 of 682 tiles held, an empty queue, and a window
/// that never opened.
///
/// So a primed tile ends in one of two states and the server reports both.
/// [`Priming::expected`] is the number the consumer can actually hold, and
/// [`Priming::settled`] says the answer will not change again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Priming {
    /// Tiles the session asked for at the pinned levels.
    pub total: u32,
    /// Of those, how many the session has given up on: the source has no such
    /// tile, the load failed, or the content turned out to be topology rather
    /// than geometry. **Resolved, not pending** — none of them will ever arrive.
    pub unavailable: u32,
    /// Still to be asked for, or asked for and not yet answered. Falls to zero;
    /// nothing else is a bound on the wait.
    pub outstanding: u32,
}

impl Priming {
    /// Whether every primed tile has been answered one way or the other.
    pub fn settled(&self) -> bool {
        self.outstanding == 0
    }

    /// How many of the primed tiles a consumer can ever hold.
    pub fn expected(&self) -> u32 {
        self.total.saturating_sub(self.unavailable)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("geometry stream closed")]
    Closed,
}

/// THE seam of the project: the streaming protocol, independent of any
/// transport. A renderer written against this trait runs unchanged over the
/// in-process binding, the WebSocket binding or the static-HTTP pull.
pub trait GeometryStream: Send {
    /// Non-blocking send (called from render loops — never awaits).
    fn send(&self, msg: ClientMessage) -> Result<(), StreamError>;

    /// Poll the next server message. `Poll::Ready(None)` means the server
    /// side is gone.
    fn poll_message(&mut self, cx: &mut Context<'_>) -> Poll<Option<ServerMessage>>;

    /// Await the next server message.
    fn next_message(&mut self) -> NextMessage<'_, Self>
    where
        Self: Sized,
    {
        NextMessage { stream: self }
    }
}

/// Future returned by [`GeometryStream::next_message`].
pub struct NextMessage<'a, S: GeometryStream> {
    stream: &'a mut S,
}

impl<S: GeometryStream + Unpin> std::future::Future for NextMessage<'_, S> {
    type Output = Option<ServerMessage>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.stream.poll_message(cx)
    }
}

/// The in-process binding: the logical, fully decoded geometry server.
/// Messages cross a channel pair, nothing is serialized.
#[derive(Debug)]
pub struct InProcessStream {
    pub(crate) tx: mpsc::UnboundedSender<ClientMessage>,
    pub(crate) rx: mpsc::UnboundedReceiver<ServerMessage>,
}

impl GeometryStream for InProcessStream {
    fn send(&self, msg: ClientMessage) -> Result<(), StreamError> {
        self.tx.unbounded_send(msg).map_err(|_| StreamError::Closed)
    }

    fn poll_message(&mut self, cx: &mut Context<'_>) -> Poll<Option<ServerMessage>> {
        Pin::new(&mut self.rx).poll_next(cx)
    }
}

/// Channel pair used by `runtime::GeometryServer` to talk to one consumer.
pub(crate) struct ServerEndpoint {
    pub rx: mpsc::UnboundedReceiver<ClientMessage>,
    pub tx: mpsc::UnboundedSender<ServerMessage>,
}

/// Creates a connected (consumer, server) endpoint pair.
pub(crate) fn in_process_pair() -> (InProcessStream, ServerEndpoint) {
    let (client_tx, client_rx) = mpsc::unbounded();
    let (server_tx, server_rx) = mpsc::unbounded();
    (
        InProcessStream {
            tx: client_tx,
            rx: server_rx,
        },
        ServerEndpoint {
            rx: client_rx,
            tx: server_tx,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;

    #[test]
    fn in_process_round_trip_and_close() {
        let (mut client, mut server) = in_process_pair();
        client
            .send(ClientMessage::Ack { tile: TileId(7) })
            .expect("send");
        let got = server.rx.try_recv().expect("recv");
        assert!(matches!(got, ClientMessage::Ack { tile: TileId(7) }));

        server
            .tx
            .unbounded_send(ServerMessage::Evict {
                tiles: vec![TileId(7)],
            })
            .expect("send");
        let msg = client.next_message().now_or_never().expect("ready");
        assert!(matches!(msg, Some(ServerMessage::Evict { .. })));

        drop(server);
        assert!(client
            .next_message()
            .now_or_never()
            .expect("ready")
            .is_none());
        assert!(client
            .send(ClientMessage::Cancel { tile: TileId(1) })
            .is_err());
    }
}
