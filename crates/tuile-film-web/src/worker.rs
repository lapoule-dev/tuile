// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use futures_util::future::try_join_all;
use js_sys::{Array, Uint8Array};
use tuile_film::{texture, Cursor, FrameCamera, Look, Mesh, Pack, TileKey};
use tuile_film_gpu::{FilmGpu, Settings, TileMesh, OUTPUT_FORMAT};
use tuile_mp4::{Muxer, ParameterSets};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Blob, BlobPropertyBag, ColorSpaceConversion, ImageBitmap, ImageBitmapOptions, OffscreenCanvas,
    PremultiplyAlpha, WorkerGlobalScope,
};

fn js(e: impl std::fmt::Display) -> JsError {
    JsError::new(&e.to_string())
}

fn scope() -> WorkerGlobalScope {
    js_sys::global().unchecked_into()
}

fn now_ms() -> f64 {
    scope().performance().map_or(0.0, |p| p.now())
}

/// What a pack holds, read before any worker is started.
#[wasm_bindgen(getter_with_clone)]
pub struct PackInfo {
    pub first: u32,
    pub last: u32,
    pub width: u32,
    pub height: u32,
    pub tiles: u32,
    pub scene: String,
}

#[wasm_bindgen]
impl PackInfo {
    /// Opens a pack — verifying its digest — and reports its range and the
    /// viewport its first frame was baked for.
    pub fn read(bytes: &[u8]) -> Result<PackInfo, JsError> {
        console_error_panic_hook::set_once();
        let pack = Pack::open(bytes).map_err(js)?;
        let (first, last) = pack.frame_range();
        let view = pack.view_of(first).map_err(js)?;
        Ok(PackInfo {
            first,
            last,
            width: view.viewport_px[0] as u32,
            height: view.viewport_px[1] as u32,
            tiles: pack.tile_count() as u32,
            scene: pack.scene_digest().to_string(),
        })
    }
}

/// One frame's account: what changed and where the time went.
#[wasm_bindgen]
#[derive(Debug, Clone, Copy)]
pub struct FrameStats {
    pub frame: u32,
    pub selected: u32,
    pub entered: u32,
    pub left: u32,
    /// Pack decompression of the entering meshes.
    pub unpack_ms: f64,
    /// Browser image decoding of the entering textures.
    pub decode_ms: f64,
    /// Buffer and texture creation, uploads.
    pub upload_ms: f64,
    /// Recording and submitting the frame.
    pub record_ms: f64,
}

/// One worker's slice of a film.
#[wasm_bindgen]
pub struct FilmWorker {
    bytes: Vec<u8>,
    cursor: Cursor,
    gpu: FilmGpu,
    surface: wgpu::Surface<'static>,
    aspect: f32,
}

async fn bitmap(png: Vec<u8>) -> Result<ImageBitmap, JsValue> {
    let parts = Array::of1(&Uint8Array::from(png.as_slice()));
    let kind = BlobPropertyBag::new();
    kind.set_type("image/png");
    let blob = Blob::new_with_u8_array_sequence_and_options(&parts, &kind)?;
    // The bytes are sRGB-encoded and stay so: no colour management, no
    // premultiplication — the shader decodes them through an sRGB view.
    let options = ImageBitmapOptions::new();
    options.set_color_space_conversion(ColorSpaceConversion::None);
    options.set_premultiply_alpha(PremultiplyAlpha::None);
    let promise =
        scope().create_image_bitmap_with_blob_and_image_bitmap_options(&blob, &options)?;
    Ok(JsFuture::from(promise).await?.unchecked_into())
}

