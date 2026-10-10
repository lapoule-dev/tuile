// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! "Centre on where I am": asking the operating system for the machine's
//! current location, and moving the view there when — and only when — it
//! answers.
//!
//! Three things shape this module.
//!
//! **The answer is late, or never.** A location service takes seconds, may
//! first have to ask the person for permission, and may be switched off,
//! refused, or simply not know. So nothing here waits: [`Locator::request`]
//! returns at once, and [`Locator::tick`] is called every frame and moves the
//! camera on the frame the answer is there. A request that gets no answer in
//! [`FIX_TIMEOUT`] is given up on with a message, and the view does not move.
//!
//! **Where the machine is, is private.** The coordinates go from the system to
//! the camera and nowhere else: never to a log, never to a file. [`Fix`] does
//! not even print them under `{:?}`. What is reported is whether a location
//! was obtained and how wide its uncertainty is.
//!
//! **The platform stays in its corner.** Everything the rest of the viewer
//! sees is the [`LocationSource`] trait and plain numbers. The one
//! implementation that talks to an operating system is the private `system`
//! module at the bottom, compiled on macOS with the `current-location`
//! feature; everywhere else the source answers "not available" and the viewer
//! runs as it always did. The tests drive the same [`Locator`] with a source
//! that is a script.

use std::time::{Duration, Instant};

use tuile_camera::CameraController;

use crate::start::StartView;

/// How long a request may go unanswered once the system is actually looking.
///
/// A fix from a warm service arrives in one or two seconds; a cold one, with
/// no recent position to hand out, can take several. Eight is past the slow
/// case and short of the point where a move would arrive as a surprise.
pub(crate) const FIX_TIMEOUT: Duration = Duration::from_secs(8);

/// How long the system's permission question may sit unanswered.
///
/// Counted separately because it is a person reading a dialog, not a radio
/// listening — and bounded all the same, because a process the system will
/// not show the dialog for stays "not yet asked" forever.
pub(crate) const PERMISSION_TIMEOUT: Duration = Duration::from_secs(60);

/// A location as the system gave it: degrees, and the radius in metres within
/// which the true position lies.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Fix {
    pub lon: f64,
    pub lat: f64,
    pub accuracy: f64,
}

impl std::fmt::Debug for Fix {
    /// The radius and nothing else. A derived `Debug` would put a home address
    /// in the first log line that mentions a `Fix`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fix")
            .field("accuracy", &self.accuracy)
            .finish_non_exhaustive()
    }
}

/// Where a request stands.
// Only the system's source ever says anything but "unavailable"; a build
// without it still has to name the other answers, because the locator and its
// tests are the same code everywhere.
#[cfg_attr(
    not(all(target_os = "macos", feature = "current-location")),
    allow(dead_code)
)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Progress {
    /// The system is looking.
    Waiting,
    /// The system is asking the person whether this program may know.
    AwaitingPermission,
    Fix(Fix),
    /// There will be no answer, and this is why — worded for the person.
    Unavailable(String),
}

/// Something that can be asked where the machine is. Never blocks.
pub(crate) trait LocationSource {
    /// Starts looking. Asking again while already looking starts over.
    fn request(&mut self);
    /// Where the request stands now. A `Fix` or an `Unavailable` is reported
    /// once; the request is over when either has been.
    fn poll(&mut self) -> Progress;
}

/// How the view is set once the location is known.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Placement {
    /// Height of the eye above the ellipsoid, metres; `None` keeps whatever
    /// the eye's altitude is on the frame the answer arrives.
    pub altitude: Option<f64>,
    /// Degrees clockwise from north.
    pub heading: f64,
    /// Degrees below the horizon.
    pub pitch: f64,
}

impl Placement {
    /// Straight down, north up, at the altitude the eye already has: the
    /// location lands in the middle of the screen at the present scale.
    pub(crate) const OVERHEAD: Self = Self {
        altitude: None,
        heading: 0.0,
        pitch: 90.0,
    };
}

