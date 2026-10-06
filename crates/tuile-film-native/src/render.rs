// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The render: a film's frames, one after the other.
//!
//! For each frame the pack says which tiles are drawn. A tile that was not
//! drawn by the frame before is built — from the tile store where the pack
//! refers to it and the store answers, from the pack's own payload
//! otherwise — and entered; the frame is drawn; tiles no longer drawn are
//! let go. Never a hole: a tile that can be built neither way stops the
//! render with its name.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use tuile_film::from_store::{compose, imagery_texture, is_baked, terrain_mesh};
use tuile_film::{
    refs_of, texture_of_span, Content, Cursor, FrameCamera, Look, Mesh, Pack, TileKey,
};
use tuile_film_gpu::{DrapeLayer, FilmGpu, Settings, TileMesh};
use tuile_radiometry::{apply_multipliers, LevelGains};
use tuile_repository::TileRepository;

use crate::observe::{FrameOut, ImageryIn, Observer, Origin, TileIn, Timings};
use crate::sink::Sink;
use crate::source::{Film, Sources};
use crate::Error;

/// The tone correction of a render.
#[derive(Debug, Clone, Default)]
pub enum Tone {
    /// The imagery as stored.
    #[default]
    Off,
    /// The table the store holds beside the imagery layer, if it holds one.
    OfTheStore,
    /// This table.
    Table(LevelGains),
}

/// What to render.
#[derive(Debug, Clone)]
pub struct Order {
    /// First and last frame, inclusive; `None` for the whole film.
    pub frames: Option<(u32, u32)>,
    /// One frame in this many.
    pub every: u32,
    /// The picture's size against the size the film was baked for.
    pub scale: f32,
    pub supersample: u32,
    pub fps: u32,
    pub tone: Tone,
    /// How much of the tone correction, 0 to 1.
    pub tone_strength: f32,
}

impl Default for Order {
    fn default() -> Self {
        Self {
            frames: None,
            every: 1,
            scale: 1.0,
            supersample: 2,
            fps: 30,
            tone: Tone::OfTheStore,
            tone_strength: 1.0,
        }
    }
}

/// What a render did.
#[derive(Debug, Clone, Default)]
pub struct Done {
    pub frames: u32,
    pub width: u32,
    pub height: u32,
    pub tiles_from_store: u64,
    pub tiles_from_pack: u64,
    /// Source tiles the store has renewed since the bake.
    pub renewed: u64,
    /// The first frame rendered, which enters every tile it draws…
    pub opening: Timings,
    /// …and every frame after it, together.
    pub timings: Timings,
    /// Tiles the first frame entered, and all the others together.
    pub opening_tiles: u64,
    pub seconds: f64,
    /// Before the first frame: opening the adapter and the sink.
    pub setup_seconds: f64,
    /// The tone table applied, if one was.
    pub tone: Option<LevelGains>,
}

/// Store tiles asked for at once.
const AT_ONCE: usize = 48;

/// Imagery tiles kept on the GPU before they are let go.
const TEXTURES_HELD: usize = 2048;

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

async fn read_back(film: &FilmGpu, buffer: &wgpu::Buffer) -> Result<Vec<u8>, Error> {
    let (done, mapped) = tokio::sync::oneshot::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = done.send(r);
    });
    film.device().poll(wgpu::PollType::wait_indefinitely())?;
    mapped.await??;
    let bytes = buffer.slice(..).get_mapped_range().to_vec();
    buffer.unmap();
    Ok(bytes)
}

