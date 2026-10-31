// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What Android says about the device, read into the viewer's own terms.
//!
//! The Android shell asks the system through its activity (`android`), and
//! the answers arrive as the platform's numbers and text: a trim level, a
//! thermal status, `/proc/meminfo`, the launch intent's extras. Each one is
//! read here, by a pure function, so that what a number *means* is tested on
//! the host — the shell only carries numbers across.

use crate::profile::{DeviceMemory, Pressure, Thermal};

/// `ComponentCallbacks2`'s trim levels, as the system sends them to
/// `onTrimMemory`.
mod trim {
    pub(super) const RUNNING_MODERATE: i32 = 5;
    #[allow(
        dead_code,
        reason = "a level the ranges below cover; named for the reader and the tests"
    )]
    pub(super) const RUNNING_LOW: i32 = 10;
    pub(super) const RUNNING_CRITICAL: i32 = 15;
    pub(super) const UI_HIDDEN: i32 = 20;
    pub(super) const BACKGROUND: i32 = 40;
    pub(super) const MODERATE: i32 = 60;
    #[allow(
        dead_code,
        reason = "a level the ranges below cover; named for the reader and the tests"
    )]
    pub(super) const COMPLETE: i32 = 80;
}

/// How short of memory a trim level says the system is.
///
/// - The three *running* levels are sent to an application in front: moderate
///   and low are a warning, critical is the system about to end background
///   processes and then this one.
/// - `UI_HIDDEN` says only that the window is gone; the lifecycle already let
///   go of what the background lets go of, so it asks for nothing more.
/// - The background levels say where the process stands in the queue of
///   those the system will end: near the top (`MODERATE`, `COMPLETE`) is
///   critical.
///
/// A level the platform adds later is read by where it falls between these.
pub(crate) fn pressure_of_trim(level: i32) -> Pressure {
    match level {
        trim::UI_HIDDEN => Pressure::Normal,
        l if l >= trim::MODERATE => Pressure::Critical,
        l if l >= trim::BACKGROUND => Pressure::Warning,
        l if l > trim::UI_HIDDEN => Pressure::Warning,
        l if l >= trim::RUNNING_CRITICAL => Pressure::Critical,
        l if l >= trim::RUNNING_MODERATE => Pressure::Warning,
        _ => Pressure::Normal,
    }
}

/// The activity's packed trim signal: how many times the system has trimmed,
/// above; the last level, in the low 32 bits.
pub(crate) fn unpack_trim(signal: i64) -> (u64, Pressure) {
    let count = (signal >> 32) as u64 & 0xffff_ffff;
    let level = (signal & 0xffff_ffff) as u32 as i32;
    (count, pressure_of_trim(level))
}

/// The safe area of a `width` × `height` drawable, in device pixels (left,
/// top, width, height), from the activity's packed insets: left, top, right
/// and bottom, sixteen bits each from the low end.
///
/// From Android 15 an application's window is drawn edge to edge, under the
/// status bar and the navigation bar, and winit reports the whole window as
/// its inner size; the bars' insets are the only word of what the system
/// covers. `None` when the activity has none yet (`-1`) or they leave nothing
/// — the controls then use the whole drawable.
pub(crate) fn safe_area_of_insets(
    packed: i64,
    width: u32,
    height: u32,
) -> Option<(f64, f64, f64, f64)> {
    if packed < 0 {
        return None;
    }
    let side = |shift: u32| f64::from(((packed >> shift) & 0xffff) as u16);
    let (left, top, right, bottom) = (side(0), side(16), side(32), side(48));
    let (w, h) = (
        f64::from(width) - left - right,
        f64::from(height) - top - bottom,
    );
    (w > 0.0 && h > 0.0).then_some((left, top, w, h))
}

/// `PowerManager`'s thermal status, 0 (none) to 6 (shutdown), as the grades
/// the viewer paces its frames by.
///
/// Light is the iPhone's "fair": warm, nothing to do yet. Moderate is where
/// the system starts throttling ("serious": 30 frames a second); severe and
/// above are close to a shutdown ("critical": 20).
pub(crate) fn thermal_of_status(status: i32) -> Thermal {
    match status {
        i32::MIN..=0 => Thermal::Nominal,
        1 => Thermal::Fair,
        2 => Thermal::Serious,
        _ => Thermal::Critical,
    }
}

