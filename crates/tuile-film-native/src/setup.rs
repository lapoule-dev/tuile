// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a program that renders sets up before its first frame, the same for
//! every one of them: the stores as the environment names them, the film's
//! encoder, the sun where the film is.

use std::path::Path;
use std::sync::Arc;

use glam::{DVec3, Vec3};
use tuile_core::geo::{ecef_to_geodetic, enu_frame};
use tuile_farm::{BucketConfig, ObjectRunStore, Tuning};
use tuile_repository::Objects;

use crate::sink::{Av1Film, Sink};
use crate::Error;

/// The bucket `name`, at the endpoint and with the credentials of the
/// environment's `TUILE_STORE_*`.
pub fn bucket(name: &str) -> Result<Arc<dyn Objects>, Error> {
    let config = BucketConfig {
        bucket: name.to_string(),
        ..BucketConfig::from_env()?
    };
    Ok(Arc::new(ObjectRunStore::bucket(
        &config,
        Tuning::from_env(),
    )?))
}

/// The tile store's bucket `name`, behind the global tier when the
/// environment names one (`TUILE_TILES_GLOBAL`, as the bakes read it:
/// [`tuile_bake::tiles::Tiers`]): its archives, immutable, are read from
/// the region's copy, and what the bucket gives is copied there once for
/// every other instance. The catalog and the manifests always come from
/// the bucket.
pub fn tile_bucket(name: &str) -> Result<Arc<dyn Objects>, Error> {
    let config = BucketConfig {
        bucket: name.to_string(),
        ..BucketConfig::from_env()?
    };
    let store = ObjectRunStore::bucket(&config, Tuning::from_env())?;
    let tiers = tuile_bake::tiles::Tiers::from_env()?;
    Ok(Arc::new(match tiers.global {
        Some(global) => {
            println!("tile store: bucket {name}, its archives behind {}", global.name);
            store.wrapped(
                |origin| Arc::new(tuile_tile_server::Tiered::new(origin, vec![global])),
                "behind its global tier",
            )
        }
        None => store,
    }))
}

/// The tile store a render reads, and what keeps its blocks.
pub struct TileStore {
    pub objects: Arc<dyn Objects>,
    /// The blocks of a remote store, counted where they cross the network.
    pub remote_blocks: Option<Arc<tuile_repository::Kept<tuile_repository::HttpGet>>>,
    /// Their store, to be closed on the way out: what it has not written by
    /// then is lost, and downloaded again by the next render.
    pub block_store: Option<tuile_storage_foyer::FoyerStore>,
}