/// How a request ended.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Outcome {
    /// The view is now over the location, known to within this many metres.
    Centred {
        accuracy: f64,
    },
    Unavailable(String),
    /// The system was looking and found nothing in [`FIX_TIMEOUT`].
    TimedOut,
    /// The permission question went unanswered for [`PERMISSION_TIMEOUT`].
    Unanswered,
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Centred { accuracy } => {
                write!(
                    f,
                    "centred on the current location (within {accuracy:.0} m)"
                )
            }
            Self::Unavailable(why) => write!(f, "current location unavailable: {why}"),
            Self::TimedOut => write!(
                f,
                "current location unavailable: no answer after {} s; the view was left alone",
                FIX_TIMEOUT.as_secs()
            ),
            Self::Unanswered => write!(
                f,
                "current location unavailable: the permission request was not answered"
            ),
        }
    }
}

/// One request in flight.
struct Pending {
    placement: Placement,
    /// When the current phase — permission, or the search itself — began.
    since: Instant,
    awaiting_permission: bool,
}

/// Carries a request from the key press to the frame its answer arrives on.
pub(crate) struct Locator {
    source: Box<dyn LocationSource>,
    pending: Option<Pending>,
}

impl Locator {
    pub(crate) fn new(source: Box<dyn LocationSource>) -> Self {
        Self {
            source,
            pending: None,
        }
    }

    /// The locator for this machine: the operating system's service where
    /// there is one this build can talk to, a polite refusal elsewhere.
    pub(crate) fn platform() -> Self {
        Self::new(system::source())
    }

    /// Starts looking. A second request while one is pending replaces it.
    pub(crate) fn request(&mut self, now: Instant, placement: Placement) {
        self.source.request();
        self.pending = Some(Pending {
            placement,
            since: now,
            awaiting_permission: false,
        });
    }

    /// Call once a frame. Moves the camera on the frame a location arrives and
    /// returns how the request ended; returns `None` while there is nothing to
    /// say, which is nearly always.
    pub(crate) fn tick(
        &mut self,
        now: Instant,
        controller: &mut CameraController,
    ) -> Option<Outcome> {
        let pending = self.pending.as_mut()?;
        let outcome = match self.source.poll() {
            Progress::AwaitingPermission => {
                if !pending.awaiting_permission {
                    pending.awaiting_permission = true;
                    pending.since = now;
                }
                (now.duration_since(pending.since) >= PERMISSION_TIMEOUT)
                    .then_some(Outcome::Unanswered)?
            }
            Progress::Waiting => {
                // The search starts when the permission question ends: the
                // seconds a person spent reading a dialog are not seconds the
                // system spent failing to find anything.
                if std::mem::take(&mut pending.awaiting_permission) {
                    pending.since = now;
                }
                (now.duration_since(pending.since) >= FIX_TIMEOUT).then_some(Outcome::TimedOut)?
            }
            Progress::Unavailable(why) => Outcome::Unavailable(why),
            Progress::Fix(fix) => match placed(fix, pending.placement, controller) {
                Some(view) => {
                    crate::steer::jump(controller, view.camera());
                    Outcome::Centred {
                        accuracy: fix.accuracy,
                    }
                }
                // A system that answers with a latitude of 400 has not
                // answered; the camera is not sent to a place that is not one.
                None => Outcome::Unavailable("the system returned no usable position".into()),
            },
        };
        self.pending = None;
        Some(outcome)
    }
}

/// The view a fix and a placement describe, or `None` when the fix is not a
/// point on the Earth.
fn placed(fix: Fix, placement: Placement, controller: &CameraController) -> Option<StartView> {
    let on_the_globe = fix.lon.is_finite()
        && fix.lat.is_finite()
        && (-180.0..=180.0).contains(&fix.lon)
        && (-90.0..=90.0).contains(&fix.lat);
    on_the_globe.then(|| StartView {
        lon: fix.lon,
        lat: fix.lat,
        altitude: placement
            .altitude
            .unwrap_or_else(|| controller.target().altitude()),
        heading: placement.heading,
        pitch: placement.pitch,
    })
}

