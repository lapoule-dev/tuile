// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//! What an application embedding the viewer may do with each frame before it
//! is traversed and drawn.
//!
//! [`ViewerHost`](crate::embed::ViewerHost) says where a session's *data*
//! comes from, once, before the window exists. This says what happens *every
//! frame*, and it is the other half: a host that owns the camera — a replay, a
//! simulator, a scripted flight — needs to put the eye exactly where its own
//! model says, and needs the globe to stream for that eye rather than for the
//! one a person last dragged.
//!
//! # Driving versus placing
//!
//! The viewer's [`CameraController`](tuile_camera::CameraController) exists to
//! make a person's gestures feel right: it eases toward what the gesture asked
//! for, and it holds the eye above the relief
//! ([`min_altitude`](tuile_camera::CameraController::min_altitude)) so a drag
//! cannot push it inside a ridge. Both are wrong for a host that has already
//! decided where the eye goes:
//!
//! * easing means the drawn pose is not the asked-for pose but somewhere
//!   behind it, by an amount that depends on the frame time;
//! * the floor means a pose the host computed against *its* ground model is
//!   silently raised by the viewer's.
//!
//! So there are two doors, and the difference is the whole contract:
//!
//! * [`HostCamera::drive`] — this frame is traversed and drawn from exactly
//!   this pose. No easing, no clamp, no input. What the host asks for is what
//!   the eye sees, bit for bit.
//! * [`HostCamera::place`] — move the controller's camera there and let it
//!   behave normally from that point: a person can carry on dragging, the
//!   easing and the floor apply again.
//!
//! [`HostCamera::release`] hands a driven camera back to the controller,
//! continuing from the last driven pose rather than jumping back to wherever
//! the controller had been left.
//!
//! # Preload views
//!
//! A host that is about to move the camera somewhere knows it before the
//! viewer does. [`HostFrame::preload`] takes poses that are **traversed but
//! never drawn**: the tiles they need are asked for now, so the move does not
//! arrive on an empty globe. They go through the same eyes/union mechanism a
//! stereo pair uses, which is why they cost traversal and not a second frame.
//!
//! # Overlay layers
//!
//! [`HostFrame::overlays`] takes the shapes the host adds to the scene,
//! grouped into **layers** it names. A layer is set and then left alone: what
//! was set last is drawn every frame until it is set again or cleared. That
//! is the opposite of [`HostFrame::preload`], which is wanted only for the
//! frame it is asked for, and it is deliberate — a host whose HUD changes
//! once a film frame should not rebuild it sixty times a second to keep it on
//! screen.
//!
//! # The ground under it
//!
//! [`HostFrame::ground_at`] answers how high the relief is at a place, from
//! the viewer's own terrain — the surface its camera controller keeps a floor
//! against. A host that puts something *on* the ground needs this, and needs
//! it from here rather than from a model of its own: two ideas of where the
//! ground is show up as geometry that floats or sinks into it.
//!
//! It is read fresh every frame on purpose. Terrain streams, so the answer is
//! `None` over ground that has not arrived and a whole region's summit over
//! ground that has arrived only coarsely, and it refines as tiles land. A
//! host that reads it once has placed its shapes against whatever happened to
//! be resident that frame.
//!
//! # A fixed drawing aspect
//!
//! [`HostCamera::set_aspect`] holds the picture to a shape of the host's
//! choosing, letterboxed inside whatever the window happens to be. A host
//! whose overlays were laid out for a film's viewport needs this: a HUD laid
//! out for 16:9 and drawn into a tall window is not the film's HUD. Outside
//! the picture is a plain clear; inside it the ground is drawn in full.
//!
//! # One contract, two shells
//!
//! The same trait is handed to the native embedding and to the web start, so a
//! host is written once. On the web it is **not** required to be `Send` or
//! `Sync`: a web host holds UI signals that are neither, and the page's event
//! loop is single-threaded anyway.

use tuile_camera::GlobeCamera;
use tuile_overlay::OverlayMesh;

