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
//! - Consumer-side `Content` is decoded ([`ResourceContent`]): a mesh or a
//!   drape, each a resource of its own; raw glb is what network bindings put
//!   on the wire (M2).
//!
//! Bindings: [`InProcessStream`] here (channels, zero serialization); the
//! WebSocket binding lives in `tuile-server`/clients (M2). The static HTTP
//! mode is this same in-process binding running over an HTTP fetcher.

use crate::budget::{
    Bytes, Class, DrawSet, Generation, PerClass, Pressure, ResourceId, Thermal, UploadPlan,
};
use crate::content::{DecodedTileContent, DrapeContent};
use crate::source::TileId;
use crate::traversal::{TraversalStats, ViewState};
use futures_channel::mpsc;
use futures_core::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// Consumer → geometry server.
///
/// Everything the graphics budget hears from the consumer and the platform
/// travels here, in order with the acknowledgements, so a session replays:
/// the views, what reached the GPU and with how many bytes, what could not,
/// and the platform's memory and thermal signals.
#[derive(Debug, Clone)]
pub enum ClientMessage {
    /// Camera update; one [`ViewState`] per view (stereo = two).
    /// Send one per frame: the server coalesces bursts, only the latest
    /// matters.
    ViewerState {
        views: Vec<ViewState>,
        /// Consumer-chosen, monotonically increasing. Echoed on every
        /// [`ServerMessage::Select`] produced from these views, so a consumer
        /// can tell WHICH camera a selection answers — the property a
        /// deterministic renderer needs, by construction rather than by
        /// heuristic. Interactive hosts may send 0 and never look.
        generation: u64,
    },
    /// The consumer uploaded `resource` of generation `gen`: it is on the
    /// GPU and drawable, and holds `bytes` there — the actual bytes, which
    /// replace the budget's estimate in its ledger.
    Ack {
        resource: ResourceId,
        gen: Generation,
        bytes: u64,
    },
    /// The consumer dropped a delivered resource without uploading it (an
    /// upload that failed, or that would have crossed its class's hard
    /// limit). It stays undrawn and is delivered again later.
    Discarded {
        resource: ResourceId,
        gen: Generation,
    },
    /// The platform's memory signal: a warning, a critical warning, the
    /// background, or back to normal.
    Pressure(Pressure),
    /// The platform's thermal state.
    Thermal(Thermal),
    /// The memory this session may use changed (the device's available
    /// memory, a browser's report, a remote consumer's declaration): every
    /// class's target, overflow and hard limit follow.
    Retarget { total_bytes: u64 },
    /// Batch, between frames: let go of GPU copies the last step did not need
    /// until the GPU holds at most `bytes`. Deterministic; announced with
    /// [`ServerMessage::Evict`].
    ReleaseBeyond { bytes: u64 },
    /// Batch: the views of the frames to come, in their order, known in
    /// advance. Their tiles are the need's prefetch ring: loaded with what
    /// the current views leave of the load slots, preempted by anything
    /// they wait on, never drawn, never awaited by a frame — and needed,
    /// so a [`ClientMessage::ReleaseBeyond`] keeps them. What a frame
    /// selects is the same with or without them; only when its tiles
    /// arrive changes. Replaces the views ahead given before; empty, none.
    Ahead { views: Vec<ViewState> },
}

/// What the tree knows about a tile, travelling with every message that names
/// one.
///
/// # Why this is on the wire at all
///
/// A [`TileId`] is an opaque handle: its payload means whatever the owning
/// [`TileTree`] says it means — a packed `(level, x, y)` for terrain, a flat
/// arena index for a 3D Tiles tileset. **The server has that tree; the consumer
/// does not, and never will.** That is the split the whole project is built on:
/// the core is renderer-agnostic and the consumer receives decoded content over
/// a channel that may be a WebSocket or a worker.
///
/// So a consumer cannot answer "how deep is this tile" or "what is its parent"
/// by looking at the handle. It tried anyway — by shifting the bits — and got 0
/// for every arena index.
///
/// [`TileTree`]: crate::source::TileTree
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ancestry {
    /// Depth in the tree, roots at 0. See [`crate::source::TileTree::level`].
    pub level: u32,
    /// The tile one level up, or `None` at a root.
    pub parent: Option<TileId>,
    /// Drawn, it stands for the ground under it — a mesh of the whole
    /// surface drawn in place of the globe's
    /// ([`TileTree::covers_ground`](crate::source::TileTree::covers_ground)):
    /// a consumer draws it before the ground, which keeps out of the pixels
    /// it takes.
    pub covers: bool,
}