/// Renders `film` as `order` says, each picture to `sink`, `observer` told
/// of everything.
pub async fn render(
    sources: &Sources,
    film: &Film,
    order: &Order,
    sink: &mut dyn Sink,
    observer: &mut dyn Observer,
) -> Result<Done, Error> {
    let began = Instant::now();
    let (film_first, film_last) = film.frames();
    let (first, last) = order.frames.unwrap_or((film_first, film_last));
    if first < film_first || last > film_last || first > last {
        return Err(format!(
            "frames {first}–{last} are not within the film's {film_first}–{film_last}"
        )
        .into());
    }
    let every = order.every.max(1);

    // The picture's size, from the first frame's view.
    let opening = film
        .packs
        .iter()
        .find(|p| (p.first..=p.last).contains(&first))
        .ok_or("no pack holds the first frame")?;
    let view = Pack::open_table(&opening.head)?.view_of(first)?;
    let width = (((view.viewport_px[0] as f32 * order.scale) as u32) / 8).max(1) * 8;
    let height = (((view.viewport_px[1] as f32 * order.scale) as u32) / 8).max(1) * 8;

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = instance.request_adapter(&Default::default()).await?;
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            required_limits: adapter.limits(),
            ..Default::default()
        })
        .await?;
    let mut gpu = FilmGpu::new(
        device,
        queue,
        Settings {
            width,
            height,
            supersample: order.supersample.max(1),
            look: Look::default(),
        },
    );
    let padded = (width * 4).div_ceil(256) * 256;
    let picture = gpu.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("film picture readback"),
        size: u64::from(padded * height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let planes = sink.wants_i420().then(|| {
        gpu.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("film i420 readback"),
            size: gpu.i420_planes().size(),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        })
    });
    sink.open(width, height, order.fps)?;

    let mut done = Done {
        width,
        height,
        setup_seconds: began.elapsed().as_secs_f64(),
        ..Done::default()
    };
    // What is resident, and the imagery levels of each one's drape.
    let mut resident: HashMap<TileKey, Vec<u8>> = HashMap::new();
    let mut textures: HashMap<(u8, u32, u32), (wgpu::Texture, bool)> = HashMap::new();
    let mut tone: Option<(String, Option<LevelGains>)> = None;
    let mut index = 0u32;

    for file in &film.packs {
        let (a, b) = (file.first.max(first), file.last.min(last));
        if a > b {
            continue;
        }
        let pack = Pack::open_table(&file.head)?;
        let blobs = file.head.len() as u64;
        // References the store answers are read from the store; anything
        // else is the pack's own payload.
        let layers = match pack.content() {
            Content::Embedded => None,
            _ => pack.store_layers(),
        };
        let scheme = layers.map(|(_, imagery)| sources.store.scheme_of(imagery));
        if let Some((_, imagery)) = layers {
            if tone.as_ref().is_none_or(|(layer, _)| layer != imagery) {
                let table = match &order.tone {
                    Tone::Off => None,
                    Tone::Table(table) => Some(table.clone()),
                    Tone::OfTheStore => sources.store.tone_of(imagery).await?,
                };
                done.tone.clone_from(&table);
                tone = Some((imagery.to_string(), table));
            }
        }
        let gain = |level: u8| {
            tone.as_ref()
                .and_then(|(_, table)| table.as_ref())
                .map_or([1.0; 3], |t| t.multipliers(level, order.tone_strength))
        };

        // Frames a, a + every, …: each from a cursor of its own when frames
        // are skipped, since a diff is against the frame before.
        let wanted: Vec<u32> = (a..=b).filter(|f| (f - first) % every == 0).collect();
        let mut walking = (every == 1).then(|| Cursor::new(&pack, a, b)).transpose()?;
        for frame in wanted {
            let diff = match walking.as_mut() {
                Some(cursor) => cursor.advance(&pack),
                None => Cursor::new(&pack, frame, frame)?.advance(&pack),
            }
            .ok_or("the pack ended before its last frame")??;
            let mut timings = Timings::default();
            let mut entered = 0usize;

            // Everything the entering tiles read from the store, asked for
            // at once: a frame's reads wait on one another for nothing.
            let (mut terrain_read, mut imagery_read) = (HashMap::new(), HashMap::new());
            if let Some((terrain_layer, imagery_layer)) = layers {
                let (mut terrain_wanted, mut imagery_wanted) = (HashSet::new(), HashSet::new());
                for tile in &diff.enter {
                    if resident.contains_key(&TileKey::of(tile)) {
                        continue;
                    }
                    let Some(refs) = refs_of(tile) else { continue };
                    terrain_wanted.insert((refs.terrain.level, refs.terrain.x, refs.terrain.y));
                    for placed in &refs.imagery {
                        let at = (placed.tile.level, placed.tile.x, placed.tile.y);
                        if !textures.contains_key(&at) {
                            imagery_wanted.insert(at);
                        }
                    }
                }
                let store = &sources.store.tiles;
                for (layer, wanted, read, spent) in [
                    (
                        terrain_layer,
                        terrain_wanted,
                        &mut terrain_read,
                        &mut timings.read_terrain,
                    ),
                    (
                        imagery_layer,
                        imagery_wanted,
                        &mut imagery_read,
                        &mut timings.read_imagery,
                    ),
                ] {
                    let t = Instant::now();
                    let wanted: Vec<(u8, u32, u32)> = wanted.into_iter().collect();
                    for some in wanted.chunks(AT_ONCE) {
                        let found = futures_util::future::join_all(
                            some.iter().map(|at| store.tile(layer, at.0, at.1, at.2)),
                        )
                        .await;
                        for (at, tile) in some.iter().zip(found) {
                            if let Some(tile) = tile? {
                                read.insert(*at, tile.bytes);
                            }
                        }
                    }
                    *spent += ms(t);
                }
            }

            for tile in &diff.enter {
                let key = TileKey::of(tile);
                if resident.contains_key(&key) {
                    continue;
                }
                let factor = match tile.base_color_factor() {
                    Some(v) if v.len() == 4 => [v.get(0), v.get(1), v.get(2), v.get(3)],
                    _ => [1.0; 4],
                };
                let refs = layers.and_then(|_| refs_of(tile));
                let (mesh, texture, origin, imagery_of) = match (refs, layers, scheme.as_ref()) {
                    (Some(refs), Some((terrain_layer, imagery_layer)), Some(scheme)) => {
                        let source = refs.terrain;
                        // Shared by the tiles cut from one ancestor.
                        let terrain = terrain_read
                            .get(&(source.level, source.x, source.y))
                            .ok_or_else(|| {
                                format!(
                                    "terrain {}/{}/{} of layer {terrain_layer} is no longer in the tile store",
                                    source.level, source.x, source.y
                                )
                            })?;
                        if !is_baked(&source, terrain) {
                            done.renewed += 1;
                        }
                        let t = Instant::now();
                        let mesh = terrain_mesh(tile.id(), &refs, terrain, factor)?;
                        timings.mesh += ms(t);

                        let mut drape = Vec::with_capacity(refs.imagery.len());
                        let mut translucent = false;
                        for placed in &refs.imagery {
                            let at = (placed.tile.level, placed.tile.x, placed.tile.y);
                            if !textures.contains_key(&at) {
                                let found = imagery_read.get(&at).ok_or_else(|| {
                                    format!(
                                        "imagery {}/{}/{} of layer {imagery_layer} is no longer in the tile store",
                                        at.0, at.1, at.2
                                    )
                                })?;
                                let renewed = !is_baked(&placed.tile, found);
                                done.renewed += u64::from(renewed);
                                let t = Instant::now();
                                observer.imagery(&ImageryIn {
                                    level: at.0,
                                    x: at.1,
                                    y: at.2,
                                    bytes: found,
                                    renewed,
                                    gain: gain(at.0),
                                });
                                timings.observe += ms(t);
                                let t = Instant::now();
                                let decoded = imagery_texture(&placed.tile, scheme, found)?;
                                let opaque = decoded.rgba8.chunks_exact(4).all(|p| p[3] == 255);
                                timings.decode += ms(t);
                                let t = Instant::now();
                                let texture = gpu.create_imagery(decoded.width, decoded.height);
                                gpu.write_rgba(&texture, &decoded.rgba8);
                                timings.upload += ms(t);
                                if textures.len() >= TEXTURES_HELD {
                                    textures.clear();
                                }
                                textures.insert(at, (texture, opaque));
                            }
                            let (texture, opaque) = &textures[&at];
                            translucent |= !opaque;
                            drape.push((placed, texture.clone()));
                        }
                        let imagery_of: Vec<(u8, u32, u32)> = refs
                            .imagery
                            .iter()
                            .map(|p| (p.tile.level, p.tile.x, p.tile.y))
                            .collect();
                        let t = Instant::now();
                        let texture = if drape.is_empty() {
                            None
                        } else if translucent {
                            // A translucent layer blends with what is under
                            // it, which the GPU's composition cannot read:
                            // that drape is composed here, exactly.
                            let mut decoded = HashMap::new();
                            for placed in &refs.imagery {
                                let at = (placed.tile.level, placed.tile.x, placed.tile.y);
                                let found = sources
                                    .store
                                    .tiles
                                    .tile(imagery_layer, at.0, at.1, at.2)
                                    .await?
                                    .ok_or("an imagery tile left the store mid-frame")?;
                                let mut texels =
                                    imagery_texture(&placed.tile, scheme, &found.bytes)?;
                                apply_multipliers(&mut texels.rgba8, gain(at.0));
                                decoded.insert(at, std::sync::Arc::new(texels));
                            }
                            compose(&refs, factor, |t| decoded[&(t.level, t.x, t.y)].clone()).map(
                                |texels| {
                                    let texture = gpu.create_albedo(texels.width, texels.height);
                                    gpu.write_rgba(&texture, &texels.rgba8);
                                    texture
                                },
                            )
                        } else {
                            let side = refs.composed_side.max(1);
                            let albedo = gpu.create_albedo(side, side);
                            let layers = drape
                                .into_iter()
                                .map(|(placed, texture)| DrapeLayer {
                                    texture,
                                    coverage: placed.coverage,
                                    translation: placed.translation,
                                    scale: placed.scale,
                                    gain: gain(placed.tile.level),
                                })
                                .collect();
                            gpu.compose(&albedo, factor, layers);
                            Some(albedo)
                        };
                        timings.enter += ms(t);
                        done.tiles_from_store += 1;
                        (mesh, texture, Origin::Store, imagery_of)
                    }
                    _ => {
                        let span = pack.span_of(tile).ok_or_else(|| {
                            format!(
                                "tile {} is neither in the tile store nor in the pack",
                                tile.id()
                            )
                        })?;
                        let t = Instant::now();
                        let part = sources
                            .packs
                            .objects
                            .read(&file.key, blobs + span.start..blobs + span.end)
                            .await?;
                        timings.read_pack += ms(t);
                        let t = Instant::now();
                        let mesh = Mesh::of_span(&pack, tile, span.start, &part)?;
                        let texture = match texture_of_span(&pack, tile, span.start, &part)? {
                            Some(png) => {
                                let rgba = image::load_from_memory(&png)?.to_rgba8();
                                let texture = gpu.create_albedo(rgba.width(), rgba.height());
                                gpu.write_rgba(&texture, &rgba);
                                Some(texture)
                            }
                            None => None,
                        };
                        timings.decode += ms(t);
                        done.tiles_from_pack += 1;
                        (mesh, texture, Origin::Pack, Vec::new())
                    }
                };
                let t = Instant::now();
                observer.tile(&TileIn {
                    frame: diff.frame,
                    key,
                    origin,
                    imagery: &imagery_of,
                });
                timings.observe += ms(t);
                let t = Instant::now();
                gpu.enter(
                    key,
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
                )?;
                timings.enter += ms(t);
                resident.insert(key, imagery_of.iter().map(|at| at.0).collect());
                entered += 1;
            }

            let t = Instant::now();
            let camera = FrameCamera::of(&diff.view, width as f32 / height as f32);
            let mut encoder = gpu.render(&camera, &diff.selection)?;
            encoder.copy_texture_to_buffer(
                gpu.output().as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &picture,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded),
                        rows_per_image: Some(height),
                    },
                },
                gpu.output().size(),
            );
            if let Some(planes) = &planes {
                gpu.encode_i420(&mut encoder)?;
                encoder.copy_buffer_to_buffer(gpu.i420_planes(), 0, planes, 0, planes.size());
            }
            gpu.queue().submit([encoder.finish()]);
            timings.draw += ms(t);
            let t = Instant::now();
            let rows = read_back(&gpu, &picture).await?;
            let mut rgba = Vec::with_capacity((width * height * 4) as usize);
            for row in rows.chunks(padded as usize) {
                rgba.extend_from_slice(&row[..(width * 4) as usize]);
            }
            let i420 = match &planes {
                Some(planes) => read_back(&gpu, planes).await?,
                None => Vec::new(),
            };
            timings.readback += ms(t);
            let t = Instant::now();

            // Let go only once the frame that no longer draws them is drawn.
            let drawn: HashSet<TileKey> = diff.selection.iter().copied().collect();
            let gone: Vec<TileKey> = resident
                .keys()
                .filter(|key| !drawn.contains(key))
                .copied()
                .collect();
            for key in gone {
                gpu.leave(&key);
                resident.remove(&key);
            }

            timings.leave += ms(t);

            let mut layers_drawn: BTreeMap<u8, u32> = BTreeMap::new();
            for level in resident.values().flatten() {
                *layers_drawn.entry(*level).or_default() += 1;
            }
            let layers: Vec<(u8, u32)> = layers_drawn.into_iter().collect();
            let t = Instant::now();
            observer.frame(&FrameOut {
                frame: diff.frame,
                index,
                width,
                height,
                rgba: &rgba,
                tiles: diff.selection.len(),
                entered,
                layers: &layers,
                timings,
            });
            timings.observe += ms(t);
            let t = Instant::now();
            sink.picture(index, diff.frame, &rgba, &i420)?;
            timings.sink += ms(t);
            if index == 0 {
                done.opening = timings;
                done.opening_tiles = entered as u64;
            } else {
                done.timings.add(&timings);
            }
            index += 1;
            if index % 200 == 0 {
                let so_far = began.elapsed().as_secs_f64();
                eprintln!(
                    "  frame {} — {index} rendered in {so_far:.0} s ({:.1} frames/s)",
                    diff.frame,
                    f64::from(index) / so_far
                );
            }
        }
    }
    let t = Instant::now();
    sink.close()?;
    done.timings.sink += ms(t);
    done.frames = index;
    done.seconds = began.elapsed().as_secs_f64();
    Ok(done)
}