/// A camera pose: ECEF metres, a unit direction and up, and a vertical field
/// of view in radians — all `f64`.
///
/// This is the viewer's own camera type rather than a new one, so a pose
/// handed in is the pose the renderer uses with no conversion to go wrong.
pub type Pose = GlobeCamera;

/// Whether the host dealt with an event, or the viewer should.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consumed {
    /// The host handled it. The viewer's own controls never see it.
    Yes,
    /// Not the host's; the viewer handles it as it always has.
    No,
}

impl Consumed {
    /// `Yes` when `consumed`, so a host can answer with a predicate.
    pub fn when(consumed: bool) -> Self {
        if consumed {
            Self::Yes
        } else {
            Self::No
        }
    }

    pub fn is_consumed(self) -> bool {
        self == Self::Yes
    }
}

/// How soon a preload view's tiles are wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Wanted as much as what is on screen: the camera is about to be there.
    Now,
    /// Wanted, but behind the drawn view.
    Later,
}

/// A pose traversed but never drawn, so its tiles are on hand before the
/// camera arrives.
#[derive(Debug, Clone, Copy)]
pub struct PreloadView {
    pub pose: Pose,
    /// Width over height of the view this pose will be drawn at. The drawn
    /// viewport's aspect when the host has no better answer.
    pub aspect: f64,
    pub priority: Priority,
}

impl PreloadView {
    /// A view wanted as soon as possible, at `aspect`.
    pub fn now(pose: Pose, aspect: f64) -> Self {
        Self {
            pose,
            aspect,
            priority: Priority::Now,
        }
    }
}

/// The camera, for the frame being prepared.
///
/// Reading is always allowed. Writing chooses between the two doors described
/// at the module level.
#[derive(Debug)]
pub struct HostCamera {
    /// The pose the last frame was drawn from.
    drawn: Pose,
    /// What the host asked for this frame, and which door it used.
    asked: Option<Asked>,
    /// The shape the picture is held to, letterboxed: see
    /// [`HostCamera::set_aspect`]. `None` is the window's own.
    aspect: Option<f64>,
    /// Whether the host said anything about it this frame.
    aspect_set: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Asked {
    /// Exactly this pose, no clamp and no easing.
    Drive(Pose),
    /// The controller's camera moves here and carries on normally.
    Place(Pose),
    /// The controller takes over from the last driven pose.
    Release,
}

impl HostCamera {
    /// For the frame about to be prepared, drawn last from `drawn` at
    /// `aspect` — the fixed shape in force, if any.
    pub(crate) fn new(drawn: Pose, aspect: Option<f64>) -> Self {
        Self {
            drawn,
            asked: None,
            aspect,
            aspect_set: false,
        }
    }

    /// The camera the last frame was drawn from.
    ///
    /// Not what this frame will use: a host that has already called
    /// [`Self::drive`] this frame still reads the previous pose here, because
    /// this frame has not been drawn yet.
    pub fn get(&self) -> Pose {
        self.drawn
    }

    /// This frame is traversed and drawn from exactly `pose`.
    ///
    /// No easing, no minimum altitude, and no gesture the viewer may have
    /// received. The next call to [`Self::get`] returns it, because by then it
    /// is what was drawn.
    pub fn drive(&mut self, pose: Pose) {
        self.asked = Some(Asked::Drive(pose));
    }

    /// Moves the controller's camera to `pose` and lets it behave normally
    /// from there: easing, the altitude floor and a person's gestures all
    /// apply again.
    pub fn place(&mut self, pose: Pose) {
        self.asked = Some(Asked::Place(pose));
    }

    /// Gives the camera back to the controller, continuing from the last
    /// driven pose.
    pub fn release(&mut self) {
        self.asked = Some(Asked::Release);
    }