/// The content of one resource: a tile's mesh, or a tile's drape (D-A2).
///
/// Shared rather than owned: the server keeps the decoded copy for as long as
/// its budget does (the decoded tier), so a resource evicted from the GPU and
/// wanted again is delivered again without a fetch or a decode.
#[derive(Debug, Clone)]
pub enum ResourceContent {
    /// A tile's geometry (and the textures it owns), never draped.
    Mesh(Arc<DecodedTileContent>),
    /// A tile's imagery of one layer at one level, its own resource: a tile's
    /// imagery changes without resending its mesh.
    Drape(Arc<DrapeContent>),
}

/// Geometry server → consumer.
///
/// The consumer is the budget's executor: it uploads what is delivered in
/// the plan's order within its frame's deadline, acknowledges each upload
/// with its bytes, draws the draw set and drops what is evicted after the
/// draw that stops using it.
#[derive(Debug, Clone)]
pub enum ServerMessage {
    /// One decision step: what to draw, in what order to upload, and the
    /// shape of the tree around it.
    Select {
        /// Every drawn mesh with its role and the drapes it wears, built
        /// from acknowledged resources only: drawable by construction. Apply
        /// it before the step's [`ServerMessage::Evict`] — new before old.
        draw: DrawSet,
        /// The uploads this step plans, in priority order, with their swap
        /// groups.
        plan: UploadPlan,
        /// The shape of the tree around this draw set: every tile it names
        /// and every ancestor of one, with what the tree says about it.
        ancestry: Vec<(TileId, Ancestry)>,
        stats: TraversalStats,
        /// Each class's effective hard limit, which the consumer never
        /// crosses: an upload that would is refused with
        /// [`ClientMessage::Discarded`].
        hard_limit: PerClass<Bytes>,
        /// Batch: every mesh and drape the frame needs is drawable, exactly.
        frame_ready: bool,
        /// Every wanted tile is drawn exactly, wearing the drapes it needs,
        /// with no work left for the view (I3).
        converged: bool,
        /// Wanted tiles the draw set covers with nothing (must be 0 once the
        /// floor is in, I1): ground left black.
        lost: u32,
        /// Drawn items that do not wear every layer they need (must be 0,
        /// I1 as D-A1 extends it): ground left bare.
        bare: u32,
        /// The [`ClientMessage::ViewerState::generation`] this answers.
        generation: u64,
    },
    /// A resource's decoded content, to upload. Carries its own
    /// [`Ancestry`] because not every resource delivered is drawn: the floor
    /// is loaded before anything stands on it.
    Content {
        resource: ResourceId,
        gen: Generation,
        /// The class it is charged to.
        class: Class,
        ancestry: Ancestry,
        content: ResourceContent,
    },
    /// Delivered, not yet uploaded: drop it (its generation, so a later
    /// delivery of the same resource is kept).
    Cancel {
        resources: Vec<(ResourceId, Generation)>,
    },
    /// These GPU copies leave: release them, after the draw of the step
    /// that announced them — which no longer uses them.
    Evict { resources: Vec<ResourceId> },
    /// A failure: a resource that could not be loaded (the budget retries it
    /// interactively), or the reason a batch session ends.
    Error {
        resource: Option<ResourceId>,
        message: String,
    },
    /// How the floor is going. Sent whenever the count changes, so a host
    /// holding its first frame back learns the shape of the wait on the
    /// session's first step and every change after it.
    Priming(Priming),
}

/// The state of the floor a session loads first: the pinned levels, mesh
/// and drape, that every fallback lands on.
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
        let mesh = ResourceId::Mesh(TileId(7));
        client
            .send(ClientMessage::Ack {
                resource: mesh,
                gen: Generation(3),
                bytes: 1024,
            })
            .expect("send");
        let got = server.rx.try_recv().expect("recv");
        assert!(matches!(
            got,
            ClientMessage::Ack {
                resource,
                gen: Generation(3),
                bytes: 1024,
            } if resource == mesh
        ));

        server
            .tx
            .unbounded_send(ServerMessage::Evict {
                resources: vec![mesh],
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
            .send(ClientMessage::Discarded {
                resource: mesh,
                gen: Generation(3),
            })
            .is_err());
    }
}
