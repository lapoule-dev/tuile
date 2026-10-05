// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use crate::source::{Held, Source, BLOCK_BYTES};
use futures_util::future::try_join_all;
use futures_util::stream::{self, StreamExt};
use js_sys::{Array, Uint8Array};
use tuile_film::{
    block_plan, cameras, file_reads, frame_tiles, texture_of_span, Cursor, FrameCamera, Look, Mesh,
    Pack, TileKey,
};
use tuile_film_gpu::{FilmGpu, Settings, TileMesh, OUTPUT_FORMAT};
use tuile_mp4::{Codec, Muxer, ParameterSets};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Blob, BlobPropertyBag, ColorSpaceConversion, ImageBitmap, ImageBitmapOptions, OffscreenCanvas,
    PremultiplyAlpha, WorkerGlobalScope,
};

/// Spans closer than this are read together.
const COALESCE_GAP: u64 = 256 << 10;
/// No read of a frame's tiles is merged past this.
const READ_AT_MOST: u64 = 16 << 20;
/// Blocks in flight at once while fetching ahead.
const PRELOAD_AT_ONCE: usize = 6;

fn js(e: impl std::fmt::Display) -> JsError {
    JsError::new(&e.to_string())
}

fn scope() -> WorkerGlobalScope {
    js_sys::global().unchecked_into()
}

fn now_ms() -> f64 {
    js_sys::Date::now()
}

/// A pack opened for looking at: its table is read, nothing else until a
/// texture is asked for.
#[wasm_bindgen]
pub struct PackView {
    source: Source,
    head: Vec<u8>,
    first: u32,
    last: u32,
    width: u32,
    height: u32,
    tiles: u32,
    scene: String,
}

#[wasm_bindgen]
impl PackView {
    /// Reads a pack's table from its URL (by range).
    pub async fn open(source: JsValue) -> Result<PackView, JsError> {
        console_error_panic_hook::set_once();
        let source = Source::from_js(&source)?;
        let head = source.head().await?;
        let (first, last, width, height, tiles, scene) = {
            let pack = Pack::open_table(&head).map_err(js)?;
            let (first, last) = pack.frame_range();
            let view = pack.view_of(first).map_err(js)?;
            (
                first,
                last,
                view.viewport_px[0] as u32,
                view.viewport_px[1] as u32,
                pack.tile_count() as u32,
                pack.scene_digest().to_string(),
            )
        };
        Ok(PackView {
            source,
            head,
            first,
            last,
            width,
            height,
            tiles,
            scene,
        })
    }

    #[wasm_bindgen(getter)]
    pub fn first(&self) -> u32 {
        self.first
    }

    #[wasm_bindgen(getter)]
    pub fn last(&self) -> u32 {
        self.last
    }

    /// The viewport the first frame was baked for.
    #[wasm_bindgen(getter)]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[wasm_bindgen(getter)]
    pub fn height(&self) -> u32 {
        self.height
    }

    #[wasm_bindgen(getter)]
    pub fn tiles(&self) -> u32 {
        self.tiles
    }

    #[wasm_bindgen(getter)]
    pub fn scene(&self) -> String {
        self.scene.clone()
    }

    /// Bytes of the table that was read to open the pack.
    #[wasm_bindgen(getter)]
    pub fn table_bytes(&self) -> u32 {
        self.head.len() as u32
    }

    /// The camera path, at most `max` samples, seven numbers each: frame,
    /// longitude, latitude (degrees), height (m), heading, pitch, vertical
    /// field of view (degrees).
    pub fn cameras(&self, max: u32) -> Result<Vec<f64>, JsError> {
        let pack = Pack::open_table(&self.head).map_err(js)?;
        Ok(cameras(&pack, max as usize)
            .into_iter()
            .flat_map(|c| {
                [
                    f64::from(c.frame),
                    c.lon_deg,
                    c.lat_deg,
                    c.height_m,
                    c.heading_deg,
                    c.pitch_deg,
                    c.fovy_deg,
                ]
            })
            .collect())
    }