/// The operating system's location service.
///
/// **Exercised by hand only.** No test in this crate calls the system: a test
/// that did would need a person to click a permission dialog, and would then
/// know where the build machine is. What is tested is everything on this side
/// of [`LocationSource`]; what this module does was checked by running it.
///
/// One request is one `requestLocation`: the system delivers a single fix or a
/// single failure to the delegate, on the main run loop — the one the window's
/// event loop already turns, which is why nothing here spawns a thread. The
/// delegate only writes the answer into a slot; [`LocationSource::poll`] reads
/// it on the next frame.
///
/// Authorisation is read back on every poll rather than through the delegate's
/// change callback: it is the same information, and it leaves the delegate
/// with exactly two methods.
#[cfg(all(target_os = "macos", feature = "current-location"))]
mod system {
    #![allow(unsafe_code)]

    use std::sync::{Arc, Mutex};

    use objc2::rc::Retained;
    use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
    use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
    use objc2_core_location::{
        CLAuthorizationStatus, CLLocation, CLLocationManager, CLLocationManagerDelegate,
    };
    use objc2_foundation::{NSArray, NSError};

    use super::{Fix, LocationSource, Progress};

    /// Where the delegate leaves the system's answer for the next poll.
    type Slot = Arc<Mutex<Option<Result<Fix, String>>>>;

    /// The system's "denied" error code in its location error domain.
    const DENIED: isize = 1;
    /// And its "could not find out right now" code.
    const UNKNOWN: isize = 0;

    const REFUSED: &str = "this program is not allowed to use the current location — \
        allow it in the system's privacy settings, under location services";

    define_class!(
        // SAFETY: `NSObject` has no subclassing requirements, and the class
        // does not implement `Drop`.
        #[unsafe(super(NSObject))]
        #[name = "TuileViewerLocationDelegate"]
        #[ivars = Slot]
        struct Delegate;

        unsafe impl NSObjectProtocol for Delegate {}

        unsafe impl CLLocationManagerDelegate for Delegate {
            #[unsafe(method(locationManager:didUpdateLocations:))]
            fn did_update(&self, _manager: &CLLocationManager, locations: &NSArray<CLLocation>) {
                // The array is oldest first; the last is the freshest.
                let Some(location) = locations.lastObject() else {
                    return;
                };
                // SAFETY: plain property reads on a valid, immutable object.
                let (coordinate, accuracy) =
                    unsafe { (location.coordinate(), location.horizontalAccuracy()) };
                // A negative radius is the system's way of saying the
                // coordinate is not valid.
                let answer = if accuracy < 0.0 {
                    Err("the system returned no usable position".to_owned())
                } else {
                    Ok(Fix {
                        lon: coordinate.longitude,
                        lat: coordinate.latitude,
                        accuracy,
                    })
                };
                self.leave(answer);
            }

            #[unsafe(method(locationManager:didFailWithError:))]
            fn did_fail(&self, _manager: &CLLocationManager, error: &NSError) {
                let why = match error.code() {
                    DENIED => REFUSED.to_owned(),
                    UNKNOWN => "the system could not determine a location".to_owned(),
                    _ => error.localizedDescription().to_string(),
                };
                self.leave(Err(why));
            }
        }
    );

    impl Delegate {
        fn new(slot: Slot) -> Retained<Self> {
            let this = Self::alloc().set_ivars(slot);
            // SAFETY: `NSObject`'s `init` is the designated initialiser.
            unsafe { msg_send![super(this), init] }
        }

        fn leave(&self, answer: Result<Fix, String>) {
            if let Ok(mut slot) = self.ivars().lock() {
                *slot = Some(answer);
            }
        }
    }

    struct System {
        manager: Retained<CLLocationManager>,
        /// The manager holds its delegate weakly; this is what keeps it alive.
        _delegate: Retained<Delegate>,
        slot: Slot,
        awaiting_permission: bool,
    }

