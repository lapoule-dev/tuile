// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The engine as a film's host: its own bake and its own render, called by
//! [`tuile_farm::produce`], with the keys their parameters give.

use std::path::Path;
use std::sync::Arc;

use tuile_bake::tape::TapeBake;
use tuile_bake::tiles::Tiles;
use tuile_farm::produce::{Failed, Host, Stretch};
use tuile_repository::Objects;

use crate::render::{render, AbsentTiles, Order};
use crate::setup::{film_sink, placed_sun};
use crate::source::{Film, Sources};
use crate::Error;

/// How a film is rendered, beyond what its packs hold.
#[derive(Debug, Clone)]
pub struct Rendering {
    /// Everything of the picture: its scale against the bake's, its
    /// cadence, its tone, its look. Its `frames` are the production's.
    pub order: Order,
    /// The encoder, by name; `None` for the best this build has.
    pub codec: Option<String>,
    /// Bits a second.
    pub bitrate: u32,
    /// The sun's azimuth and elevation, in degrees, where the film begins.
    /// `None`: the look's own direction.
    pub sun: Option<(f64, f64)>,
}

/// How a film is to be rendered, as a caller writes it — the command line of
/// `tuile-film-produce`, or the body of a request to `tuile-film-serve`.
/// Colours are `r,g,b` linear; what is absent is the engine's own.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RenderSpec {
    pub fps: u32,
    pub scale: f32,
    pub supersample: u32,
    pub codec: String,
    pub mbps: f64,
    /// The sun's azimuth and elevation, in degrees, where the film begins.
    pub sun: Option<(f64, f64)>,
    pub exposure: Option<f32>,
    pub contrast: Option<f32>,
    pub saturation: Option<f32>,
    pub shadow: Option<f32>,
    pub haze_density: Option<f32>,
    pub haze_height: Option<f32>,
    pub haze_horizon: Option<String>,
    pub haze_zenith: Option<String>,
    pub sun_light: Option<String>,
    pub sky_light: Option<String>,
    pub no_tone: bool,
    /// How the ground is smoothed: `off`, `shading` (normals only) or
    /// `geometry` (triangles cut and their new vertices lifted onto the
    /// surface, by `smooth_bound` metres at most).
    pub smooth: String,
    pub smooth_bound: f32,
    pub smooth_strength: f32,
    /// Neighbouring tiles of different levels sewn together where they meet.
    pub stitch: bool,
    /// For two eyes, packed into one picture. Absent: one eye, as always.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stereo: Option<StereoSpec>,
    /// The physical atmosphere (`tuile_film::Look::atmosphere`), and its
    /// turbidity. Absent: the look's own light.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub atmosphere: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turbidity: Option<f32>,
    /// Each frame's instant, as a time-lapse states it (`tuile_tape::Clock`):
    /// `<start>,<seconds a frame>` (`Order::clock`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock: Option<String>,
    /// Towns lit at night (`Order::night_lights`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub night_lights: bool,
    /// The sun's and the Moon's discs drawn this many times their size,
    /// and the lens's glare (`Look::sun_scale`, `moon_scale`, `glare`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sun_scale: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moon_scale: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glare: Option<f32>,
}

/// A film for two eyes, as a caller writes it: see [`tuile_film::Stereo`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StereoSpec {
    /// `side-by-side` or `top-bottom`.
    pub packing: StereoPacking,
    /// Each eye squeezed into half the picture, for a 3D television; else
    /// the picture is doubled.
    pub half: bool,
    /// The parallax of the furthest ground, as a fraction of an eye's
    /// width: how deep the picture goes behind the screen.
    pub divergence: f64,
    /// The screen's distance from the eyes, in metres. Absent: where the
    /// camera's axis meets the ground, frame by frame.
    pub convergence_m: Option<f64>,
    /// The ground's height over the ellipsoid, in metres, where the screen
    /// is put on it.
    pub ground_m: f64,
    /// The eyes this many metres apart, in place of what the divergence
    /// gives: hyperstereo by the metre.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_m: Option<f64>,
}

/// How two eyes share a picture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StereoPacking {
    #[default]
    SideBySide,
    TopBottom,
}

