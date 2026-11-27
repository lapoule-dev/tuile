// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! How hard a session may push the machine it runs on.
//!
//! The viewer is one program on a desktop and on a phone, and nearly every
//! difference between the two is a number: how much ground the GPU holds,
//! how many reads the store serves at once, how many threads decode, how big
//! the tile store is, how many frames a second are worth drawing when nothing
//! moves. Those numbers live here, side by side, so the two machines are
//! compared in one place instead of in `cfg` blocks scattered through the
//! session.
//!
//! What a session may hold, load and upload is the graphics budget's
//! ([`Profile::budget`]): its numbers are sized here from the device.
//! [`Profile::handheld`] is sized from the phone: it shares one pool of
//! memory between the CPU and the GPU, and the system ends an application
//! that takes too much of it without asking — so the budget is a share of
//! what the process may still take, not a constant.
//!
//! [`Profile::browser`] is a page: the desktop's numbers or the phone's, cut
//! to what a tab may hold — the worker that keeps the residency is a wasm32
//! heap, four gibibytes at the very most — drawn on demand, and paused while
//! the tab is hidden.

use std::time::Duration;

use tuile_core::budget::{BudgetConfig, Bytes};

/// The platform's signals, in the budget's vocabulary: what a memory warning,
/// an Android trim level, the background and the device's temperature are
/// handed to the budget as (`set_pressure`, `set_thermal`).
pub(crate) use tuile_core::budget::{Pressure, Thermal};

const MIB: u64 = 1024 * 1024;

/// What the system says about the memory this process may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeviceMemory {
    /// The device's physical memory, bytes.
    pub physical: u64,
    /// How much more this process may allocate before the system ends it,
    /// bytes — when the system says. A simulator and a desktop do not.
    pub available: Option<u64>,
}

impl DeviceMemory {
    /// The bytes a session may plan around: what is left to this process when
    /// that is known, a quarter of the device otherwise.
    fn usable(&self) -> u64 {
        self.available
            .filter(|&a| a > 0)
            .unwrap_or(self.physical / 4)
    }
}

/// The budget's pinned floor on every profile: levels `0..=2`, mesh and
/// drape, loaded first and never evicted — the backdrop's level
/// ([`crate::backdrop::BASE_LEVEL`]). Level 3 and up are graded inside the
/// progressive score (Laurent, 2026-11-23): level 3 weighs 0.95 of a tile of
/// the present need, level 4 half that, halving finer, so under pressure the
/// fine go first, then the intermediate levels, level 4, level 3 last
/// (`docs/explorations/budget-progressive-coarse-preload.md`).
/// `TUILE_PIN_LEVEL` sets another floor.
pub(crate) const FLOOR_LEVEL: u32 = 2;
const _: () = assert!(FLOOR_LEVEL >= crate::backdrop::BASE_LEVEL);