    /// Holds the drawn picture to a fixed width over height, letterboxed
    /// inside the frame: `None` gives the window's own shape back.
    ///
    /// Everything of the scene is drawn inside the picture — the ground in
    /// full, every overlay through the same camera — and outside it is a
    /// plain clear. So a host whose overlays were laid out for a film's
    /// viewport sees them laid out as the film lays them out, whatever shape
    /// the window is.
    ///
    /// It survives the frame: the shape stays until it is set again. A value
    /// that is not finite and positive is refused, because an aspect reaches
    /// the projection matrix and a NaN there loses the frame.
    pub fn set_aspect(&mut self, aspect: Option<f64>) {
        match aspect {
            Some(a) if !a.is_finite() || a <= 0.0 => return,
            _ => {}
        }
        self.aspect = aspect;
        self.aspect_set = true;
    }

    /// The fixed shape in force, or `None` when the picture is the window's
    /// own shape.
    pub fn aspect(&self) -> Option<f64> {
        self.aspect
    }

    /// What the host asked for, for the shell to apply.
    pub(crate) fn taken(&self) -> Option<Asked> {
        self.asked
    }

    /// The shape in force after the host had its say, and whether it said
    /// anything: the shell keeps it for the next frame either way.
    pub(crate) fn taken_aspect(&self) -> (Option<f64>, bool) {
        (self.aspect, self.aspect_set)
    }
}

/// Where the picture goes inside a frame held to a fixed shape.
///
/// A plain rectangle in device pixels, from the frame's top-left. The bars
/// around it are cleared and nothing else is drawn in them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Letterbox {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Letterbox {
    /// The largest rectangle of `aspect` that fits a frame of `viewport`,
    /// centred in it. `None` for no fixed shape at all: the frame itself.
    ///
    /// A frame too small to hold a pixel of the shape still gets one pixel:
    /// a viewport of zero width is a render pass wgpu refuses, and a window
    /// being dragged to nothing is not an error.
    pub fn of(viewport: (u32, u32), aspect: Option<f64>) -> Self {
        let (w, h) = (viewport.0.max(1), viewport.1.max(1));
        let whole = Self {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        let Some(aspect) = aspect else { return whole };
        if !aspect.is_finite() || aspect <= 0.0 {
            return whole;
        }
        let frame = f64::from(w) / f64::from(h);
        if aspect > frame {
            // Wider than the frame: bars above and below.
            let height = ((f64::from(w) / aspect).round() as u32).clamp(1, h);
            Self {
                x: 0,
                y: (h - height) / 2,
                width: w,
                height,
            }
        } else {
            let width = ((f64::from(h) * aspect).round() as u32).clamp(1, w);
            Self {
                x: (w - width) / 2,
                y: 0,
                width,
                height: h,
            }
        }
    }

    /// Width over height of the picture itself, never dividing by zero.
    pub fn aspect(&self) -> f64 {
        f64::from(self.width.max(1)) / f64::from(self.height.max(1))
    }
}

/// The shapes a host adds to the scene, by layer.
///
/// A **layer** is a name the host chooses — `"flight"`, `"paths"`, `"hud"` —
/// and a set of meshes. A layer that is set is drawn every frame until it is
/// set again or cleared; a layer the host does not touch this frame keeps
/// what it had. A host whose HUD changes once a film frame therefore builds
/// it once, not sixty times a second.
///
/// Layers are drawn in the order they were first set, and within a layer in
/// the order the meshes were given. Across the whole frame, every opaque mesh
/// goes before every translucent one, as `tuile_overlay::OverlayMesh` says.
#[derive(Debug, Default)]
pub struct OverlayLayers {
    /// By name, in the order the names first arrived.
    layers: Vec<(String, Vec<OverlayMesh>)>,
    /// Whether anything changed since the renderer last read it.
    changed: bool,
}

impl OverlayLayers {
    /// Replaces `layer` with `meshes`. An empty `meshes` is a layer that
    /// draws nothing, which is not the same as one that was never set — but
    /// it looks the same, so prefer [`Self::clear`] when it is done with.
    pub fn set(&mut self, layer: &str, meshes: Vec<OverlayMesh>) {
        self.changed = true;
        match self.layers.iter_mut().find(|(name, _)| name == layer) {
            Some((_, held)) => *held = meshes,
            None => self.layers.push((layer.to_owned(), meshes)),
        }
    }