/// The device's memory from `/proc/meminfo`: `MemTotal` and `MemAvailable`,
/// in kibibytes there.
///
/// `MemAvailable` is the kernel's estimate of what can still be allocated
/// without swapping — on Android, what the low-memory killer leaves before it
/// starts on the cached processes. It stands in for iOS's per-process figure:
/// Android has no per-process limit for native memory, only this pool. `None`
/// when the text has no `MemTotal`, which a Linux kernel always writes.
pub(crate) fn memory_from_meminfo(text: &str) -> Option<DeviceMemory> {
    let field = |name: &str| {
        text.lines().find_map(|line| {
            let rest = line.strip_prefix(name)?.strip_prefix(':')?;
            let kib: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            Some(kib * 1024)
        })
    };
    Some(DeviceMemory {
        physical: field("MemTotal")?,
        available: field("MemAvailable"),
    })
}

/// The launch's settings: what a developer passes with
/// `adb shell am start … -e TUILE_PIN_LEVEL 5`, the counterpart of a
/// simulator's `SIMCTL_CHILD_` variables.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct LaunchSettings {
    /// Set in the process's environment before the session starts.
    pub accepted: Vec<(String, String)>,
    /// Named, never their values: refused because they look like a
    /// credential, or are not a setting of the viewer at all.
    pub refused: Vec<String>,
}

/// Reads the activity's `KEY=VALUE` lines.
///
/// Only the viewer's own knobs are taken — `TUILE_…`, `RUST_LOG`,
/// `RUST_BACKTRACE` — and **never a credential**: a name with `TOKEN`,
/// `SECRET`, `PASSWORD` or `KEY` in it is refused whatever its prefix. An
/// intent's extras are visible to whoever can read the system's log of
/// activity starts, and an Android build of this viewer holds no secret: its
/// catalog is asked through a proxy that holds the token (see
/// `tuile_cesium_ion::proxy`).
pub(crate) fn launch_settings(lines: &str) -> LaunchSettings {
    let mut settings = LaunchSettings::default();
    for line in lines.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let named_well = key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        let ours = key.starts_with("TUILE_") || key == "RUST_LOG" || key == "RUST_BACKTRACE";
        let secret = ["TOKEN", "SECRET", "PASSWORD", "KEY"]
            .iter()
            .any(|word| key.contains(word));
        if named_well && ours && !secret {
            settings
                .accepted
                .push((key.to_owned(), value.trim().to_owned()));
        } else {
            settings.refused.push(key.to_owned());
        }
    }
    settings
}