/// The numbers a session runs with. See the module.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Profile {
    /// The GPU memory the graphics budget is sized from, bytes: every
    /// class's target, overflow and hard limit are shares of it
    /// ([`BudgetConfig::interactive`]). A phone's follows what the system
    /// leaves the process (`retarget`).
    pub gpu_bytes: u64,
    /// The budget's floor: levels `0..=pinned_level`, mesh and drape, loaded
    /// first and never evicted — what every fallback lands on, and what the
    /// backdrop draws.
    pub pinned_level: u32,
    /// The radius `ρ` of the neighbourhood preloaded around the view, in
    /// tiles of each level (`docs/explorations/budget-progressive-coarse-preload.md`):
    /// `⌊π(ρ+1)²⌋` tiles per level between the floor and the eye's, the
    /// bound of what is preloaded. A desktop's 4 (78 a level, about 300 MiB
    /// at level 13), a phone's 2 (28 a level, about 105 MiB).
    pub reach_tiles: u32,
    /// Reads of the local tile store in flight at once, a quota of their own
    /// beside the network's (I15): a desktop's store answers 32 at a time —
    /// at 8 a warm start converged later than a cold one (D5, 2026-10-20).
    pub max_local_reads: u32,
    /// The most imagery levels a drape may sit above its tile's terrain
    /// level: what one draw's slots hold (a 5 × 5 mosaic is two levels).
    pub imagery_over_terrain_cap: u32,
    /// The most imagery slots the ground shader is built for: every one is a
    /// texture fetch on every fragment, covered or not, so it is the number
    /// that sets a fragment's cost. A desktop's
    /// [`USEFUL_IMAGERY_SLOTS`](tuile_core::raster::USEFUL_IMAGERY_SLOTS) (25,
    /// a 5 × 5 mosaic and the floor); a phone's
    /// [`MIN_IMAGERY_SLOTS`](tuile_core::raster::MIN_IMAGERY_SLOTS) (9, the
    /// 3 × 3 a drape needs to be correct), which an iPhone 12 Pro Max drew in
    /// 17 ms a frame against 61–71 ms at 25. What a lower ceiling gives up is
    /// the width of the mosaic: it coarsens a level to fit.
    pub imagery_slot_ceiling: u32,
    /// The traversal's screen-space error threshold, logical pixels (D-L1):
    /// a desktop's 1, the 2 device pixels it always drew at on a display of
    /// density 2; a phone's ⅔, the same 2 at its density 3. A window moved
    /// between displays then asks for nothing new.
    pub sse_threshold: f64,
    /// How long one load may take before the budget cancels and retries it,
    /// seconds: the budget's `load_timeout`. A desktop's 4 s; a page's
    /// longer, its loads crossing a proxy and waiting on a few decode
    /// workers — at 4 s a browser cancelled two hundred loads in forty
    /// seconds that would have landed, and the ground near the eye never
    /// sharpened.
    pub load_timeout_s: f64,
    /// Threads of the async runtime: network, store, the server.
    pub runtime_workers: usize,
    /// Its blocking pool: the store's file I/O.
    pub blocking_threads: usize,
    /// Threads decoding and resampling tiles.
    pub decode_threads: usize,
    /// The tile store's memory and disk tiers, bytes.
    pub store_memory_bytes: usize,
    pub store_disk_bytes: usize,
    /// Whether the window stays hidden until the floor is on the GPU (a
    /// desktop), or opens at once with a loading indicator over the globe
    /// (a phone, where the system ends an application that shows nothing).
    pub hold_window_until_warm: bool,
    /// Whether the frame rate is held to sixty frames a second and lowered as
    /// the device heats or saves power — a phone's battery and heat. A
    /// desktop is never held back. Every device draws only what changed
    /// (`app::demand`); this is how fast it may draw while something does.
    pub paced_by_heat: bool,
    /// Whether the surface and its render targets are let go of while the
    /// application is in the background.
    pub release_surface_in_background: bool,
    /// Whether loads are held while the view is not on screen: a phone in
    /// the background, a hidden browser tab.
    pub pause_loads_when_hidden: bool,
    /// Whether the window is the application's own, which Escape closes and
    /// a missing GPU ends — never a page's canvas, which belongs to the page.
    pub owns_its_window: bool,
    /// Whether the counters are served on a local HTTP endpoint.
    pub metrics_endpoint: bool,
    /// Whether a damaged tile store is cleared and started afresh — a phone,
    /// whose system purges caches from under it — rather than left alone for
    /// the session to run without.
    pub clear_damaged_store: bool,
    /// Whether the system takes the window's surface away when the
    /// application leaves the front — Android destroys it, and a GPU surface
    /// must be let go of before the call that says so returns — rather than
    /// only telling the application it is no longer in front.
    pub suspend_takes_the_surface: bool,
}

impl Profile {
    /// The desktop application: a quarter of the machine's memory for the
    /// GPU's ground, between 1 and 4 GiB — what unified memory carries
    /// beside a compositor and a browser.
    /// A page's is [`Self::browser`].
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn desktop() -> Self {
        Self::desktop_with(crate::device::memory().physical)
    }

    /// [`Self::desktop`] on a machine of `physical` bytes.
    pub(crate) fn desktop_with(physical: u64) -> Self {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        Self {
            gpu_bytes: (physical / 4).clamp(1024 * MIB, 4096 * MIB),
            pinned_level: FLOOR_LEVEL,
            reach_tiles: 4,
            max_local_reads: 32,
            imagery_over_terrain_cap: 2,
            imagery_slot_ceiling: tuile_core::raster::USEFUL_IMAGERY_SLOTS,
            sse_threshold: 1.0,
            load_timeout_s: 4.0,
            runtime_workers: cores,
            blocking_threads: 512,
            decode_threads: cores.saturating_sub(1).max(1),
            store_memory_bytes: 256 * MIB as usize,
            // Saturating: on a 32-bit target (a page's wasm) the number does
            // not fit, and is never used there.
            store_disk_bytes: 4096usize.saturating_mul(MIB as usize),
            hold_window_until_warm: true,
            paced_by_heat: false,
            release_surface_in_background: false,
            pause_loads_when_hidden: false,
            owns_its_window: true,
            metrics_endpoint: true,
            clear_damaged_store: false,
            suspend_takes_the_surface: false,
        }
    }