    impl System {
        fn leave(&self, answer: Result<Fix, String>) {
            if let Ok(mut slot) = self.slot.lock() {
                *slot = Some(answer);
            }
        }

        fn look(&self) {
            // SAFETY: the manager is valid and has its delegate, which
            // implements both methods a one-shot request calls.
            unsafe { self.manager.requestLocation() };
        }
    }

    impl LocationSource for System {
        fn request(&mut self) {
            if let Ok(mut slot) = self.slot.lock() {
                *slot = None;
            }
            self.awaiting_permission = false;
            // SAFETY: a class property read and an instance property read.
            let (enabled, status) = unsafe {
                (
                    CLLocationManager::locationServicesEnabled_class(),
                    self.manager.authorizationStatus(),
                )
            };
            if !enabled {
                self.leave(Err("location services are switched off".to_owned()));
            } else if status == CLAuthorizationStatus::NotDetermined {
                self.awaiting_permission = true;
                // SAFETY: asks the system to show its permission dialog.
                unsafe { self.manager.requestWhenInUseAuthorization() };
            } else if status == CLAuthorizationStatus::Denied
                || status == CLAuthorizationStatus::Restricted
            {
                self.leave(Err(REFUSED.to_owned()));
            } else {
                self.look();
            }
        }

        fn poll(&mut self) -> Progress {
            if self.awaiting_permission {
                // SAFETY: an instance property read.
                let status = unsafe { self.manager.authorizationStatus() };
                if status == CLAuthorizationStatus::NotDetermined {
                    return Progress::AwaitingPermission;
                }
                self.awaiting_permission = false;
                if status == CLAuthorizationStatus::Denied
                    || status == CLAuthorizationStatus::Restricted
                {
                    return Progress::Unavailable(REFUSED.to_owned());
                }
                self.look();
                return Progress::Waiting;
            }
            match self.slot.lock().ok().and_then(|mut slot| slot.take()) {
                Some(Ok(fix)) => Progress::Fix(fix),
                Some(Err(why)) => Progress::Unavailable(why),
                None => Progress::Waiting,
            }
        }
    }

    /// Built on the main thread, before the event loop: the manager delivers
    /// to the run loop of the thread that created it.
    pub(super) fn source() -> Box<dyn LocationSource> {
        let slot = Slot::default();
        let delegate = Delegate::new(slot.clone());
        // SAFETY: `new` on a class with a plain `init`; the delegate outlives
        // the manager because both live in the same struct.
        let manager = unsafe {
            let manager = CLLocationManager::new();
            manager.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            manager
        };
        Box::new(System {
            manager,
            _delegate: delegate,
            slot,
            awaiting_permission: false,
        })
    }
}

/// Everywhere the system's service is not compiled in: the request is taken
/// and answered at once, so the flag and the key say why nothing happened.
#[cfg(not(all(target_os = "macos", feature = "current-location")))]
mod system {
    use super::{LocationSource, Progress};

    struct Absent;

    impl LocationSource for Absent {
        fn request(&mut self) {}

        fn poll(&mut self) -> Progress {
            Progress::Unavailable(
                if cfg!(target_os = "macos") {
                    "this build was made without the `current-location` feature"
                } else {
                    "not available on this platform"
                }
                .to_owned(),
            )
        }
    }