/// The tile store as the environment names it: a directory here
/// (`TUILE_TILES_LOCAL`), somebody else's server of
/// the store routes (`TUILE_TILES_REMOTE`, see
/// [`tuile_repository::HttpGet::from_env`]), its blocks kept under `cache`,
/// or the bucket `TUILE_TILES_BUCKET` ([`tile_bucket`]).
pub async fn tile_store(cache: &Path) -> Result<TileStore, Error> {
    // A store on this machine, for renders of packs that carry their tiles:
    // nothing is asked of a bucket.
    if let Ok(dir) = std::env::var("TUILE_TILES_LOCAL") {
        let dir = Path::new(&dir);
        std::fs::create_dir_all(dir)?;
        let catalog = dir.join("catalog.json");
        if !catalog.exists() {
            std::fs::write(&catalog, br#"{"layers":[]}"#)?;
        }
        return Ok(TileStore {
            objects: Arc::new(tuile_farm::ObjectRunStore::local(
                dir,
                tuile_farm::Tuning::from_env(),
            )?),
            remote_blocks: None,
            block_store: None,
        });
    }
    match tuile_repository::HttpGet::from_env()? {
        Some(get) => {
            println!("tile store: {}/store, by its routes", get.root());
            // A block is downloaded once: kept in a store of its own beside
            // the film's other caches — memory over disk — for this render
            // and the next. Sized well above a film's blocks: what the store
            // gives up is downloaded again.
            let blocks =
                tuile_storage_foyer::FoyerStore::keeping(tuile_storage_foyer::StoreConfig {
                    dir: cache.join("store-blocks"),
                    memory_bytes: 256 << 20,
                    disk_bytes: 32 << 30,
                    default_ttl: None,
                })
                .await?;
            let kept = Arc::new(tuile_repository::Kept::new(get, Arc::new(blocks.clone())));
            Ok(TileStore {
                objects: Arc::new(tuile_repository::RemoteStore::new(kept.clone(), "store")),
                remote_blocks: Some(kept),
                block_store: Some(blocks),
            })
        }
        None => Ok(TileStore {
            objects: tile_bucket(&std::env::var("TUILE_TILES_BUCKET").map_err(|_| {
                "no tile store: neither TUILE_TILES_REMOTE nor TUILE_TILES_BUCKET is set"
            })?)?,
            remote_blocks: None,
            block_store: None,
        }),
    }
}

/// `r,g,b`: a colour as three linear values, none below zero.
pub fn rgb(name: &str, colour: &str) -> Result<Vec3, Error> {
    let parts: Vec<f32> = colour
        .split(',')
        .map(|part| part.trim().parse())
        .collect::<Result<_, _>>()
        .map_err(|_| format!("{name} is <r>,<g>,<b>, not {colour}"))?;
    match parts[..] {
        [r, g, b] if parts.iter().all(|c| c.is_finite() && *c >= 0.0) => Ok(Vec3::new(r, g, b)),
        _ => Err(format!("{name} is <r>,<g>,<b>, each zero or more, not {colour}").into()),
    }
}

/// The sun a film is lit by when nobody placed one: 45° up, facing the
/// equator from 30° east of the meridian — 150° in the northern hemisphere,
/// 30° in the southern. A mid-morning sun where any film is: relief reads,
/// shadows fall away from the camera's usual side, and the ground is never
/// lit from the pole.
///
/// It replaced a sun left unoriented — along the Earth's axis, a sun over
/// the north pole — which lit every film from the north at an elevation
/// equal to its latitude. Films that placed no sun changed with it.
pub const DEFAULT_SUN: (f64, f64) = (150.0, 45.0);

/// The sun `sun` places where `eye` (ECEF) is — azimuth and elevation in
/// degrees — or, when none is given, [`DEFAULT_SUN`] there, its azimuth
/// mirrored south of the equator.
pub fn placed_sun(eye: [f64; 3], sun: Option<(f64, f64)>) -> Vec3 {
    let (azimuth, elevation) = sun.unwrap_or_else(|| {
        let north = ecef_to_geodetic(DVec3::from_array(eye)).lat >= 0.0;
        let (azimuth, elevation) = DEFAULT_SUN;
        (if north { azimuth } else { 180.0 - azimuth }, elevation)
    });
    sun_at(eye, azimuth, elevation)
}

/// The direction of a sun at `azimuth` from north towards east and
/// `elevation` above the horizon, both in degrees, where `eye` (ECEF) is.
pub fn sun_at(eye: [f64; 3], azimuth: f64, elevation: f64) -> Vec3 {
    let (azimuth, elevation) = (azimuth.to_radians(), elevation.to_radians());
    // East, north and up where the eye is.
    let local = enu_frame(ecef_to_geodetic(DVec3::from_array(eye)));
    let level = local.x_axis * azimuth.sin() + local.y_axis * azimuth.cos();
    (level * elevation.cos() + local.z_axis * elevation.sin()).as_vec3()
}

/// `bitrate` (bits a second), or when it is 0 the mean the H.264 profile
/// gives a `width`×`height` film at `fps`: a bitrate by the picture's size,
/// not one number for every size.
pub fn bitrate_for(bitrate: u32, width: u32, height: u32, fps: u32) -> u32 {
    match bitrate {
        0 => {
            tuile_farm::compress::Profile::h264()
                .bitrates(width, height, f64::from(fps.max(1)))
                .0
        }
        bits => bits,
    }
}

/// The film's encoder: the one asked for, or the best this build has — the
/// machine's own before software. `bitrate` in bits a second; 0 for the
/// encoder's own by the picture's size (`tuile_farm::compress::Profile`'s
/// rate tables), in place of one number for every size.
///
/// `mezzanine` is what a render node writes for the final stage
/// ([`crate::compress`]): HEVC Main10, near-transparent, by an NVIDIA card
/// or a Mac's VideoToolbox.
pub fn film_sink(path: String, bitrate: u32, codec: Option<&str>) -> Result<Box<dyn Sink>, Error> {
    #[cfg(all(target_os = "macos", feature = "videotoolbox"))]
    if matches!(codec, Some("h264") | None) {
        return Ok(Box::new(crate::VtFilm::at(
            path,
            bitrate,
            crate::VtCodec::H264,
        )));
    }
    #[cfg(all(target_os = "macos", feature = "videotoolbox"))]
    if codec == Some("mezzanine") {
        return Ok(Box::new(crate::VtFilm::at(
            path,
            bitrate,
            crate::VtCodec::Mezzanine,
        )));
    }
    #[cfg(all(target_os = "macos", feature = "videotoolbox"))]
    if codec == Some("mv-hevc") {
        return Ok(Box::new(crate::SpatialFilm::at(path, bitrate)));
    }
    #[cfg(all(target_os = "linux", feature = "nvenc"))]
    {
        use crate::{NvencCodec, NvencFilm};
        let device = std::env::var("TUILE_CUDA_DEVICE")
            .ok()
            .and_then(|d| d.parse().ok())
            .unwrap_or(0);
        match codec {
            Some("nvenc-av1") | None => {
                return Ok(Box::new(
                    NvencFilm::at(path, bitrate, NvencCodec::Av1).on_device(device),
                ))
            }
            Some("mezzanine") => {
                return Ok(Box::new(
                    NvencFilm::at(path, bitrate, NvencCodec::Mezzanine).on_device(device),
                ))
            }
            Some("nvenc-mv-hevc") | Some("mv-hevc") => {
                return Ok(Box::new(
                    NvencFilm::at(path, bitrate, NvencCodec::MvHevc).on_device(device),
                ))
            }
            Some("nvenc-h264") | Some("h264") => {
                return Ok(Box::new(
                    NvencFilm::at(path, bitrate, NvencCodec::H264).on_device(device),
                ))
            }
            _ => {}
        }
    }
    match codec {
        Some("av1") | None => Ok(Box::new(Av1Film::at(path, bitrate))),
        Some(other) => Err(format!("no {other} encoder was built into this binary").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::geo::{geodetic_to_ecef, Geodetic};

    /// Nobody's sun is 45° up and faces the equator, whichever side of it
    /// the film is; a sun asked for is that one.
    #[test]
    fn the_default_sun_stands_over_the_equator_side_never_the_pole() {
        for lat in [45.0f64, -33.0] {
            let at = Geodetic {
                lon: 6.0f64.to_radians(),
                lat: lat.to_radians(),
                height: 1500.0,
            };
            let eye = geodetic_to_ecef(at).to_array();
            let local = enu_frame(at);
            let sun = placed_sun(eye, None).as_dvec3();
            let up = sun.dot(local.z_axis).asin().to_degrees();
            assert!((up - 45.0).abs() < 1e-3, "{lat}: {up}° up");
            let towards_pole = sun.dot(local.y_axis) * lat.signum();
            assert!(towards_pole < -0.5, "{lat}: facing the equator");
            assert!(sun.dot(local.x_axis) > 0.3, "{lat}: from the east");
            let asked = placed_sun(eye, Some((270.0, 10.0))).as_dvec3();
            assert!((asked - sun_at(eye, 270.0, 10.0).as_dvec3()).length() < 1e-9);
        }
    }

    #[test]
    fn a_film_asked_for_no_bitrate_gets_its_size_s() {
        assert_eq!(bitrate_for(45_000_000, 3840, 2160, 30), 45_000_000);
        assert_eq!(bitrate_for(0, 720, 1280, 30), 3_000_000);
        assert_eq!(bitrate_for(0, 1920, 1080, 30), 6_000_000);
    }
}