impl Default for StereoSpec {
    fn default() -> Self {
        Self {
            packing: StereoPacking::SideBySide,
            half: false,
            divergence: tuile_film::DEFAULT_DIVERGENCE,
            convergence_m: None,
            ground_m: 0.0,
            baseline_m: None,
        }
    }
}

impl StereoSpec {
    /// The stereo it asks for, refused when it cannot be filmed.
    pub fn stereo(&self) -> Result<tuile_film::Stereo, Error> {
        let stereo = tuile_film::Stereo {
            packing: match self.packing {
                StereoPacking::SideBySide => tuile_film::Packing::SideBySide,
                StereoPacking::TopBottom => tuile_film::Packing::TopBottom,
            },
            half: self.half,
            divergence: self.divergence,
            baseline_m: self.baseline_m,
            convergence: match self.convergence_m {
                Some(metres) => tuile_film::Convergence::Distance(metres),
                None => tuile_film::Convergence::Ground {
                    height_m: self.ground_m,
                },
            },
        };
        stereo.check()?;
        Ok(stereo)
    }
}

impl Default for RenderSpec {
    fn default() -> Self {
        Self {
            fps: 30,
            scale: 1.0,
            supersample: 2,
            codec: "h264".into(),
            mbps: 12.0,
            sun: None,
            exposure: None,
            contrast: None,
            saturation: None,
            shadow: None,
            haze_density: None,
            haze_height: None,
            haze_horizon: None,
            haze_zenith: None,
            sun_light: None,
            sky_light: None,
            no_tone: false,
            stereo: None,
            smooth: "geometry".into(),
            smooth_bound: tuile_film::smoothing::DEFAULT_BOUND_M,
            smooth_strength: 1.0,
            stitch: true,
            atmosphere: false,
            turbidity: None,
            clock: None,
            night_lights: false,
            sun_scale: None,
            moon_scale: None,
            glare: None,
        }
    }
}

impl RenderSpec {
    /// The rendering it asks for. A colour that is not three values, none
    /// below zero, is refused.
    pub fn rendering(&self) -> Result<Rendering, Error> {
        use crate::setup::rgb;
        let mut order = Order {
            scale: self.scale,
            supersample: self.supersample.max(1),
            fps: self.fps.max(1),
            ..Default::default()
        };
        if self.no_tone {
            order.tone = crate::Tone::Off;
        }
        order.stereo = self.stereo.as_ref().map(StereoSpec::stereo).transpose()?;
        order.smooth = match self.smooth.as_str() {
            "off" => tuile_film::smoothing::Smooth::Off,
            "shading" => tuile_film::smoothing::Smooth::Shading,
            "geometry" => tuile_film::smoothing::Smooth::Geometry {
                bound: self.smooth_bound,
            },
            other => return Err(format!("smooth is off, shading or geometry, not {other}").into()),
        };
        order.smooth_strength = self.smooth_strength;
        order.stitch = self.stitch;
        order.clock = self
            .clock
            .as_deref()
            .map(tuile_tape::Clock::parse)
            .transpose()?;
        order.night_lights = self.night_lights;
        if let Some(scale) = self.sun_scale {
            order.look.sun_scale = scale;
        }
        if let Some(scale) = self.moon_scale {
            order.look.moon_scale = scale;
        }
        if let Some(glare) = self.glare {
            order.look.glare = glare;
        }
        if (self.atmosphere || self.turbidity.is_some())
            && order.clock.is_none()
            && self.sun.is_none()
        {
            return Err(
                "a film under a physical atmosphere is lit by its frames' instants: \
                        a tape that states them, --clock, or a sun placed by hand (--sun)"
                    .into(),
            );
        }
        if self.atmosphere || self.turbidity.is_some() {
            order.look.atmosphere = Some(tuile_film::AtmosphereParams::default());
            order.look.turbidity = self.turbidity;
        }
        let look = &mut order.look;
        if let Some(stops) = self.exposure {
            look.exposure_ev = stops;
        }
        if let Some(power) = self.contrast {
            look.contrast = power;
        }
        if let Some(factor) = self.saturation {
            look.saturation = factor;
        }
        if let Some(strength) = self.shadow {
            look.shadow = strength;
        }
        if let Some(colour) = &self.sun_light {
            look.sun = rgb("sun-light", colour)?;
        }
        if let Some(colour) = &self.sky_light {
            look.world = rgb("sky-light", colour)?;
        }
        // Naming anything of the air asks for the air.
        if self.haze_density.is_some()
            || self.haze_height.is_some()
            || self.haze_horizon.is_some()
            || self.haze_zenith.is_some()
        {
            let haze = look.haze.get_or_insert_with(tuile_film::Haze::default);
            if let Some(density) = self.haze_density {
                haze.density = density;
            }
            if let Some(height) = self.haze_height {
                haze.scale_height = height;
            }
            if let Some(colour) = &self.haze_horizon {
                haze.horizon = rgb("haze-horizon", colour)?;
            }
            if let Some(colour) = &self.haze_zenith {
                haze.zenith = rgb("haze-zenith", colour)?;
            }
        }
        Ok(Rendering {
            order,
            codec: Some(self.codec.clone()),
            bitrate: (self.mbps * 1e6) as u32,
            sun: self.sun,
        })
    }
}