    /// A phone or a tablet with `memory`, and `cores` processors.
    pub(crate) fn handheld(memory: DeviceMemory, cores: usize) -> Self {
        // A fifth of what is left to the process for the GPU's ground: the
        // CPU's decoded copies, the render targets and the runtime need the
        // rest, well under the half at which the system starts ending
        // applications.
        let gpu_bytes = (memory.usable() / 5).clamp(160 * MIB, 768 * MIB);
        Self {
            gpu_bytes,
            pinned_level: FLOOR_LEVEL,
            reach_tiles: 2,
            max_local_reads: 8,
            // The low clamp drapes one level over its terrain, the rest two.
            imagery_over_terrain_cap: if gpu_bytes <= 192 * MIB { 1 } else { 2 },
            imagery_slot_ceiling: tuile_core::raster::MIN_IMAGERY_SLOTS,
            sse_threshold: 2.0 / 3.0,
            load_timeout_s: 4.0,
            runtime_workers: 2,
            blocking_threads: 4,
            // Leave the main thread and the runtime a core each.
            decode_threads: cores.saturating_sub(2).clamp(1, 3),
            store_memory_bytes: 32 * MIB as usize,
            store_disk_bytes: 768 * MIB as usize,
            hold_window_until_warm: false,
            paced_by_heat: true,
            release_surface_in_background: true,
            pause_loads_when_hidden: true,
            owns_its_window: true,
            metrics_endpoint: false,
            clear_damaged_store: true,
            suspend_takes_the_surface: false,
        }
    }

    /// An Android phone or tablet, as [`Profile::current`] makes it there: the
    /// handheld numbers, sized the same way from what the system leaves the
    /// process, on a system that takes the surface away whenever the
    /// application leaves the front.
    #[cfg(test)]
    pub(crate) fn android(memory: DeviceMemory, cores: usize) -> Self {
        Self {
            suspend_takes_the_surface: true,
            ..Self::handheld(memory, cores)
        }
    }

    /// A page in a browser: a desktop's or a phone's, by `handheld`, with
    /// `memory_gib` the device's memory as the browser reports it.
    ///
    /// The worker that keeps the session is a wasm32 heap: four gibibytes at
    /// the very most and in practice half that before a tab is killed, so
    /// the desktop's budget does not fit and is cut. There is no tile store
    /// yet, no runtime of threads, no counters' endpoint; decoding runs in
    /// the worker, which has one thread.
    #[cfg(any(target_arch = "wasm32", test))]
    pub(crate) fn browser(handheld: bool, memory_gib: Option<f64>) -> Self {
        const GIB: f64 = (1u64 << 30) as f64;
        let base = if handheld {
            let physical = memory_gib.unwrap_or(4.0).max(1.0) * GIB;
            Self::handheld(
                DeviceMemory {
                    physical: physical as u64,
                    available: None,
                },
                1,
            )
        } else {
            Self::desktop_with((memory_gib.unwrap_or(8.0).max(1.0) * GIB) as u64)
        };
        // A desktop page near the ground at density 2 wants more than 768 MiB
        // of ground: held at the pool's target there, a drag evicted five
        // thousand tiles in a few seconds and the ground flickered back and
        // forth between its levels. A desktop browser's GPU is the machine's;
        // 1.5 GiB is what a quarter of 8 GiB — the most `deviceMemory` ever
        // reports — and the desktop's own quarter rule give.
        let ceiling = if handheld { 384 * MIB } else { 1536 * MIB };
        Self {
            gpu_bytes: base.gpu_bytes.min(ceiling),
            // A page's "local" reads are its loader's own decoded caches —
            // memory, no store — and eight of them at once left the network
            // idle with two thousand tiles wanted near the ground.
            max_local_reads: 32,
            // `tuile_web::patience::LOAD_ATTEMPT_TIMEOUT` is the same.
            load_timeout_s: 30.0,
            runtime_workers: 1,
            blocking_threads: 1,
            decode_threads: 1,
            store_memory_bytes: 0,
            store_disk_bytes: 0,
            hold_window_until_warm: false,
            paced_by_heat: true,
            release_surface_in_background: false,
            pause_loads_when_hidden: true,
            owns_its_window: false,
            metrics_endpoint: false,
            clear_damaged_store: false,
            suspend_takes_the_surface: false,
            ..base
        }
    }