    /// The tiles a frame draws, as a JSON array of
    /// `{id, drape, vertices, triangles, textureBytes, lon, lat}`. Ids are
    /// strings: they do not fit a JS number.
    pub fn frame_tiles(&self, frame: u32) -> Result<String, JsError> {
        let pack = Pack::open_table(&self.head).map_err(js)?;
        let rows: Vec<String> = frame_tiles(&pack, frame)
            .map_err(js)?
            .into_iter()
            .map(|t| {
                format!(
                    r#"{{"id":"{}","drape":"{:016x}","vertices":{},"triangles":{},"textureBytes":{},"lon":{},"lat":{}}}"#,
                    t.id,
                    t.drape,
                    t.vertices,
                    t.triangles,
                    t.texture_bytes,
                    if t.lon_deg.is_finite() { t.lon_deg } else { 0.0 },
                    if t.lat_deg.is_finite() { t.lat_deg } else { 0.0 },
                )
            })
            .collect();
        Ok(format!("[{}]", rows.join(",")))
    }

    /// The encoded texture (PNG) of the `index`-th tile of a frame, read by
    /// range; empty when the tile has none.
    pub async fn texture(&self, frame: u32, index: u32) -> Result<Vec<u8>, JsError> {
        let pack = Pack::open_table(&self.head).map_err(js)?;
        let tiles = pack.frame(frame).map_err(js)?;
        let tile = tiles
            .get(index as usize)
            .ok_or_else(|| js(format!("frame {frame} has no tile {index}")))?;
        let blob = self.head.len() as u64;
        let Some(fetch) = file_reads(&pack, blob, std::slice::from_ref(tile), 0, u64::MAX)
            .into_iter()
            .next()
        else {
            return Ok(Vec::new());
        };
        if fetch.range.is_empty() {
            return Ok(Vec::new());
        }
        let bytes = self.source.read(fetch.range.clone()).await?;
        Ok(
            texture_of_span(&pack, tile, fetch.range.start - blob, &bytes)
                .map_err(js)?
                .unwrap_or_default(),
        )
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
    /// Reading the entering tiles' bytes (ranges).
    pub fetch_ms: f64,
    pub fetched_bytes: f64,
    pub requests: u32,
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
    source: Source,
    head: Vec<u8>,
    cursor: Cursor,
    gpu: FilmGpu,
    surface: wgpu::Surface<'static>,
    aspect: f32,
    /// Where each frame's I420 planes are copied for a software encoder;
    /// `None` when the browser's own encoder reads the canvas.
    readback: Option<wgpu::Buffer>,
    /// The slice this worker renders.
    first: u32,
    last: u32,
    /// Blocks fetched ahead of the frames that read them.
    held: Held,
}

/// What fetching ahead did.
#[wasm_bindgen]
#[derive(Debug, Clone, Copy)]
pub struct Preloaded {
    pub blocks: u32,
    pub bytes: f64,
    /// Whether the blocks are held in this module's memory. When they did
    /// not fit the budget they were fetched all the same — into the
    /// browser's cache, which answers the render's reads from disk.
    pub in_memory: bool,
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
    /// Takes the pack's source (a URL or a Blob) and an `OffscreenCanvas` of
    /// the film's display size, and gets a WebGPU device ready for frames
    /// `first..=last`. Only the pack's table is read now.
    pub async fn create(
        canvas: OffscreenCanvas,
        source: JsValue,
        first: u32,
        last: u32,
        supersample: u32,
    ) -> Result<FilmWorker, JsError> {
        console_error_panic_hook::set_once();
        let source = Source::from_js(&source)?;
        let head = source.head().await?;
        let cursor = {
            let opened = Pack::open_table(&head).map_err(js)?;
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
            source,
            head,
            cursor,
            gpu,
            surface,
            aspect: width as f32 / height as f32,
            readback: None,
            first,
            last,
            held: Held::default(),
        })
    }

