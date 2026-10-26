// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Getting a drawn frame back onto the CPU.
//!
//! Two things make this worth owning rather than rewriting per host. The first
//! is that wgpu requires every copied row to begin on a 256-byte boundary, so
//! the buffer is not `width * height * 4` and the rows have to be unpadded on
//! the way out. Getting that wrong does not fail — it produces an image that is
//! **sheared**, which then gets blamed on whatever the capture was taken to
//! diagnose.
//!
//! The second is that reading back means waiting for the GPU, and a caller
//! should be able to see that in the shape of the API rather than discover it
//! in a profile. Hence two calls: [`Readback::capture`] costs nothing and joins
//! the frame's own command buffer; [`Readback::take`] is the one that blocks.

use crate::context::GpuContext;

/// wgpu requires each copied row to start on a 256-byte boundary.
fn padded_row_bytes(width: u32) -> u32 {
    const ALIGN: u32 = 256;
    (width * 4).div_ceil(ALIGN) * ALIGN
}

/// A reusable staging buffer for frame captures.
///
/// Grown on demand and kept, so a session recording every frame does not
/// allocate a megabyte per frame — which, on a backend where each allocation is
/// a kernel round trip, is a cost large enough to change what is being measured.
#[derive(Default)]
pub struct Readback {
    buffer: Option<wgpu::Buffer>,
}

impl Readback {
    /// Queues the copy into the frame's own encoder. Does not block.
    ///
    /// Must be recorded **before** the frame is presented: afterwards the
    /// texture belongs to the surface again and there is nothing left to copy.
    pub fn capture(
        &mut self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        texture: &wgpu::Texture,
        size: (u32, u32),
    ) {
        let (width, height) = size;
        let padded = padded_row_bytes(width);
        let needed = u64::from(padded) * u64::from(height);
        if self.buffer.as_ref().is_none_or(|b| b.size() < needed) {
            self.buffer = Some(gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tuile readback"),
                size: needed,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }));
        }
        let Some(buffer) = self.buffer.as_ref() else {
            return;
        };
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Maps the buffer and returns tightly packed RGBA8.
    ///
    /// **Blocks on the GPU.** Call after the frame has been submitted, and only
    /// when the pixels are actually wanted: a frame that waits for its own
    /// readback runs at a fraction of the speed of the session being diagnosed.
    pub fn take(&mut self, gpu: &GpuContext, size: (u32, u32)) -> Option<Vec<u8>> {
        let (width, height) = size;
        let padded = padded_row_bytes(width) as usize;
        let tight = (width * 4) as usize;
        let buffer = self.buffer.as_ref()?;
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        gpu.device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
        // A mapping that failed reads as no pixels, as a poll that failed
        // does: the caller already lives without a capture.
        let data = slice.get_mapped_range().ok()?;
        let mut out = Vec::with_capacity(tight * height as usize);
        for row in 0..height as usize {
            let start = row * padded;
            out.extend_from_slice(&data[start..start + tight]);
        }
        drop(data);
        buffer.unmap();
        Some(out)
    }
}

/// A slot of a [`ReadbackRing`]: free, copied into, being mapped, mapped
/// and waiting to be read, failed, or handed to a reader.
const FREE: u8 = 0;
const COPIED: u8 = 1;
const MAPPING: u8 = 2;
const MAPPED: u8 = 3;
const FAILED: u8 = 4;
const HANDED: u8 = 5;

struct Slot {
    buffer: Option<wgpu::Buffer>,
    state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    tag: u64,
    size: (u32, u32),
    format: wgpu::TextureFormat,
}