    /// Forgets `layer` entirely. Clearing a layer that was never set does
    /// nothing, so a host may clear unconditionally.
    pub fn clear(&mut self, layer: &str) {
        let before = self.layers.len();
        self.layers.retain(|(name, _)| name != layer);
        self.changed |= self.layers.len() != before;
    }

    /// Forgets every layer.
    pub fn clear_all(&mut self) {
        self.changed |= !self.layers.is_empty();
        self.layers.clear();
    }

    /// What `layer` holds, or `None` when it was never set.
    pub fn get(&self, layer: &str) -> Option<&[OverlayMesh]> {
        self.layers
            .iter()
            .find(|(name, _)| name == layer)
            .map(|(_, meshes)| meshes.as_slice())
    }

    /// Every layer's name, in the order they were first set.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.layers.iter().map(|(name, _)| name.as_str())
    }

    /// Whether anything has been set at all.
    pub fn is_empty(&self) -> bool {
        self.layers.iter().all(|(_, meshes)| meshes.is_empty())
    }

    /// Whether a host changed anything since the renderer last took it.
    pub(crate) fn changed(&self) -> bool {
        self.changed
    }

    /// Every mesh of every layer, in drawing order, and the change mark
    /// cleared: the renderer has it now.
    ///
    /// The new set is uploaded **before** the old one is let go, which is the
    /// renderer's job and the reason this hands over a borrow rather than
    /// draining: a frame between the two would have no overlays at all.
    pub(crate) fn taken(&mut self) -> Vec<OverlayMesh> {
        self.changed = false;
        self.layers
            .iter()
            .flat_map(|(_, meshes)| meshes.iter().cloned())
            .collect()
    }
}

/// What a host may read and change about the frame being prepared.
pub struct HostFrame<'a> {
    pub camera: &'a mut HostCamera,
    /// Poses to traverse without drawing. Replaced wholesale each frame the
    /// host touches it: what is not asked for again is no longer wanted.
    pub preload: &'a mut Vec<PreloadView>,
    /// The shapes the host adds to the scene, by layer. Unlike
    /// [`Self::preload`], a layer is **kept** until it is set again or
    /// cleared.
    pub overlays: &'a mut OverlayLayers,
    /// The drawn frame's size in device pixels — the whole frame, bars
    /// included when the camera is held to a fixed aspect.
    pub viewport: (u32, u32),
    /// The relief the viewer itself stands on, read through
    /// [`Self::ground_at`].
    ///
    /// `None` before a session has come up, or for a scene with no terrain.
    pub(crate) ground: Option<&'a dyn tuile_core::ground::GroundHeight>,
}

impl HostFrame<'_> {
    /// How high the ground is at `lon`/`lat` (radians), metres above the
    /// ellipsoid, or `None` where nothing is known there **yet**.
    ///
    /// The viewer's own relief: what the camera controller keeps its floor
    /// against, recorded tile by tile as the terrain is decoded — the
    /// quantized mesh states its own highest point, so this is a tile's
    /// maximum at the finest level resident, not a sampled surface. It
    /// refines as the ground arrives and is coarse, or absent, until it
    /// does.
    ///
    /// So a host that places anything against it must expect the answer to
    /// change, and must not read it once and keep the answer: over ground
    /// that has not loaded it is `None`, over ground that has only loaded
    /// coarsely it is a whole region's summit. A host that needs the relief
    /// settled should ask again as tiles land, which is what the viewer's own
    /// controller does with it.
    pub fn ground_at(&self, lon: f64, lat: f64) -> Option<f64> {
        self.ground?.height_at(lon, lat)
    }

    /// Whether the viewer has any relief at all to answer with — a scene with
    /// terrain, in a session that has come up.
    ///
    /// Not whether it knows a given place: that is `ground_at` returning
    /// `None`, and the two are different questions. A host that waits for
    /// terrain it will never get wants this one.
    pub fn has_ground(&self) -> bool {
        self.ground.is_some()
    }
    /// Width over height of the **picture**: the fixed shape the host asked
    /// for with [`HostCamera::set_aspect`], or the frame's own.
    ///
    /// This is what a preload view wants, and what a host laying out its own
    /// overlays should measure against — not the window, which may be
    /// letterboxed around it.
    pub fn aspect(&self) -> f64 {
        self.picture().aspect()
    }

    /// Where the picture sits inside the frame. The whole frame unless the
    /// host held the camera to an aspect.
    pub fn picture(&self) -> Letterbox {
        Letterbox::of(self.viewport, self.camera.aspect())
    }
}

