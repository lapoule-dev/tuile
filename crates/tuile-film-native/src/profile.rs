// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A segment's CPU, sampled while the service makes it.
//!
//! The process's threads are sampled [`HZ`] times a second of CPU from the
//! request to the answer; the stacks are named and written as a gzipped
//! pprof, which the service puts beside the segment for whoever reads its
//! receipt — a profiler that cannot reach the service reads it from there.
//!
//! One sampler runs in a process at a time: a segment that begins while
//! another is sampled is not, and its receipt says it has no profile.
//! Sampling sees time on the CPU only: a thread that waits for the card or
//! the encoder is not in it — the receipt's GPU and encoder times are.

use crate::Error;

/// Samples a second of CPU.
pub const HZ: u32 = 100;

/// A sampler running; its profile is taken with [`Sampling::pprof_gz`].
pub struct Sampling {
    #[cfg(unix)]
    guard: pprof::ProfilerGuard<'static>,
}

impl Sampling {
    /// Starts sampling, if no other sampler runs in the process and the
    /// platform has one.
    pub fn start() -> Option<Self> {
        #[cfg(unix)]
        {
            // Not where a sample's unwinding could meet a lock its thread
            // holds: the C library, the unwinder, threads, the vDSO.
            pprof::ProfilerGuardBuilder::default()
                .frequency(HZ as i32)
                .blocklist(&["libc", "libgcc", "pthread", "vdso"])
                .build()
                .ok()
                .map(|guard| Self { guard })
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    /// Stops sampling and gives the profile: a gzipped pprof, its stacks
    /// named. Blocking: naming the stacks reads the binary's symbols.
    pub fn pprof_gz(self) -> Result<Vec<u8>, Error> {
        #[cfg(unix)]
        {
            use pprof::protos::Message;
            use std::io::Write;
            let report = self.guard.report().build()?;
            drop(self.guard);
            let profile = report.pprof()?;
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            gz.write_all(&profile.encode_to_vec())?;
            Ok(gz.finish()?)
        }
        #[cfg(not(unix))]
        {
            Err("no sampler on this platform".into())
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Read;

    /// Burns CPU in a function of its own, for the profile to name.
    #[inline(never)]
    fn spin_for_the_sampler(until: std::time::Instant) -> u64 {
        let mut x = 1u64;
        while std::time::Instant::now() < until {
            for _ in 0..10_000 {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
            }
        }
        x
    }

    #[test]
    fn a_sampled_stretch_comes_back_as_a_gzipped_pprof_that_names_its_functions() {
        let Some(sampling) = Sampling::start() else {
            // Another test of this binary samples at the same time.
            eprintln!("a sampler runs already: skipped");
            return;
        };
        // Only one sampler at a time.
        assert!(Sampling::start().is_none());
        let until = std::time::Instant::now() + std::time::Duration::from_millis(400);
        std::hint::black_box(spin_for_the_sampler(until));
        let gz = sampling.pprof_gz().expect("a profile");
        assert_eq!(&gz[..2], &[0x1f, 0x8b], "gzipped");
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(&gz[..])
            .read_to_end(&mut raw)
            .expect("gunzip");
        let profile = <pprof::protos::Profile as pprof::protos::Message>::decode(&raw[..])
            .expect("a pprof");
        assert!(!profile.sample.is_empty(), "no sample in 400 ms of CPU");
        assert!(
            profile
                .string_table
                .iter()
                .any(|s| s.contains("spin_for_the_sampler")),
            "the burning function is named"
        );
    }
}