/// The prefix of a film's keys: `films/<its packs' name>/<look>`, the look
/// naming everything a rendered picture depends on beside its pack — and
/// not which frames are rendered, so that every part of a film agrees.
pub fn film_prefix(packs: &str, rendering: &Rendering) -> String {
    let look = tuile_pack::scene_digest(&[canonical(rendering).as_bytes()]);
    format!(
        "films/{}/{look}",
        packs.trim_matches('/').trim_start_matches("packs/")
    )
}

/// Fields added to a rendering after films were named by it, each with the
/// text its default prints as. While a field holds its default it is left
/// out of the name, so a film made before it existed keeps its name — and
/// a production stopped then goes on from its parts.
#[cfg(test)]
const FROZEN_DEFAULT_LOOK: &str = "ff9e7af3dbfa95e5";

const LATER_FIELDS: [&str; 5] = [
    ", clock: None",
    ", night_lights: false",
    ", sun_scale: 1.0",
    ", moon_scale: 1.0",
    ", glare: 1.0",
];

/// The text a rendering is named by: everything but which frames are
/// rendered, the later fields at their defaults left out.
fn canonical(rendering: &Rendering) -> String {
    let mut rendering = rendering.clone();
    rendering.order.frames = None;
    let mut text = format!("{rendering:?}");
    for field in LATER_FIELDS {
        text = text.replacen(field, "", 1);
    }
    text
}

/// What owns a tokio runtime of its own — the shared tile store does, to
/// flush from a bake's thread — made and let go off the runtime a production
/// runs on: a runtime may be neither started nor dropped from within another.
pub struct OffRuntime<T: Send + 'static> {
    inner: Option<T>,
}

impl<T: Send + 'static> OffRuntime<T> {
    /// `open` run on a thread of the blocking pool.
    pub async fn open(
        open: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<Self, String> {
        let inner = tokio::task::spawn_blocking(open)
            .await
            .map_err(|e| e.to_string())??;
        Ok(Self { inner: Some(inner) })
    }

    pub fn get(&self) -> Option<&T> {
        self.inner.as_ref()
    }
}

impl<T: Send + 'static> Drop for OffRuntime<T> {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            // On a thread of its own, waited for: whatever it flushes on its
            // way out is done before this returns.
            let _ = std::thread::spawn(move || drop(inner)).join();
        }
    }
}

/// The engine, set for one film.
pub struct Engine {
    bake: TapeBake,
    /// The shared tile store a bake reads and fills, when there is one.
    tiles: OffRuntime<Option<Tiles>>,
    sources: Sources,
    rendering: Rendering,
    /// The names its parameters give, taken once.
    packs: String,
    film: String,
    /// The runtime a render's reads run on.
    runtime: tokio::runtime::Handle,
}