    /// Makes every frame from now on also leave its picture as I420 planes,
    /// for [`FilmWorker::read_i420`]. Off by default: the browser's encoder
    /// reads the canvas and needs no copy.
    pub fn enable_i420(&mut self) {
        let size = self.gpu.i420_planes().size();
        self.readback = Some(self.gpu.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("film i420 readback"),
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        }));
    }

    /// The last frame's picture as planar I420 (Y, then U, then V), BT.709
    /// limited range, converted on the GPU. Half the bytes of RGBA, and what
    /// an encoder wants.
    pub async fn read_i420(&self) -> Result<Vec<u8>, JsError> {
        let buffer = self
            .readback
            .as_ref()
            .ok_or_else(|| js("enable_i420 was not called"))?;
        let (done, mapped) = futures_channel::oneshot::channel();
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = done.send(result);
            });
        mapped
            .await
            .map_err(|_| js("the readback was dropped"))?
            .map_err(js)?;
        let bytes = buffer.slice(..).get_mapped_range().to_vec();
        buffer.unmap();
        Ok(bytes)
    }

    /// Fetches, before the first frame, every block of the pack this slice
    /// reads — so that rendering waits on no network.
    ///
    /// The table says exactly which blocks those are. When they fit in
    /// `budget_bytes` they are held in this module's memory, and each is let
    /// go after the last frame that reads it; when they do not, they are
    /// fetched and dropped, which leaves them in the browser's cache.
    /// `progress(done, total)` is called as blocks arrive.
    pub async fn preload(
        &mut self,
        budget_bytes: f64,
        progress: &js_sys::Function,
    ) -> Result<Preloaded, JsError> {
        let plan = {
            let pack = Pack::open_table(&self.head).map_err(js)?;
            block_plan(
                &pack,
                self.head.len() as u64,
                self.first,
                self.last,
                BLOCK_BYTES,
                COALESCE_GAP,
                READ_AT_MOST,
            )
            .map_err(js)?
        };
        let total = plan.len();
        let in_memory = total as f64 * BLOCK_BYTES as f64 <= budget_bytes;
        let source = &self.source;
        let mut fetched = stream::iter(plan.iter().map(|(index, last)| (*index, *last)))
            .map(|(index, last)| async move {
                Ok::<_, JsError>((index, last, source.fetch_block(index).await?))
            })
            .buffer_unordered(PRELOAD_AT_ONCE);
        let (mut done, mut bytes) = (0u32, 0f64);
        while let Some(block) = fetched.next().await {
            let (index, last, data) = block?;
            done += 1;
            bytes += data.len() as f64;
            if in_memory {
                self.held.keep(index, data, last);
            }
            let _ = progress.call2(&JsValue::NULL, &done.into(), &(total as u32).into());
        }
        Ok(Preloaded {
            blocks: done,
            bytes,
            in_memory,
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
            source,
            head,
            cursor,
            gpu,
            surface,
            aspect,
            readback,
            held,
            ..
        } = self;
        let pack = Pack::open_table(head).map_err(js)?;
        let Some(diff) = cursor.advance(&pack) else {
            return Ok(None);
        };
        let diff = diff.map_err(js)?;

        // The entering tiles' bytes, in as few ranged reads as possible, all
        // in flight at once.
        // What enters is brought in a read at a time: a frame can bring in
        // a hundred megabytes of tiles, and holding every read, every
        // decompressed mesh and every PNG of it at once is more than a
        // worker's memory should be asked for. Each read's tiles are on the
        // GPU, and its bytes let go, before the next is made.
        let blob = head.len() as u64;
        let fetches = file_reads(&pack, blob, &diff.enter, COALESCE_GAP, READ_AT_MOST);
        let (mut fetch_ms, mut unpack_ms, mut decode_ms, mut upload_ms) = (0.0, 0.0, 0.0, 0.0);
        let mut fetched_bytes = 0usize;
        for fetch in &fetches {
            let tf = now_ms();
            let bytes = source.read_with(fetch.range.clone(), held).await?;
            fetched_bytes += bytes.len();
            let at = fetch.range.start - blob;

            let t0 = now_ms();
            let mut meshes = Vec::with_capacity(fetch.serves.len());
            let mut pngs = Vec::new();
            for &i in &fetch.serves {
                let tile = &diff.enter[i];
                meshes.push((
                    TileKey::of(tile),
                    Mesh::of_span(&pack, tile, at, &bytes).map_err(js)?,
                ));
                if let Some(png) = texture_of_span(&pack, tile, at, &bytes).map_err(js)? {
                    pngs.push((meshes.len() - 1, png));
                }
            }
            drop(bytes);

            let t1 = now_ms();
            // This read's textures decode at once, on the browser's threads.
            let (owners, futures): (Vec<usize>, Vec<_>) =
                pngs.into_iter().map(|(at, png)| (at, bitmap(png))).unzip();
            let bitmaps = try_join_all(futures)
                .await
                .map_err(|e| js(format!("image decode: {e:?}")))?;
            let mut textures: Vec<Option<wgpu::Texture>> =
                (0..meshes.len()).map(|_| None).collect();

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
            fetch_ms += t0 - tf;
            unpack_ms += t1 - t0;
            decode_ms += t2 - t1;
            upload_ms += t3 - t2;
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
        if let Some(buffer) = readback.as_ref() {
            gpu.encode_i420(&mut encoder).map_err(js)?;
            encoder.copy_buffer_to_buffer(gpu.i420_planes(), 0, buffer, 0, buffer.size());
        }
        gpu.queue().submit([encoder.finish()]);
        frame.present();
        // Released only now: the frame that stopped drawing them is recorded.
        for key in &diff.leave {
            gpu.leave(key);
        }
        // And the blocks no later frame reads.
        held.done_with(diff.frame);
        let t4 = now_ms();

        Ok(Some(FrameStats {
            frame: diff.frame,
            selected: diff.selection.len() as u32,
            entered: diff.enter.len() as u32,
            left: diff.leave.len() as u32,
            fetch_ms,
            fetched_bytes: fetched_bytes as f64,
            requests: fetches.len() as u32,
            unpack_ms,
            decode_ms,
            upload_ms,
            record_ms: t4 - t3,
        }))
    }
}

/// The page's side: one mp4 from every worker's encoded frames, in order.
#[wasm_bindgen]
pub struct FilmMuxer {
    inner: Option<Muxer>,
}

/// The codec an encoder's configuration record is for: an `av1C` starts with
/// its marker bit set, an `avcC` with its version, 1.
fn codec_of(config: &[u8]) -> Result<Codec, JsError> {
    match config.first() {
        Some(0x81) => Ok(Codec::Av1(config.to_vec())),
        _ => Ok(Codec::Avc(ParameterSets::from_avcc(config).map_err(js)?)),
    }
}

#[wasm_bindgen]
impl FilmMuxer {
    /// `config` is the first encoder's configuration record: the `avcC`
    /// description WebCodecs hands out for H.264, or an `av1C` for AV1.
    #[wasm_bindgen(constructor)]
    pub fn new(width: u16, height: u16, fps: u32, config: &[u8]) -> Result<FilmMuxer, JsError> {
        Ok(FilmMuxer {
            inner: Some(Muxer::with(width, height, fps, codec_of(config)?).map_err(js)?),
        })
    }

    /// Fails unless an encoder with this record can join the film.
    pub fn check(&self, config: &[u8]) -> Result<(), JsError> {
        self.muxer()?.check(&codec_of(config)?).map_err(js)
    }

    /// Appends frame `index` of the film (from 0), as its encoder emitted it.
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