/// An application that owns what the viewer draws, frame by frame.
///
/// On the web this is required to be neither `Send` nor `Sync`: a page's host
/// holds UI signals that are neither, and its event loop is single-threaded.
pub trait FrameHost: 'static {
    /// Before the frame is traversed. `dt` is the seconds since the last one.
    fn before_frame(&mut self, dt: f64, frame: &mut HostFrame<'_>);

    /// Before the viewer's own controls see the event.
    ///
    /// Answering [`Consumed::Yes`] stops it there — the controller never
    /// receives it, so a host that drives the camera is not fighting a drag it
    /// cannot see. The default answers [`Consumed::No`]: a host that does not
    /// care about input does not have to say so.
    fn window_event(&mut self, _event: &HostEvent, _frame: &HostFrame<'_>) -> Consumed {
        Consumed::No
    }
}

/// An input, as the host sees it: what happened, not how the shell received
/// it.
///
/// Deliberately not winit's own type. A host written against this compiles for
/// the page and for a window alike, and neither shell's version leaks into it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HostEvent {
    /// A pointer moved to a position in device pixels, from the frame's
    /// top-left.
    PointerMoved { x: f64, y: f64 },
    /// A button went down or came up at that position.
    PointerButton {
        x: f64,
        y: f64,
        button: PointerButton,
        down: bool,
    },
    /// A wheel or a trackpad scrolled. Lines where the device reports lines,
    /// device pixels where it reports pixels — `lines` says which.
    Scrolled { dx: f64, dy: f64, lines: bool },
    /// A key went down or came up.
    Key { key: Key, down: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Primary,
    Secondary,
    Middle,
    Other(u16),
}