#[wasm_bindgen]
impl FilmWorker {
    /// Takes the pack's bytes and an `OffscreenCanvas` of the film's display
    /// size, and gets a WebGPU device ready for frames `first..=last`.
    pub async fn create(
        canvas: OffscreenCanvas,
        pack: Vec<u8>,
        first: u32,
        last: u32,
        supersample: u32,
    ) -> Result<FilmWorker, JsError> {
        console_error_panic_hook::set_once();
        let cursor = {
            let opened = Pack::open(&pack).map_err(js)?;
            Cursor::new(&opened, first, last).map_err(js)?
        };
        let (width, height) = (canvas.width(), canvas.height());

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::BROWSER_WEBGPU,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let surface = instance
            .create_surface(wgpu::SurfaceTarget::OffscreenCanvas(canvas))
            .map_err(js)?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
            })
            .await
            .map_err(js)?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("film worker"),
                required_limits: adapter.limits(),
                ..Default::default()
            })
            .await
            .map_err(js)?;
        let caps = surface.get_capabilities(&adapter);
        if !caps.formats.contains(&OUTPUT_FORMAT) {
            return Err(js(format!(
                "the canvas cannot be configured as {OUTPUT_FORMAT:?}; it offers {:?}",
                caps.formats
            )));
        }
        surface.configure(
            &device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: OUTPUT_FORMAT,
                width,
                height,
                present_mode: wgpu::PresentMode::Fifo,
                desired_maximum_frame_latency: 2,
                alpha_mode: wgpu::CompositeAlphaMode::Opaque,
                view_formats: vec![],
            },
        );
        let gpu = FilmGpu::new(
            device,
            queue,
            Settings {
                width,
                height,
                supersample: supersample.max(1),
                look: Look::default(),
            },
        );
        Ok(FilmWorker {
            bytes: pack,
            cursor,
            gpu,
            surface,
            aspect: width as f32 / height as f32,
        })
    }

    /// Frames this worker has yet to render.
    pub fn remaining(&self) -> u32 {
        self.cursor.remaining()
    }

    /// Renders the next frame of the slice and presents it to the canvas, or
    /// returns `None` when the slice is done. The caller captures the canvas
    /// right after this resolves.
    pub async fn next(&mut self) -> Result<Option<FrameStats>, JsError> {
        let Self {
            bytes,
            cursor,
            gpu,
            surface,
            aspect,
        } = self;
        let pack = Pack::reopen(bytes).map_err(js)?;
        let Some(diff) = cursor.advance(&pack) else {
            return Ok(None);
        };
        let diff = diff.map_err(js)?;

        let t0 = now_ms();
        let mut meshes = Vec::with_capacity(diff.enter.len());
        let mut pngs = Vec::new();
        for tile in &diff.enter {
            meshes.push((TileKey::of(tile), Mesh::of(&pack, tile).map_err(js)?));
            if let Some(png) = texture(&pack, tile).map_err(js)? {
                pngs.push((meshes.len() - 1, png));
            }
        }

        let t1 = now_ms();
        // Every entering texture decodes at once, on the browser's threads.
        let (owners, futures): (Vec<usize>, Vec<_>) =
            pngs.into_iter().map(|(at, png)| (at, bitmap(png))).unzip();
        let bitmaps = try_join_all(futures)
            .await
            .map_err(|e| js(format!("image decode: {e:?}")))?;
        let mut textures: Vec<Option<wgpu::Texture>> = (0..meshes.len()).map(|_| None).collect();

        let t2 = now_ms();
        for (at, bitmap) in owners.into_iter().zip(bitmaps) {
            let (w, h) = (bitmap.width(), bitmap.height());
            let texture = gpu.create_albedo(w, h);
            gpu.queue().copy_external_image_to_texture(
                &wgpu::CopyExternalImageSourceInfo {
                    source: wgpu::ExternalImageSource::ImageBitmap(bitmap.clone()),
                    origin: wgpu::Origin2d::ZERO,
                    flip_y: false,
                },
                wgpu::CopyExternalImageDestInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                    color_space: wgpu::PredefinedColorSpace::Srgb,
                    premultiplied_alpha: false,
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
            bitmap.close();
            textures[at] = Some(texture);
        }
        for ((key, mesh), texture) in meshes.iter().zip(textures) {
            gpu.enter(
                *key,
                &TileMesh {
                    origin_ecef: mesh.origin_ecef,
                    positions: &mesh.positions,
                    normals: &mesh.normals,
                    uvs: &mesh.uvs,
                    indices: &mesh.indices,
                    index_count: mesh.index_count,
                    base_color_factor: mesh.base_color_factor,
                },
                texture,
            )
            .map_err(js)?;
        }

        let t3 = now_ms();
        let camera = FrameCamera::of(&diff.view, *aspect);
        let mut encoder = gpu.render(&camera, &diff.selection).map_err(js)?;
        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            other => return Err(js(format!("no canvas texture: {other:?}"))),
        };
        encoder.copy_texture_to_texture(
            gpu.output().as_image_copy(),
            frame.texture.as_image_copy(),
            gpu.output().size(),
        );
        gpu.queue().submit([encoder.finish()]);
        frame.present();
        // Released only now: the frame that stopped drawing them is recorded.
        for key in &diff.leave {
            gpu.leave(key);
        }
        let t4 = now_ms();

        Ok(Some(FrameStats {
            frame: diff.frame,
            selected: diff.selection.len() as u32,
            entered: diff.enter.len() as u32,
            left: diff.leave.len() as u32,
            unpack_ms: t1 - t0,
            decode_ms: t2 - t1,
            upload_ms: t3 - t2,
            record_ms: t4 - t3,
        }))
    }
}

/// The page's side: one mp4 from every worker's encoded chunks, in order.
#[wasm_bindgen]
pub struct FilmMuxer {
    inner: Option<Muxer>,
}

#[wasm_bindgen]
impl FilmMuxer {
    /// `avcc` is the `description` of the first encoder's decoder config.
    #[wasm_bindgen(constructor)]
    pub fn new(width: u16, height: u16, fps: u32, avcc: &[u8]) -> Result<FilmMuxer, JsError> {
        let sets = ParameterSets::from_avcc(avcc).map_err(js)?;
        Ok(FilmMuxer {
            inner: Some(Muxer::new(width, height, fps, sets).map_err(js)?),
        })
    }

    /// Fails unless an encoder with this `avcc` can join the film.
    pub fn check(&self, avcc: &[u8]) -> Result<(), JsError> {
        let sets = ParameterSets::from_avcc(avcc).map_err(js)?;
        self.muxer()?.check(&sets).map_err(js)
    }

    /// Appends frame `index` of the film (from 0), as WebCodecs emitted it.
    pub fn push(&mut self, index: u32, data: Vec<u8>, key: bool) -> Result<(), JsError> {
        self.inner
            .as_mut()
            .ok_or_else(|| js("the film is already finished"))?
            .push(u64::from(index), data, key)
            .map_err(js)
    }

    pub fn frames(&self) -> Result<u32, JsError> {
        Ok(self.muxer()?.frames() as u32)
    }

    /// The finished mp4.
    pub fn finish(&mut self) -> Result<Vec<u8>, JsError> {
        self.inner
            .take()
            .ok_or_else(|| js("the film is already finished"))?
            .finish()
            .map_err(js)
    }

    fn muxer(&self) -> Result<&Muxer, JsError> {
        self.inner
            .as_ref()
            .ok_or_else(|| js("the film is already finished"))
    }
}