/// Frames read back without waiting for them: a ring of staging buffers,
/// each copied into on the frame's own encoder, mapped once the GPU is
/// done with it, and read later — on whatever thread the caller likes.
///
/// [`Readback`] blocks the frame on its own pixels, which is right for a
/// trace of a few frames and wrong for a recording of a whole session: the
/// session recorded would no longer be the session. Here nothing blocks.
/// When every slot is still in flight the frame is not captured
/// ([`Self::capture`] says so) — a dropped picture is counted and costs
/// nothing; a stalled frame would change the very timing a flicker is
/// recorded to explain.
pub struct ReadbackRing {
    slots: Vec<Slot>,
    dropped: u64,
    failed: u64,
}

/// One captured frame, mapped and ready to be read: see
/// [`ReadbackRing::ready`]. Reading it gives its slot back to the ring.
pub struct MappedFrame {
    buffer: wgpu::Buffer,
    state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// What the caller tagged the capture with: a frame index, say.
    pub tag: u64,
    /// Its size, pixels.
    pub size: (u32, u32),
    format: wgpu::TextureFormat,
}

impl MappedFrame {
    /// Tightly packed RGBA8 — the channels put in that order whatever the
    /// surface's own (BGRA on a Mac) — and the slot freed. Every
    /// `step`-th pixel of every `step`-th row, averaged over the
    /// `step × step` block it stands for: `step` 1 is the frame as drawn.
    pub fn read(self, step: u32) -> Option<Vec<u8>> {
        // Out of the mapping in one copy, and the slot given back at once:
        // mapped memory is read best in one sweep, and the ring wants its
        // slot for the next frame while this one is still being worked on.
        let raw = self
            .buffer
            .slice(..)
            .get_mapped_range()
            .ok()
            .map(|data| data.to_vec());
        self.buffer.unmap();
        self.state
            .store(FREE, std::sync::atomic::Ordering::Release);
        Some(self.unpadded(&raw?, step.max(1)))
    }

    fn unpadded(&self, data: &[u8], step: u32) -> Vec<u8> {
        let (width, height) = self.size;
        let padded = padded_row_bytes(width) as usize;
        let bgra = matches!(
            self.format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        );
        let order: [usize; 4] = if bgra { [2, 1, 0, 3] } else { [0, 1, 2, 3] };
        let (w, h) = ((width / step) as usize, (height / step) as usize);
        let step = step as usize;
        let mut out = vec![0u8; w * h * 4];
        if step == 1 {
            for (y, row) in out.chunks_exact_mut(w * 4).enumerate() {
                let src = &data[y * padded..y * padded + w * 4];
                for (d, s) in row.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    d.copy_from_slice(&[s[order[0]], s[order[1]], s[order[2]], s[order[3]]]);
                }
            }
            return out;
        }
        let area = (step * step) as u32;
        let mut sums = vec![0u32; w * 4];
        for (y, row) in out.chunks_exact_mut(w * 4).enumerate() {
            sums.iter_mut().for_each(|s| *s = 0);
            for dy in 0..step {
                let src = &data[(y * step + dy) * padded..][..w * step * 4];
                for (x, block) in src.chunks_exact(step * 4).enumerate() {
                    for px in block.chunks_exact(4) {
                        for c in 0..4 {
                            sums[x * 4 + c] += u32::from(px[order[c]]);
                        }
                    }
                }
            }
            for (d, s) in row.iter_mut().zip(&sums) {
                *d = ((s + area / 2) / area) as u8;
            }
        }
        out
    }
}