/// A key, named by what it means rather than where it sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Space,
    Left,
    Right,
    Up,
    Down,
    Escape,
    /// A printable character, lowercased.
    Char(char),
    /// Anything else, by the shell's own name for it.
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    fn pose(height: f64) -> Pose {
        GlobeCamera::look_at(
            DVec3::new(6_378_137.0 + height, 0.0, 0.0),
            DVec3::new(6_378_137.0, 0.0, 0.0),
            DVec3::Z,
            0.8,
        )
    }

    #[test]
    fn a_host_that_says_nothing_leaves_the_frame_alone() {
        let camera = HostCamera::new(pose(1000.0), None);
        assert!(
            camera.taken().is_none(),
            "nothing asked for is nothing applied"
        );
        assert_eq!(camera.get().position, pose(1000.0).position);
    }

    #[test]
    fn get_is_the_pose_last_drawn_not_the_one_just_asked_for() {
        // The distinction matters to a host that reads the camera to decide
        // where to put it: it must see where it *was*, or it reads back its
        // own answer and the feedback loop is on itself.
        let mut camera = HostCamera::new(pose(1000.0), None);
        camera.drive(pose(5000.0));
        assert_eq!(camera.get().position, pose(1000.0).position);
    }

    #[test]
    fn the_last_word_in_a_frame_wins() {
        // A host may change its mind within one `before_frame`; the frame can
        // only be drawn once, so the last call is what happens.
        let mut camera = HostCamera::new(pose(1000.0), None);
        camera.drive(pose(5000.0));
        camera.release();
        assert!(matches!(camera.taken(), Some(Asked::Release)));
    }

    #[test]
    fn an_unhandled_event_goes_on_to_the_viewer() {
        struct Quiet;
        impl FrameHost for Quiet {
            fn before_frame(&mut self, _dt: f64, _frame: &mut HostFrame<'_>) {}
        }
        let mut host = Quiet;
        let mut camera = HostCamera::new(pose(1000.0), None);
        let mut preload = Vec::new();
        let mut overlays = OverlayLayers::default();
        let frame = HostFrame {
            camera: &mut camera,
            preload: &mut preload,
            overlays: &mut overlays,
            ground: None,
            viewport: (1920, 1080),
        };
        let seen = host.window_event(
            &HostEvent::Key {
                key: Key::Space,
                down: true,
            },
            &frame,
        );
        assert_eq!(seen, Consumed::No);
    }

    #[test]
    fn the_frame_s_aspect_is_its_pixels_and_never_divides_by_zero() {
        let mut camera = HostCamera::new(pose(1000.0), None);
        let mut preload = Vec::new();
        let mut overlays = OverlayLayers::default();
        let frame = HostFrame {
            camera: &mut camera,
            preload: &mut preload,
            overlays: &mut overlays,
            ground: None,
            viewport: (1920, 1080),
        };
        assert!((frame.aspect() - 16.0 / 9.0).abs() < 1e-12);

        let mut camera = HostCamera::new(pose(1000.0), None);
        let mut preload = Vec::new();
        // A window collapsed to nothing still has to answer something finite:
        // a NaN aspect reaches the projection matrix and loses the frame.
        let mut overlays = OverlayLayers::default();
        let frame = HostFrame {
            camera: &mut camera,
            preload: &mut preload,
            overlays: &mut overlays,
            ground: None,
            viewport: (0, 0),
        };
        assert!(frame.aspect().is_finite());
    }

    /// What the application does with each answer, written as the frame step
    /// does it. The frame step itself needs a GPU; this pins the decision it
    /// makes, which is the part that can be got wrong silently.
    #[test]
    fn each_door_reaches_the_controller_it_should() {
        use tuile_camera::CameraController;

        let floor = 100.0;
        let under = {
            let p = pose(10.0);
            GlobeCamera::look_at(p.position, DVec3::ZERO, DVec3::Z, p.fovy)
        };

        // Drive: the pose survives the floor and the frame's ease.
        let mut ctrl = CameraController::new(pose(50_000.0)).with_min_altitude(floor);
        ctrl.drive(under);
        ctrl.advance(1.0 / 60.0);
        assert_eq!(
            ctrl.camera.position, under.position,
            "drive must not be clamped"
        );
        assert!(ctrl.is_driven());

        // Place: the floor applies, and the host is no longer driving.
        let mut ctrl = CameraController::new(pose(50_000.0)).with_min_altitude(floor);
        ctrl.jump_to(under);
        assert!(!ctrl.is_driven(), "placing hands the camera back");
        assert_ne!(
            ctrl.camera.position, under.position,
            "place must be clamped"
        );

        // Release: driving stops, and the eye stays where it was driven.
        let mut ctrl = CameraController::new(pose(50_000.0)).with_min_altitude(floor);
        let high = pose(4_000.0);
        ctrl.drive(high);
        ctrl.release();
        assert!(!ctrl.is_driven());
        ctrl.advance(1.0 / 60.0);
        assert_eq!(
            ctrl.camera.position, high.position,
            "release must not move the eye"
        );
    }

    /// The layer contract, and the one thing that separates it from
    /// `preload`: a layer stays until it is spoken about again.
    #[test]
    fn a_layer_is_drawn_until_it_is_replaced() {
        let mut layers = OverlayLayers::default();
        layers.set("hud", vec![OverlayMesh::default()]);
        layers.set("paths", vec![OverlayMesh::default(); 3]);
        // A frame in which the host says nothing: both are still there.
        assert_eq!(layers.get("hud").map(<[_]>::len), Some(1));
        assert_eq!(layers.get("paths").map(<[_]>::len), Some(3));
        // Set again, and it is the new set, not both.
        layers.set("paths", vec![OverlayMesh::default()]);
        assert_eq!(layers.get("paths").map(<[_]>::len), Some(1));
        assert_eq!(layers.taken().len(), 2, "one of each, in one batch");
    }

    #[test]
    fn clearing_a_layer_forgets_it_and_leaves_the_others() {
        let mut layers = OverlayLayers::default();
        layers.set("hud", vec![OverlayMesh::default()]);
        layers.set("trail", vec![OverlayMesh::default()]);
        layers.clear("hud");
        assert!(layers.get("hud").is_none());
        assert_eq!(layers.get("trail").map(<[_]>::len), Some(1));
        // Clearing what was never set is not an error: a host switching view
        // clears unconditionally.
        layers.clear("mascot");
        assert_eq!(layers.names().count(), 1);
    }

    #[test]
    fn layers_are_drawn_in_the_order_they_were_first_set() {
        // The order matters for translucency, and "first set" rather than
        // "last set" so that re-setting a layer every frame does not shuffle
        // the frame under the host.
        let mut layers = OverlayLayers::default();
        layers.set("a", vec![OverlayMesh::default()]);
        layers.set("b", vec![OverlayMesh::default()]);
        layers.set("a", vec![OverlayMesh::default()]);
        assert_eq!(layers.names().collect::<Vec<_>>(), ["a", "b"]);
    }

    #[test]
    fn the_renderer_is_told_only_when_something_changed() {
        // A layer set once and left alone is drawn from the buffers it is
        // already in: that is what makes keeping a layer cheap.
        let mut layers = OverlayLayers::default();
        assert!(!layers.changed(), "nothing said is nothing to upload");
        layers.set("hud", vec![OverlayMesh::default()]);
        assert!(layers.changed());
        let _ = layers.taken();
        assert!(!layers.changed(), "taken once is taken");
        layers.clear("absent");
        assert!(!layers.changed(), "clearing nothing changes nothing");
        layers.clear("hud");
        assert!(layers.changed());
    }

    #[test]
    fn a_fixed_aspect_survives_the_frame_and_a_nonsense_one_is_refused() {
        let mut camera = HostCamera::new(pose(1000.0), None);
        assert_eq!(camera.aspect(), None, "the window's own shape by default");
        camera.set_aspect(Some(16.0 / 9.0));
        assert_eq!(camera.aspect(), Some(16.0 / 9.0));
        let (aspect, said) = camera.taken_aspect();
        assert_eq!((aspect, said), (Some(16.0 / 9.0), true));
        // The next frame starts with it in force, and a host that says
        // nothing keeps it.
        let camera = HostCamera::new(pose(1000.0), Some(16.0 / 9.0));
        assert_eq!(camera.taken_aspect(), (Some(16.0 / 9.0), false));
        // A NaN aspect reaches the projection matrix and loses the frame.
        let mut camera = HostCamera::new(pose(1000.0), Some(1.5));
        camera.set_aspect(Some(f64::NAN));
        camera.set_aspect(Some(0.0));
        camera.set_aspect(Some(-2.0));
        assert_eq!(camera.aspect(), Some(1.5), "nonsense never reaches it");
        // And `None` gives the window back.
        camera.set_aspect(None);
        assert_eq!(camera.aspect(), None);
    }

    #[test]
    fn the_letterbox_is_the_largest_picture_of_its_shape_centred() {
        // 16:9 in a square window: bars above and below.
        let box_ = Letterbox::of((1000, 1000), Some(16.0 / 9.0));
        assert_eq!(box_.width, 1000);
        assert_eq!(box_.height, 563);
        assert_eq!(box_.x, 0);
        assert_eq!(box_.y, (1000 - 563) / 2);
        assert!((box_.aspect() - 16.0 / 9.0).abs() < 0.01);

        // 16:9 in a wider window: bars left and right.
        let box_ = Letterbox::of((2000, 500), Some(16.0 / 9.0));
        assert_eq!(box_.height, 500);
        assert_eq!(box_.width, 889);
        assert_eq!(box_.y, 0);
        assert_eq!(box_.x, (2000 - 889) / 2);

        // No fixed shape: the frame itself, with nothing cropped.
        let box_ = Letterbox::of((1920, 1080), None);
        assert_eq!(
            (box_.x, box_.y, box_.width, box_.height),
            (0, 0, 1920, 1080)
        );
    }

    #[test]
    fn a_letterbox_is_never_zero_wide_whatever_it_is_asked() {
        // A render pass of zero width is one wgpu refuses, and a window being
        // dragged to nothing is not an error.
        for viewport in [(0, 0), (1, 1000), (1000, 1)] {
            for aspect in [None, Some(16.0 / 9.0), Some(0.01), Some(100.0)] {
                let box_ = Letterbox::of(viewport, aspect);
                assert!(
                    box_.width >= 1 && box_.height >= 1,
                    "{viewport:?} {aspect:?}"
                );
                assert!(box_.aspect().is_finite());
            }
        }
        // And a shape that is not a shape is the frame, not a panic.
        let box_ = Letterbox::of((1920, 1080), Some(f64::NAN));
        assert_eq!((box_.width, box_.height), (1920, 1080));
    }

    #[test]
    fn a_host_reads_the_viewer_s_own_relief_and_can_tell_it_has_none() {
        // A scene with terrain answers where it has loaded, `None` where it
        // has not — the distinction a host placing things on the ground has
        // to act on. A scene with no terrain at all answers nothing anywhere,
        // and says so separately, because waiting for ground that will never
        // arrive is a hang and not a load.
        struct Ridge;
        impl tuile_core::ground::GroundHeight for Ridge {
            fn height_at(&self, lon: f64, _lat: f64) -> Option<f64> {
                (lon > 0.0).then_some(1200.0)
            }
        }

        let mut camera = HostCamera::new(pose(1000.0), None);
        let mut preload = Vec::new();
        let mut overlays = OverlayLayers::default();
        let ridge = Ridge;
        let frame = HostFrame {
            camera: &mut camera,
            preload: &mut preload,
            overlays: &mut overlays,
            ground: Some(&ridge),
            viewport: (1920, 1080),
        };
        assert!(frame.has_ground());
        assert_eq!(
            frame.ground_at(0.5, 0.1),
            Some(1200.0),
            "where it has loaded"
        );
        assert_eq!(frame.ground_at(-0.5, 0.1), None, "where it has not");

        let mut camera = HostCamera::new(pose(1000.0), None);
        let mut preload = Vec::new();
        let mut overlays = OverlayLayers::default();
        let bare = HostFrame {
            camera: &mut camera,
            preload: &mut preload,
            overlays: &mut overlays,
            ground: None,
            viewport: (1920, 1080),
        };
        assert!(!bare.has_ground(), "a scene with no terrain");
        assert_eq!(bare.ground_at(0.5, 0.1), None);
    }

    #[test]
    fn a_frames_aspect_is_the_pictures_not_the_windows() {
        // What a host measures its overlays against: a HUD laid out for 16:9
        // in a tall window is laid out for 16:9, and the bars are outside it.
        let mut camera = HostCamera::new(pose(1000.0), Some(16.0 / 9.0));
        let mut preload = Vec::new();
        let mut overlays = OverlayLayers::default();
        let frame = HostFrame {
            camera: &mut camera,
            preload: &mut preload,
            overlays: &mut overlays,
            ground: None,
            viewport: (1000, 1000),
        };
        assert!(
            (frame.aspect() - 16.0 / 9.0).abs() < 0.01,
            "the picture's shape, not the window's: {}",
            frame.aspect()
        );
        assert_eq!(frame.picture().height, 563);
    }

    #[test]
    fn a_preload_view_takes_the_frame_s_shape_by_default() {
        let p = pose(2000.0);
        let view = PreloadView::now(p, 16.0 / 9.0);
        assert_eq!(view.priority, Priority::Now);
        assert_eq!(view.pose.position, p.position);
    }
}