impl Engine {
    /// The engine for `bake` rendered as `rendering` says. `runs` holds the
    /// packs and `tiles` the tile store the render reads; `cache` is where
    /// chunks of both are kept while the film is made. `first` is the
    /// film's first frame: the sun is placed where its camera is.
    pub async fn open(
        bake: TapeBake,
        mut rendering: Rendering,
        first: u32,
        runs: Arc<dyn Objects>,
        tiles: Arc<dyn Objects>,
        cache: &Path,
    ) -> Result<Self, Error> {
        // One sun for the whole film: placed anew at each part's first
        // frame, it would step between two parts. Placed even when none is
        // asked for: the look's own is unoriented ([`placed_sun`]).
        let eye = first
            .checked_sub(1)
            .and_then(|i| bake.poses.get(i as usize))
            .map(|pose| pose.position);
        match (eye, rendering.sun) {
            (Some(eye), sun) => rendering.order.look.to_sun = placed_sun(eye, sun),
            (None, Some(_)) => {
                return Err(format!(
                    "frame {first} is not one of the tape's {}",
                    bake.poses.len()
                )
                .into())
            }
            (None, None) => {}
        }
        let packs = bake.packs();
        let film = film_prefix(&packs, &rendering);
        Ok(Self {
            tiles: OffRuntime::open(Tiles::from_env).await?,
            sources: Sources::open(runs, tiles, cache).await?,
            bake,
            rendering,
            packs,
            film,
            runtime: tokio::runtime::Handle::current(),
        })
    }

    fn tiles(&self) -> Option<&Tiles> {
        self.tiles.get().and_then(Option::as_ref)
    }

    async fn rendered(&self, frames: Stretch, pack: &str, out: &Path) -> Result<(), Error> {
        let film = Film::of(self.sources.packs.objects.as_ref(), &[pack]).await?;
        let mut order = self.rendering.order.clone();
        order.frames = Some((frames.first, frames.last));
        let mut sink = film_sink(
            out.to_string_lossy().into_owned(),
            self.rendering.bitrate,
            self.rendering.codec.as_deref(),
        )?;
        render(
            &self.sources,
            &film,
            &order,
            &mut (),
            sink.as_mut(),
            &mut (),
        )
        .await?;
        Ok(())
    }
}

impl Host for Engine {
    fn packs(&self) -> String {
        self.packs.clone()
    }

    fn film(&self) -> String {
        self.film.clone()
    }

    fn bake(&self, frames: Stretch, out: &Path) -> Result<(), Failed> {
        if self.bake.token.is_empty() && self.bake.needs_token() {
            return Err(Failed::for_good(
                "a pack is to be baked and TUILE_ION_TOKEN is not set",
            ));
        }
        let baked = self.bake.bake(frames.first, frames.last, out, self.tiles());
        // Whatever the bake fetched is published, even if it failed.
        let flushed = self.tiles().map(Tiles::flush).transpose();
        baked.map_err(Failed::once)?;
        flushed.map_err(Failed::once)?;
        Ok(())
    }