    pub(super) fn source() -> Box<dyn LocationSource> {
        Box::new(Absent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use tuile_core::geo::ecef_to_geodetic;

    /// A source that answers what it was told to, one poll at a time, and
    /// keeps answering the last thing once the script runs out.
    struct Scripted {
        script: VecDeque<Progress>,
        requests: Rc<RefCell<u32>>,
    }

    fn locator(script: &[Progress]) -> (Locator, Rc<RefCell<u32>>) {
        let requests = Rc::new(RefCell::new(0));
        let source = Scripted {
            script: script.iter().cloned().collect(),
            requests: requests.clone(),
        };
        (Locator::new(Box::new(source)), requests)
    }

    impl LocationSource for Scripted {
        fn request(&mut self) {
            *self.requests.borrow_mut() += 1;
        }

        fn poll(&mut self) -> Progress {
            if self.script.len() > 1 {
                self.script.pop_front().unwrap_or(Progress::Waiting)
            } else {
                self.script.front().cloned().unwrap_or(Progress::Waiting)
            }
        }
    }

    /// Nowhere in particular, and nowhere anyone lives.
    const SOMEWHERE: Fix = Fix {
        lon: -140.25,
        lat: -48.5,
        accuracy: 35.0,
    };

    fn controller() -> CameraController {
        CameraController::new(StartView::default().camera())
    }

    fn same_camera(a: &CameraController, b: &CameraController) -> bool {
        a.camera.position == b.camera.position
            && a.camera.direction == b.camera.direction
            && a.target().position == b.target().position
            && a.target().direction == b.target().direction
    }

    /// The answer arrives a few frames late: nothing moves until it does, and
    /// on that frame the eye is over the fix at the altitude asked for, with
    /// nothing left to ease toward. Tolerances as for the start flags: 1e-9°
    /// and a millimetre.
    #[test]
    fn a_location_that_arrives_centres_the_camera_there_at_the_altitude_asked() {
        let (mut locator, requests) = locator(&[
            Progress::Waiting,
            Progress::Waiting,
            Progress::Fix(SOMEWHERE),
        ]);
        let mut c = controller();
        let untouched = c.clone();
        let t0 = Instant::now();
        locator.request(
            t0,
            Placement {
                altitude: Some(3000.0),
                heading: 0.0,
                pitch: 90.0,
            },
        );
        assert_eq!(*requests.borrow(), 1);
        assert_eq!(locator.tick(t0, &mut c), None);
        assert_eq!(locator.tick(t0 + Duration::from_millis(16), &mut c), None);
        assert!(same_camera(&c, &untouched), "moved before the answer");

        let outcome = locator.tick(t0 + Duration::from_millis(32), &mut c);
        assert_eq!(outcome, Some(Outcome::Centred { accuracy: 35.0 }));
        for camera in [c.camera, *c.target()] {
            let eye = ecef_to_geodetic(camera.position);
            assert!((eye.lon.to_degrees() - SOMEWHERE.lon).abs() < 1e-9);
            assert!((eye.lat.to_degrees() - SOMEWHERE.lat).abs() < 1e-9);
            assert!((eye.height - 3000.0).abs() < 1e-3);
            assert!((camera.pitch().to_degrees() - 90.0).abs() < 1e-6);
        }
        // Over, and said once.
        assert_eq!(locator.tick(t0 + Duration::from_secs(1), &mut c), None);
    }

    /// The key's placement: the scale the person was looking at is kept.
    #[test]
    fn with_no_altitude_asked_the_present_one_is_kept() {
        let (mut locator, _) = locator(&[Progress::Fix(SOMEWHERE)]);
        let mut c = CameraController::new(
            StartView {
                altitude: 12_345.0,
                ..StartView::default()
            }
            .camera(),
        );
        let now = Instant::now();
        locator.request(now, Placement::OVERHEAD);
        assert!(matches!(
            locator.tick(now, &mut c),
            Some(Outcome::Centred { .. })
        ));
        assert!((c.camera.altitude() - 12_345.0).abs() < 1e-3);
        let eye = ecef_to_geodetic(c.camera.position);
        assert!((eye.lat.to_degrees() - SOMEWHERE.lat).abs() < 1e-9);
    }

    /// The system looks and finds nothing: after the timeout the request is
    /// given up on with a message, and the camera is exactly where it was.
    #[test]
    fn no_location_within_the_timeout_leaves_the_camera_alone_and_says_so() {
        let (mut locator, _) = locator(&[Progress::Waiting]);
        let mut c = controller();
        let untouched = c.clone();
        let t0 = Instant::now();
        locator.request(t0, Placement::OVERHEAD);

        let almost = t0 + FIX_TIMEOUT - Duration::from_millis(1);
        assert_eq!(locator.tick(almost, &mut c), None, "gave up early");
        let outcome = locator.tick(t0 + FIX_TIMEOUT, &mut c);
        assert_eq!(outcome, Some(Outcome::TimedOut));
        assert!(same_camera(&c, &untouched));
        let said = outcome.map(|o| o.to_string()).unwrap_or_default();
        assert!(
            said.contains("unavailable") && said.contains("left alone"),
            "{said:?}"
        );
        // A fix that turns up after the request was abandoned moves nothing.
        assert_eq!(locator.tick(t0 + FIX_TIMEOUT * 2, &mut c), None);
        assert!(same_camera(&c, &untouched));
    }

    /// Reading a permission dialog is not searching: the search's clock starts
    /// when the dialog is answered.
    #[test]
    fn time_spent_on_the_permission_question_is_not_charged_to_the_search() {
        let (mut locator, _) = locator(&[
            Progress::AwaitingPermission,
            Progress::AwaitingPermission,
            Progress::Waiting,
        ]);
        let mut c = controller();
        let t0 = Instant::now();
        locator.request(t0, Placement::OVERHEAD);
        assert_eq!(locator.tick(t0, &mut c), None);
        // Thirty seconds of dialog: well past the search's own timeout.
        let answered = t0 + Duration::from_secs(30);
        assert_eq!(locator.tick(answered, &mut c), None);
        assert_eq!(
            locator.tick(answered, &mut c),
            None,
            "the search just began"
        );
        assert_eq!(
            locator.tick(answered + FIX_TIMEOUT - Duration::from_millis(1), &mut c),
            None
        );
        assert_eq!(
            locator.tick(answered + FIX_TIMEOUT, &mut c),
            Some(Outcome::TimedOut)
        );
    }

    #[test]
    fn a_permission_question_nobody_answers_ends_too() {
        let (mut locator, _) = locator(&[Progress::AwaitingPermission]);
        let mut c = controller();
        let t0 = Instant::now();
        locator.request(t0, Placement::OVERHEAD);
        assert_eq!(locator.tick(t0, &mut c), None);
        assert_eq!(locator.tick(t0 + FIX_TIMEOUT * 2, &mut c), None);
        assert_eq!(
            locator.tick(t0 + PERMISSION_TIMEOUT, &mut c),
            Some(Outcome::Unanswered)
        );
    }

    #[test]
    fn a_refusal_is_passed_on_in_its_own_words_and_moves_nothing() {
        let (mut locator, _) = locator(&[Progress::Unavailable("switched off".into())]);
        let mut c = controller();
        let untouched = c.clone();
        let now = Instant::now();
        locator.request(now, Placement::OVERHEAD);
        let outcome = locator.tick(now, &mut c);
        assert_eq!(outcome, Some(Outcome::Unavailable("switched off".into())));
        assert!(same_camera(&c, &untouched));
    }

    /// A fix that is not a point on the Earth is a failure, not a destination.
    #[test]
    fn a_position_off_the_globe_is_refused() {
        for (lon, lat) in [(0.0, 400.0), (f64::NAN, 10.0), (181.0, 0.0)] {
            let (mut locator, _) = locator(&[Progress::Fix(Fix {
                lon,
                lat,
                accuracy: 10.0,
            })]);
            let mut c = controller();
            let untouched = c.clone();
            let now = Instant::now();
            locator.request(now, Placement::OVERHEAD);
            assert!(matches!(
                locator.tick(now, &mut c),
                Some(Outcome::Unavailable(_))
            ));
            assert!(same_camera(&c, &untouched));
        }
    }

    /// The coordinates must not reach a log through a stray `{:?}`.
    #[test]
    fn a_fix_does_not_print_where_it_is() {
        let printed = format!("{SOMEWHERE:?} {:?}", Progress::Fix(SOMEWHERE));
        assert!(
            !printed.contains("140.25") && !printed.contains("48.5"),
            "{printed}"
        );
        assert!(printed.contains("35"));
    }
}
