// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! LAB PROBE (throwaway, branch lab/render-opportunity-probe): where a
//! frame's time goes, on the CPU by named step and on the GPU by pass,
//! through pass-level timestamp queries where the device has them.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::time::Instant;

const SLOTS: u32 = 64;

struct Gpu {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    read: wgpu::Buffer,
    period_ns: f64,
}

/// Totals: milliseconds and count, by label.
#[derive(Default)]
pub struct Prof {
    cpu: RefCell<BTreeMap<&'static str, (f64, u64)>>,
    /// The first call and the longest, by label.
    first_max: RefCell<BTreeMap<&'static str, (f64, f64)>>,
    gpu_totals: RefCell<BTreeMap<&'static str, (f64, u64)>>,
    labels: RefCell<Vec<&'static str>>,
    next: Cell<u32>,
    gpu: Option<Gpu>,
    resolved: Cell<u32>,
}

impl Prof {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let gpu = device
            .features()
            .contains(wgpu::Features::TIMESTAMP_QUERY)
            .then(|| Gpu {
                set: device.create_query_set(&wgpu::QuerySetDescriptor {
                    label: Some("probe timestamps"),
                    ty: wgpu::QueryType::Timestamp,
                    count: SLOTS,
                }),
                resolve: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("probe resolve"),
                    size: u64::from(SLOTS) * 8,
                    usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                }),
                read: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("probe read"),
                    size: u64::from(SLOTS) * 8,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                }),
                period_ns: f64::from(queue.get_timestamp_period()),
            });
        Self {
            gpu,
            ..Self::default()
        }
    }

    pub fn has_gpu(&self) -> bool {
        self.gpu.is_some()
    }

    pub fn cpu(&self, label: &'static str, since: Instant) {
        let ms = since.elapsed().as_secs_f64() * 1000.0;
        {
            let mut fm = self.first_max.borrow_mut();
            let e = fm.entry(label).or_insert((ms, 0.0));
            e.1 = e.1.max(ms);
        }
        let mut cpu = self.cpu.borrow_mut();
        let e = cpu.entry(label).or_default();
        e.0 += ms;
        e.1 += 1;
    }

    fn pair(&self, label: &'static str) -> Option<(&wgpu::QuerySet, u32)> {
        let gpu = self.gpu.as_ref()?;
        let at = self.next.get();
        if at + 2 > SLOTS {
            return None;
        }
        self.next.set(at + 2);
        self.labels.borrow_mut().push(label);
        Some((&gpu.set, at))
    }

    pub fn render(&self, label: &'static str) -> Option<wgpu::RenderPassTimestampWrites<'_>> {
        self.pair(label)
            .map(|(set, at)| wgpu::RenderPassTimestampWrites {
                query_set: set,
                beginning_of_pass_write_index: Some(at),
                end_of_pass_write_index: Some(at + 1),
            })
    }

    pub fn compute(&self, label: &'static str) -> Option<wgpu::ComputePassTimestampWrites<'_>> {
        self.pair(label)
            .map(|(set, at)| wgpu::ComputePassTimestampWrites {
                query_set: set,
                beginning_of_pass_write_index: Some(at),
                end_of_pass_write_index: Some(at + 1),
            })
    }

    /// At the end of a frame's last encoder.
    pub fn resolve(&self, encoder: &mut wgpu::CommandEncoder) {
        let (Some(gpu), n) = (&self.gpu, self.next.get()) else {
            return;
        };
        if n == 0 {
            return;
        }
        encoder.resolve_query_set(&gpu.set, 0..n, &gpu.resolve, 0);
        encoder.copy_buffer_to_buffer(&gpu.resolve, 0, &gpu.read, 0, u64::from(n) * 8);
        self.resolved.set(n);
    }

    /// After the frame's submission was waited for.
    pub fn collect(&self, device: &wgpu::Device) {
        let n = self.resolved.replace(0);
        let labels: Vec<&'static str> = self.labels.borrow_mut().drain(..).collect();
        self.next.set(0);
        let Some(gpu) = &self.gpu else { return };
        if n == 0 {
            return;
        }
        let slice = gpu.read.slice(..u64::from(n) * 8);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        {
            let bytes = slice.get_mapped_range();
            let stamps: &[u64] = bytemuck::cast_slice(&bytes);
            let mut totals = self.gpu_totals.borrow_mut();
            for (i, label) in labels.iter().enumerate() {
                let (a, b) = (stamps[2 * i], stamps[2 * i + 1]);
                if b >= a && a != 0 {
                    let e = totals.entry(label).or_default();
                    e.0 += (b - a) as f64 * gpu.period_ns / 1e6;
                    e.1 += 1;
                }
            }
        }
        gpu.read.unmap();
    }

    /// Per frame, over `frames`.
    pub fn report(&self, frames: u32) -> String {
        let frames = f64::from(frames.max(1));
        let mut out = String::from("PROBE cpu (ms/frame, calls):\n");
        let cpu = self.cpu.borrow();
        let mut rows: Vec<_> = cpu.iter().collect();
        rows.sort_by(|a, b| b.1 .0.total_cmp(&a.1 .0));
        let fm = self.first_max.borrow();
        for (label, (ms, n)) in rows {
            let (first, max) = fm.get(label).copied().unwrap_or_default();
            out += &format!(
                "PROBE   cpu {label:34} {:9.2} ms/frame  {n:6}  first {first:9.2}  max {max:9.2}  mean-after-first {:9.2}\n",
                ms / frames,
                if *n > 1 { (ms - first) / (*n - 1) as f64 } else { 0.0 }
            );
        }
        let gpu = self.gpu_totals.borrow();
        if gpu.is_empty() {
            out += "PROBE gpu: no timestamp queries on this device\n";
        } else {
            let all: f64 = gpu.values().map(|v| v.0).sum();
            out += &format!("PROBE gpu passes (ms/frame), sum {:.2}:\n", all / frames);
            let mut rows: Vec<_> = gpu.iter().collect();
            rows.sort_by(|a, b| b.1 .0.total_cmp(&a.1 .0));
            for (label, (ms, n)) in rows {
                out += &format!("PROBE   gpu {label:28} {:9.2} ms/frame  {n}\n", ms / frames);
            }
        }
        out
    }
}