    fn render(&self, frames: Stretch, pack: &str, out: &Path) -> Result<(), Failed> {
        // Called on a thread of its own: the render's reads are waited for
        // here, on the runtime that made the engine.
        self.runtime
            .block_on(self.rendered(frames, pack, out))
            .map_err(|e| {
                match e.downcast_ref::<AbsentTiles>() {
                    // The same tiles would be absent on the next try.
                    Some(_) => Failed::for_good(e.to_string()),
                    None => Failed::once(e.to_string()),
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_for_two_eyes_reads_from_json_and_is_held_to_its_bounds() {
        // Mono unless asked: an order of before reads, and writes, as it did.
        let mono: RenderSpec = serde_json::from_str(r#"{"fps": 30}"#).expect("a spec");
        assert!(mono.rendering().expect("rendering").order.stereo.is_none());
        assert!(!serde_json::to_string(&mono)
            .expect("json")
            .contains("stereo"));

        let spec: RenderSpec = serde_json::from_str(
            r#"{"stereo": {"packing": "top-bottom", "half": true, "convergence_m": 900}}"#,
        )
        .expect("a stereo spec");
        let stereo = spec
            .rendering()
            .expect("rendering")
            .order
            .stereo
            .expect("two eyes");
        assert_eq!(stereo.packing, tuile_film::Packing::TopBottom);
        assert!(stereo.half);
        assert_eq!(stereo.divergence, tuile_film::DEFAULT_DIVERGENCE);
        assert_eq!(stereo.convergence, tuile_film::Convergence::Distance(900.0));
        // No distance: the ground, at the height given.
        let ground: RenderSpec =
            serde_json::from_str(r#"{"stereo": {"ground_m": 1200}}"#).expect("a spec");
        assert_eq!(
            ground
                .rendering()
                .expect("rendering")
                .order
                .stereo
                .map(|s| s.convergence),
            Some(tuile_film::Convergence::Ground { height_m: 1200.0 })
        );
        let hyper: RenderSpec =
            serde_json::from_str(r#"{"stereo": {"baseline_m": 10}}"#).expect("a spec");
        assert_eq!(
            hyper
                .rendering()
                .expect("rendering")
                .order
                .stereo
                .and_then(|s| s.baseline_m),
            Some(10.0)
        );
        assert!(
            serde_json::from_str::<RenderSpec>(r#"{"stereo": {"baseline_m": -3}}"#)
                .expect("parsed")
                .rendering()
                .is_err()
        );
        // Out of bounds, or a field nobody knows: refused.
        let wild: RenderSpec =
            serde_json::from_str(r#"{"stereo": {"divergence": 0.3}}"#).expect("parsed");
        assert!(wild.rendering().is_err());
        assert!(serde_json::from_str::<RenderSpec>(r#"{"stereo": {"ipd": 0.065}}"#).is_err());
        assert!(
            serde_json::from_str::<RenderSpec>(r#"{"stereo": {"packing": "anaglyph"}}"#).is_err()
        );
    }

    fn rendering() -> Rendering {
        Rendering {
            order: Order::default(),
            codec: None,
            bitrate: 12_000_000,
            sun: None,
        }
    }

    /// What holds a runtime of its own, as the shared tile store does: made
    /// and let go on the async runtime that drives a production, either one
    /// panics ("Cannot start a runtime from within a runtime").
    struct OwnRuntime(#[allow(dead_code, reason = "held for its drop")] tokio::runtime::Runtime);

    impl OwnRuntime {
        fn open() -> Result<Self, String> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .map_err(|e| e.to_string())?;
            runtime.block_on(async {});
            Ok(Self(runtime))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn what_owns_a_runtime_is_made_and_let_go_off_the_production_s_own() {
        let held = OffRuntime::open(OwnRuntime::open).await.expect("opened");
        assert!(held.get().is_some());
        drop(held);
        // Nothing to hold is nothing.
        let none = OffRuntime::<OwnRuntime>::open(|| Err("not configured".into())).await;
        assert!(none.is_err());
    }

    #[test]
    fn a_film_is_kept_under_its_packs_and_its_look_and_not_under_its_frames() {
        let film = film_prefix("packs/abc.references", &rendering());
        // The name of a default rendering, frozen: what it was before any
        // later field existed. A change here renames every film.
        assert!(!canonical(&rendering()).contains("night_lights"));
        assert_eq!(
            film,
            format!("films/abc.references/{}", FROZEN_DEFAULT_LOOK)
        );
        assert!(film.starts_with("films/abc.references/"), "{film}");
        assert_eq!(film_prefix("packs/abc.references/", &rendering()), film);
        // Whichever frames are rendered, it is the same film.
        let mut part = rendering();
        part.order.frames = Some((51, 100));
        assert_eq!(film_prefix("packs/abc.references", &part), film);

        // Anything of the picture is another film; another bake too.
        let mut others = vec![film_prefix("packs/abd.references", &rendering())];
        for change in [
            (|r: &mut Rendering| r.order.fps = 60) as fn(&mut Rendering),
            |r| r.order.scale = 0.5,
            |r| r.order.supersample = 1,
            |r| r.order.look.exposure_ev = 1.6,
            |r| r.order.look.shadow = 1.0,
            |r| r.order.look.haze = Some(tuile_film::Haze::default()),
            |r| r.order.tone = crate::Tone::Off,
            |r| r.codec = Some("h264".into()),
            |r| r.bitrate = 45_000_000,
            |r| r.sun = Some((255.0, 10.0)),
            |r| r.order.stereo = Some(tuile_film::Stereo::default()),
        ] {
            let mut changed = rendering();
            change(&mut changed);
            others.push(film_prefix("packs/abc.references", &changed));
        }
        others.push(film);
        let distinct: std::collections::HashSet<&String> = others.iter().collect();
        assert_eq!(distinct.len(), others.len(), "{others:?}");
    }
}