/// What the activity holds for import (`TuileActivity.takeImport`), as a
/// request: `link\n<url>`, or `file\n<name>\n<text>`. Anything else is
/// nothing.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn import_request(held: &str) -> Option<crate::steer::Request> {
    let (kind, rest) = held.split_once('\n')?;
    match kind {
        "link" => Some(crate::steer::Request::Url(rest.trim().to_owned())),
        "file" => {
            let (file, text) = rest.split_once('\n').unwrap_or((rest, ""));
            Some(crate::steer::Request::Import {
                file: file.trim().to_owned(),
                text: text.to_owned(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn what_the_activity_holds_becomes_a_request() {
        use super::import_request;
        use crate::steer::Request;
        assert_eq!(
            import_request("link\ntuile://import?src=https://x.test/c.tuile"),
            Some(Request::Url(
                "tuile://import?src=https://x.test/c.tuile".into()
            ))
        );
        assert_eq!(
            import_request("file\nmine.tuile\n[ion]\ntoken = \"t\"\n"),
            Some(Request::Import {
                file: "mine.tuile".into(),
                text: "[ion]\ntoken = \"t\"\n".into()
            })
        );
        assert_eq!(import_request("other\nx"), None);
        assert_eq!(import_request(""), None);
    }
    use super::*;

    /// Every level the platform defines, read as the viewer's three grades —
    /// what `adb shell am send-trim-memory <package> RUNNING_CRITICAL` sends
    /// among them.
    #[test]
    fn trim_levels_read_as_pressure() {
        use Pressure::{Critical, Normal, Warning};
        assert_eq!(pressure_of_trim(0), Normal);
        assert_eq!(pressure_of_trim(trim::RUNNING_MODERATE), Warning);
        assert_eq!(pressure_of_trim(trim::RUNNING_LOW), Warning);
        assert_eq!(pressure_of_trim(trim::RUNNING_CRITICAL), Critical);
        assert_eq!(pressure_of_trim(trim::UI_HIDDEN), Normal);
        assert_eq!(pressure_of_trim(trim::BACKGROUND), Warning);
        assert_eq!(pressure_of_trim(trim::MODERATE), Critical);
        assert_eq!(pressure_of_trim(trim::COMPLETE), Critical);
        // Levels the platform may add later fall where they lie.
        assert_eq!(pressure_of_trim(12), Warning);
        assert_eq!(pressure_of_trim(30), Warning);
        assert_eq!(pressure_of_trim(100), Critical);
        assert_eq!(pressure_of_trim(-1), Normal);
    }

    #[test]
    fn the_trim_signal_unpacks_into_a_count_and_a_level() {
        assert_eq!(unpack_trim(0), (0, Pressure::Normal));
        assert_eq!(unpack_trim((3 << 32) | 15), (3, Pressure::Critical));
        assert_eq!(unpack_trim((1 << 32) | 5), (1, Pressure::Warning));
    }

    /// A Pixel-sized window, drawn edge to edge: the status bar above, the
    /// gesture bar below; the controls keep between them.
    #[test]
    fn insets_give_the_area_the_bars_leave() {
        let packed = |l: i64, t: i64, r: i64, b: i64| l | (t << 16) | (r << 32) | (b << 48);
        assert_eq!(
            safe_area_of_insets(packed(0, 128, 0, 66), 1080, 2400),
            Some((0.0, 128.0, 1080.0, 2206.0))
        );
        // Landscape, the cutout on the left and the navigation bar on the right.
        assert_eq!(
            safe_area_of_insets(packed(110, 0, 126, 63), 2400, 1080),
            Some((110.0, 0.0, 2164.0, 1017.0))
        );
        assert_eq!(
            safe_area_of_insets(packed(0, 0, 0, 0), 800, 600),
            Some((0.0, 0.0, 800.0, 600.0))
        );
        // Not known yet, or nothing left: the whole drawable is the caller's.
        assert_eq!(safe_area_of_insets(-1, 1080, 2400), None);
        assert_eq!(
            safe_area_of_insets(packed(600, 0, 600, 0), 1080, 2400),
            None
        );
    }

    #[test]
    fn thermal_status_reads_as_the_grades_frames_are_paced_by() {
        assert_eq!(thermal_of_status(0), Thermal::Nominal);
        assert_eq!(thermal_of_status(1), Thermal::Fair);
        assert_eq!(thermal_of_status(2), Thermal::Serious);
        assert_eq!(thermal_of_status(3), Thermal::Critical);
        assert_eq!(thermal_of_status(6), Thermal::Critical);
        assert_eq!(thermal_of_status(-1), Thermal::Nominal);
    }

    /// The head of a real `/proc/meminfo`, from a 2 GiB emulator image.
    #[test]
    fn meminfo_gives_the_device_and_what_is_left() {
        let text = "MemTotal:        2014028 kB\n\
                    MemFree:          123456 kB\n\
                    MemAvailable:     901234 kB\n\
                    Buffers:            4096 kB\n";
        let m = memory_from_meminfo(text).expect("a total");
        assert_eq!(m.physical, 2_014_028 * 1024);
        assert_eq!(m.available, Some(901_234 * 1024));
        // A kernel too old for MemAvailable: the total only, and the profile
        // plans around a quarter of it.
        let old = memory_from_meminfo("MemTotal: 1000 kB\nMemFree: 10 kB\n").expect("a total");
        assert_eq!((old.physical, old.available), (1000 * 1024, None));
        assert_eq!(memory_from_meminfo("nothing"), None);
        // Not confused by a field whose name starts the same.
        let m = memory_from_meminfo("MemTotalX: 5 kB\nMemTotal: 7 kB\n").expect("a total");
        assert_eq!(m.physical, 7 * 1024);
    }

    /// The viewer's knobs are taken; a credential never is, whatever it is
    /// called, and neither is anything that is not the viewer's.
    #[test]
    fn launch_settings_take_knobs_and_never_a_credential() {
        let s = launch_settings(
            "TUILE_PIN_LEVEL=5\n\
             RUST_LOG=info,tuile_viewer=debug\n\
             TUILE_CATALOG_URL=http://127.0.0.1:8091/catalog/\n\
             CESIUM_ION_TOKEN=abc\n\
             TUILE_TILES_TOKEN=abc\n\
             TUILE_ION_TOKEN=abc\n\
             TUILE_API_KEY=abc\n\
             HOME=/sdcard\n\
             tuile_lower=1\n\
             no separator\n",
        );
        assert_eq!(
            s.accepted,
            [
                ("TUILE_PIN_LEVEL".to_owned(), "5".to_owned()),
                ("RUST_LOG".to_owned(), "info,tuile_viewer=debug".to_owned()),
                (
                    "TUILE_CATALOG_URL".to_owned(),
                    "http://127.0.0.1:8091/catalog/".to_owned()
                ),
            ]
        );
        assert_eq!(
            s.refused,
            [
                "CESIUM_ION_TOKEN",
                "TUILE_TILES_TOKEN",
                "TUILE_ION_TOKEN",
                "TUILE_API_KEY",
                "HOME",
                "tuile_lower"
            ]
        );
        assert_eq!(launch_settings(""), LaunchSettings::default());
    }
}