    /// The profile for this build's platform and this device. A page's is
    /// [`Profile::browser`], chosen by the page.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn current() -> Self {
        let cores = || std::thread::available_parallelism().map_or(2, |n| n.get());
        if cfg!(target_os = "android") {
            Self {
                suspend_takes_the_surface: true,
                ..Self::handheld(crate::device::memory(), cores())
            }
        } else if cfg!(target_os = "ios") {
            Self::handheld(crate::device::memory(), cores())
        } else {
            Self::desktop()
        }
    }

    /// The graphics budget this profile runs on: the interactive policy over
    /// [`Self::gpu_bytes`], with the profile's floor, local-read quota and
    /// imagery cap. Everything a session may hold, load and upload is in it.
    pub(crate) fn budget(&self) -> BudgetConfig {
        let mut b = BudgetConfig::interactive(Bytes(self.gpu_bytes));
        b.pinned_level = Some(self.pinned_level);
        b.reach_tiles = self.reach_tiles;
        b.max_local_reads = self.max_local_reads;
        b.imagery_over_terrain_cap = self.imagery_over_terrain_cap;
        b.load_timeout = tuile_core::budget::Micros((self.load_timeout_s * 1e6) as u64);
        b
    }

    /// The traversal's configuration for this profile: the reference
    /// interactive session at the profile's threshold.
    pub(crate) fn traversal(&self) -> tuile_core::traversal::Config {
        tuile_core::traversal::Config {
            maximum_screen_space_error: self.sse_threshold,
            ..tuile_core::traversal::Config::interactive_globe()
        }
    }

    /// The shortest interval between two frames, for how hot the device runs
    /// and whether it is saving power. A desktop is never held back.
    pub(crate) fn frame_interval(&self, thermal: Thermal, low_power: bool) -> Duration {
        if !self.paced_by_heat {
            return Duration::ZERO;
        }
        let fps = match thermal {
            Thermal::Critical => 20,
            Thermal::Serious => 30,
            _ if low_power => 30,
            _ => 60,
        };
        Duration::from_secs(1) / fps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    /// The desktop's budget is a quarter of the machine, between 1 and
    /// 4 GiB, reads its store 32 at a time, and is never held back.
    #[test]
    fn the_desktop_sizes_its_budget_from_the_machine() {
        assert_eq!(Profile::desktop_with(16 * GIB).gpu_bytes, 4 * GIB);
        assert_eq!(Profile::desktop_with(64 * GIB).gpu_bytes, 4 * GIB);
        assert_eq!(Profile::desktop_with(2 * GIB).gpu_bytes, GIB);
        let p = Profile::desktop_with(16 * GIB);
        let b = p.budget();
        assert_eq!(b.max_local_reads, 32);
        assert_eq!(b.pinned_level, Some(FLOOR_LEVEL));
        assert_eq!(b.reach_tiles, 4);
        assert!(b.validate().is_ok());
        assert!(p.hold_window_until_warm && !p.paced_by_heat);
        assert!(p.owns_its_window && !p.pause_loads_when_hidden);
        assert_eq!(p.frame_interval(Thermal::Critical, true), Duration::ZERO);
    }

    /// A phone's budget is a share of what the process may still take, held
    /// between a floor that can still show a view and a ceiling well short of
    /// where the system ends applications.
    #[test]
    fn a_phone_sizes_its_budget_from_the_memory_it_is_left() {
        let phone = |available| {
            Profile::handheld(
                DeviceMemory {
                    physical: 6 * GIB,
                    available,
                },
                6,
            )
        };
        let roomy = phone(Some(3 * GIB));
        assert_eq!(roomy.gpu_bytes, 3 * GIB / 5);
        // Less left, less held — down to the floor and no further.
        let tight = phone(Some(GIB / 2));
        assert_eq!(tight.gpu_bytes, 160 * MIB);
        assert_eq!(tight.imagery_over_terrain_cap, 1);
        // Lots left still caps out.
        assert_eq!(phone(Some(16 * GIB)).gpu_bytes, 768 * MIB);
        // A simulator says nothing about what is left: a quarter of the
        // machine stands in for it.
        assert_eq!(phone(None).gpu_bytes, 6 * GIB / 4 / 5);
        for p in [roomy, tight] {
            assert!(p.budget().validate().is_ok());
            assert!(p.runtime_workers <= 2 && p.decode_threads <= 3);
            assert!(p.decode_threads >= 1);
            assert!(!p.hold_window_until_warm && p.paced_by_heat);
            assert!(p.release_surface_in_background && !p.metrics_endpoint);
            assert!(p.pause_loads_when_hidden && p.owns_its_window);
        }
    }

    /// An Android handset is a phone in every number, and the one whose
    /// surface goes when it leaves the front.
    #[test]
    fn an_android_handset_is_a_phone_that_loses_its_surface() {
        let memory = DeviceMemory {
            physical: 3 * GIB,
            available: Some(GIB),
        };
        let android = Profile::android(memory, 8);
        let phone = Profile::handheld(memory, 8);
        assert!(android.suspend_takes_the_surface);
        assert!(!phone.suspend_takes_the_surface);
        assert!(!Profile::desktop_with(16 * GIB).suspend_takes_the_surface);
        assert!(!Profile::browser(true, None).suspend_takes_the_surface);
        assert_eq!(
            Profile {
                suspend_takes_the_surface: false,
                ..android.clone()
            },
            phone
        );
        assert_eq!(android.gpu_bytes, GIB / 5);
    }

    /// The fetches a fragment pays are the machine's to weigh: a desktop keeps
    /// the 25 slots it always drew with, a phone (and a page on one) the 9 of
    /// the 3 × 3 floor. The viewer builds its shader, and tells its loader,
    /// from this number.
    #[test]
    fn the_slot_ceiling_is_the_machines() {
        use tuile_core::raster::{MIN_IMAGERY_SLOTS, USEFUL_IMAGERY_SLOTS};
        let memory = DeviceMemory {
            physical: 6 * GIB,
            available: None,
        };
        assert_eq!(
            Profile::desktop_with(16 * GIB).imagery_slot_ceiling,
            USEFUL_IMAGERY_SLOTS
        );
        assert_eq!(
            Profile::handheld(memory, 6).imagery_slot_ceiling,
            MIN_IMAGERY_SLOTS
        );
        assert_eq!(
            Profile::android(memory, 6).imagery_slot_ceiling,
            MIN_IMAGERY_SLOTS
        );
        assert_eq!(
            Profile::browser(true, Some(4.0)).imagery_slot_ceiling,
            MIN_IMAGERY_SLOTS
        );
        assert_eq!(
            Profile::browser(false, Some(16.0)).imagery_slot_ceiling,
            USEFUL_IMAGERY_SLOTS
        );
    }

    /// A page fits a wasm32 heap, opens at once on its indicator, draws on
    /// demand, pauses when hidden, keeps its surface and never quits on a key.
    #[test]
    fn a_browser_fits_a_tab_and_belongs_to_its_page() {
        let desktop = Profile::browser(false, Some(16.0));
        let phone = Profile::browser(true, Some(4.0));
        // Under a wasm32 heap's 4 GiB with room to spare: the page keeps its
        // meshes' CPU copies, the worker its decoded tier (a quarter).
        assert!(desktop.gpu_bytes <= 1536 * MIB);
        for p in [&desktop, &phone] {
            assert_eq!(p.pinned_level, FLOOR_LEVEL);
            assert!(!p.hold_window_until_warm && p.paced_by_heat);
            assert!(p.pause_loads_when_hidden && !p.release_surface_in_background);
            assert!(!p.owns_its_window && !p.metrics_endpoint);
            assert_eq!((p.store_memory_bytes, p.store_disk_bytes), (0, 0));
        }
        assert!(phone.gpu_bytes < desktop.gpu_bytes);
        assert!(phone.gpu_bytes <= 384 * MIB);
        // A browser that does not say how much memory there is gets a phone
        // of four gibibytes, not nothing.
        assert!(Profile::browser(true, None).gpu_bytes >= 160 * MIB);
    }

    /// Hotter runs slower; saving power runs slower.
    #[test]
    fn the_phone_slows_down_when_it_is_hot() {
        let p = Profile::handheld(
            DeviceMemory {
                physical: 8 * GIB,
                available: None,
            },
            6,
        );
        let at = |t, low| p.frame_interval(t, low);
        assert_eq!(at(Thermal::Nominal, false), Duration::from_secs(1) / 60);
        assert_eq!(at(Thermal::Fair, true), Duration::from_secs(1) / 30);
        assert_eq!(at(Thermal::Serious, false), Duration::from_secs(1) / 30);
        assert_eq!(at(Thermal::Critical, false), Duration::from_secs(1) / 20);
    }
}