impl ReadbackRing {
    /// A ring of `slots` staging buffers: frames that may be in flight at
    /// once between the copy and the read.
    pub fn new(slots: usize) -> Self {
        Self {
            slots: (0..slots.max(1))
                .map(|_| Slot {
                    buffer: None,
                    state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(FREE)),
                    tag: 0,
                    size: (0, 0),
                    format: wgpu::TextureFormat::Rgba8Unorm,
                })
                .collect(),
            dropped: 0,
            failed: 0,
        }
    }

    /// Frames copied whose mapping failed: not read.
    pub fn failed(&self) -> u64 {
        self.failed
    }

    /// Frames not captured because every slot was in flight.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Queues the copy of `texture` into a free slot, on the frame's own
    /// encoder, tagged `tag`. Does not block. `false` when no slot is free:
    /// the frame is not captured.
    ///
    /// Must be recorded before the frame is presented, and followed by
    /// [`Self::submitted`] once the encoder has been submitted.
    pub fn capture(
        &mut self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        texture: &wgpu::Texture,
        size: (u32, u32),
        tag: u64,
    ) -> bool {
        use std::sync::atomic::Ordering;
        let Some(slot) = self
            .slots
            .iter_mut()
            .find(|s| s.state.load(Ordering::Acquire) == FREE)
        else {
            self.dropped += 1;
            return false;
        };
        let (width, height) = size;
        let needed = u64::from(padded_row_bytes(width)) * u64::from(height);
        if slot.buffer.as_ref().is_none_or(|b| b.size() < needed) {
            slot.buffer = Some(gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tuile readback ring"),
                size: needed,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }));
        }
        let Some(buffer) = slot.buffer.as_ref() else {
            return false;
        };
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row_bytes(width)),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        slot.tag = tag;
        slot.size = size;
        slot.format = texture.format();
        slot.state.store(COPIED, Ordering::Release);
        true
    }

    /// Asks for the slots copied into since the last call to be mapped:
    /// call once the encoder that copied them has been submitted.
    pub fn submitted(&mut self) {
        use std::sync::atomic::Ordering;
        for slot in &self.slots {
            if slot.state.load(Ordering::Acquire) != COPIED {
                continue;
            }
            let Some(buffer) = slot.buffer.as_ref() else {
                continue;
            };
            slot.state.store(MAPPING, Ordering::Release);
            let state = std::sync::Arc::clone(&slot.state);
            buffer.map_async(wgpu::MapMode::Read, .., move |result| {
                state.store(
                    if result.is_ok() { MAPPED } else { FAILED },
                    Ordering::Release,
                );
            });
        }
    }

    /// The frames mapped since the last call, oldest first; polls the
    /// device without waiting. With `wait`, waits for every frame in
    /// flight first: at the end of a recording.
    pub fn ready(&mut self, gpu: &GpuContext, wait: bool) -> Vec<MappedFrame> {
        use std::sync::atomic::Ordering;
        let poll = if wait {
            wgpu::PollType::wait_indefinitely()
        } else {
            wgpu::PollType::Poll
        };
        let _ = gpu.device.poll(poll);
        let mut out = Vec::new();
        for slot in &mut self.slots {
            match slot.state.load(Ordering::Acquire) {
                MAPPED => {
                    let Some(buffer) = slot.buffer.clone() else {
                        continue;
                    };
                    // Handed out: the slot is the reader's until it reads.
                    slot.state.store(HANDED, Ordering::Release);
                    out.push(MappedFrame {
                        buffer,
                        state: std::sync::Arc::clone(&slot.state),
                        tag: slot.tag,
                        size: slot.size,
                        format: slot.format,
                    });
                }
                FAILED => {
                    self.failed += 1;
                    slot.state.store(FREE, Ordering::Release);
                }
                _ => {}
            }
        }
        out.sort_by_key(|f| f.tag);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::padded_row_bytes;

    /// **Every row starts on a 256-byte boundary, and none is short.**
    ///
    /// The failure this guards is silent: too small a stride does not error, it
    /// shears the image, and the shear then gets blamed on the renderer.
    #[test]
    fn rows_are_padded_up_to_the_alignment_never_down() {
        for width in [1u32, 63, 64, 65, 100, 256, 257, 1920, 3420] {
            let padded = padded_row_bytes(width);
            assert_eq!(padded % 256, 0, "width {width} gave an unaligned stride");
            assert!(
                padded >= width * 4,
                "width {width} needs {} bytes but was given {padded}",
                width * 4
            );
            assert!(
                padded - width * 4 < 256,
                "width {width} was padded by a whole row or more"
            );
        }
    }
}
