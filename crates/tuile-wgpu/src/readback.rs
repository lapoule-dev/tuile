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
        let data = slice.get_mapped_range();
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
