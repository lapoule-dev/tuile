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
//!
//! Beside the ground, a frame shows what the host's [`Overlays`] hand it:
//! shapes placed in the world, asked for frame by frame and drawn in the
//! same scene.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use glam::DVec3;
use tuile_film::drape::{on_screen_px, spared, sphere};
use tuile_film::from_store::{compose, imagery_texture, is_baked, terrain_mesh};
use tuile_film::smoothing::Smooth;
use tuile_film::stitching::Stitching;
use tuile_film::{
    refs_of, texture_of_span, Content, Cursor, Eye, FrameCamera, FrameLight, Look, Mesh,
    OverlayCamera, OverlayMesh, Overlays, Pack, Stereo, TileKey,
};
use tuile_film_gpu::{
    DrapeLayer, FilmGpu, LayerCorner, LayerField, LayerGrade, Settings, Shot, TileMesh,
};
use tuile_farm::receipt::{Ahead, GpuPass, GpuTimes};
use tuile_radiometry::{linear_of, through, Affine, Blended, FilmGrade, Local, MatrixField, SAME};
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
    /// The film's own grade, kept beside its packs; nothing if it has
    /// none.
    OfTheFilm,
    /// This grade.
    Grade(Box<FilmGrade>),
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
    /// How the film is lit, exposed, and its imagery read.
    pub look: Look,
    /// Whether the tiles of each frame are stitched to one another
    /// (`tuile_core::stitch`): on, unless something is being compared.
    pub stitch: bool,
    /// Whether tiles keep their skirts. Off is a probe: it shows what
    /// stitching closes by itself.
    pub skirts: bool,
    /// A probe: paint magenta what shows through a gap in the ground.
    pub holes: bool,
    /// PROTOTYPE (terrain smoothing): how the ground is smoothed, how much
    /// of a vertex's normal a face takes (0 to 1), and whether the mesh is
    /// drawn as lines.
    pub smooth: Smooth,
    pub smooth_strength: f32,
    pub wire: bool,
    /// Two eyes, packed into one picture: see [`tuile_film::Stereo`]. Each
    /// frame is drawn twice, from the eyes its camera gives, out of the
    /// tiles the pack selected for that camera.
    pub stereo: Option<Stereo>,
    /// The instant each frame shows ([`tuile_tape::Clock`]),
    /// when a frame's host does not place its light: the frame is then lit
    /// by the sun of its instant, the Moon of its instant over the frame's
    /// own eye, and the stars turned to it. `None`: the look's light.
    pub clock: Option<tuile_tape::Clock>,
}

/// The light of `frame` under `clock`: the sun of its instant from the
/// Earth's centre (a planet's radius at an astronomical unit is 0.0025°),
/// the instant itself for the renderer to place the Moon and the stars from
/// the frame's own eye, and the rest the look's.
pub fn clocked_light(clock: &tuile_tape::Clock, frame: u32, look: &Look) -> FrameLight {
    let instant = clock.instant(frame);
    let mut light = FrameLight::of(look);
    let to_sun = tuile_atmosphere::Sun::at_unix_seconds(instant).direction_ecef;
    if to_sun.is_finite() {
        light.illumination.to_sun_ecef = to_sun;
    }
    light.with_instant(instant)
}

/// The light frame `frame` is lit by — one path, in this order:
///
/// 1. the host's, when it places it ([`tuile_film::Overlays::light`]): a
///    host's plan is the record of its film;
/// 2. else the instant the film's clock gives the frame ([`clocked_light`]):
///    a tape that states its instants is lit by them;
/// 3. else the look's own, as ordered.
pub fn frame_light(
    host: Option<FrameLight>,
    clock: Option<&tuile_tape::Clock>,
    frame: u32,
    look: &Look,
) -> FrameLight {
    host.or_else(|| clock.map(|c| clocked_light(c, frame, look)))
        .unwrap_or_else(|| FrameLight::of(look))
}

impl Default for Order {
    fn default() -> Self {
        Self {
            frames: None,
            every: 1,
            scale: 1.0,
            supersample: 2,
            fps: 30,
            tone: Tone::OfTheFilm,
            tone_strength: 1.0,
            look: Look::default(),
            stitch: true,
            skirts: true,
            holes: false,
            smooth: Smooth::Off,
            smooth_strength: 1.0,
            wire: false,
            stereo: None,
            clock: None,
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
    /// The film's grade as applied, if one was.
    pub grade: Option<FilmGrade>,
    /// Tiles stitched anew over the frames rendered, those of them whose
    /// mesh was uploaded again, the most knots a tile held, and the bytes of
    /// strips uploaded.
    pub restitched: u64,
    pub resplit: u64,
    pub most_knots: usize,
    pub strip_bytes: u64,
    /// Frames whose selection was planned — those that draw other tiles
    /// than the frame before — and the milliseconds spent planning them
    /// (the engine's rule, and the meshes that gain vertices cut again),
    /// apart from handing the result to the GPU.
    pub planned: u64,
    pub plan_ms: f64,
    /// With the probe of holes: the pixels a hole touches, over the frames
    /// rendered.
    pub hole_pixels: u64,
    /// PROTOTYPE (terrain smoothing): tiles that came with normals of their
    /// own, triangles before and after, the furthest a vertex was put from
    /// the source's surface (metres) and the sum of the tiles' means.
    pub with_normals: u64,
    pub triangles: (u64, u64),
    pub smooth_furthest: f32,
    pub smooth_mean_sum: f64,
    pub smoothed_tiles: u64,
    pub smooth_ms: f64,
    /// With shadows: the finest and the coarsest a texel of the sun's map
    /// was on the ground over the frames rendered, in metres.
    pub shadow_texel: Option<(f32, f32)>,
    /// Tiles entered with less than their whole drape, being far; and tiles
    /// entered again with more of it, having come closer.
    pub drapes_reduced: u64,
    pub drapes_regrown: u64,
    /// Every store tile of each pack's frames asked for once, before them.
    pub ahead: Ahead,
    /// The card's own clock on each kind of pass, where the adapter has
    /// timestamps.
    pub gpu: GpuTimes,
    /// The adapter, as its driver names it.
    pub adapter: String,
}

/// Tiles a stretch of frames names that the tile store does not hold. A
/// render that meets it draws nothing; trying again gives the same until
/// the store has them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbsentTiles {
    /// The frames whose tiles were asked for.
    pub frames: (u32, u32),
    /// Each one's layer, then its level, x and y.
    pub tiles: Vec<(String, (u8, u32, u32))>,
}

impl std::fmt::Display for AbsentTiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "frames {}–{} name {} tiles that are not in the tile store",
            self.frames.0,
            self.frames.1,
            self.tiles.len()
        )?;
        if let Some((layer, at)) = self.tiles.first() {
            write!(f, ", {layer} {}/{}/{} the first of them", at.0, at.1, at.2)?;
        }
        Ok(())
    }
}

impl std::error::Error for AbsentTiles {}

/// Store tiles asked for at once.
const AT_ONCE: usize = 48;

/// Imagery tiles kept on the GPU before they are let go.
const TEXTURES_HELD: usize = 2048;

/// Tiles entered before what they queued — drapes to compose, mips to make —
/// is handed to the GPU and waited for. A frame that enters ten thousand
/// tiles otherwise holds every imagery tile and every whole drape of all of
/// them at once, until it is drawn.
const ENTERED_AT_ONCE: usize = 256;

/// A drape is kept for this many times the pixels its tile spans, so that a
/// tile coming closer is not composed again at every step.
const DRAPE_HEADROOM: f64 = 1.5;

/// A tile as it is resident.
struct Held {
    /// The imagery levels of its drape.
    levels: Vec<u8>,
    /// Its drape's side as baked, and the levels of it not kept; no side
    /// for a drape that is kept whole whatever the distance.
    side: u32,
    spared: u32,
    /// The sphere around its mesh.
    centre: DVec3,
    radius: f64,
}

/// A tile's corners as the composition is handed them, each taken at
/// `strength`: every number of a corner is one that blends, so a part of
/// it is that part of each.
/// The night lights of the store's layer `layer`, put together whole from
/// the tiles it holds at [`tuile_bake::open::NIGHT_LEVEL`]: a tile it does
/// not hold is one no camera of the film could see, and stays dark.
pub async fn night_lights_of(
    store: &tuile_repository::ArchivedTiles,
    layer: &str,
) -> Result<tuile_bake::open::NightLights, Error> {
    let level = tuile_bake::open::NIGHT_LEVEL as u8;
    let (across, down) = tuile_bake::open::night_grid();
    let all: Vec<(u8, u32, u32)> = (0..down)
        .flat_map(|y| (0..across).map(move |x| (level, x, y)))
        .collect();
    store.open_zones(layer, all.iter().copied()).await?;
    let mut held = HashMap::new();
    for (_, x, y) in all {
        if let Some(tile) = store.tile(layer, level, x, y).await? {
            held.insert((x, y), tile.bytes);
        }
    }
    if held.is_empty() {
        return Err(format!("the night layer {layer} holds no tile at level {level}").into());
    }
    Ok(tuile_bake::open::NightLights::from_tiles(|x, y| {
        held.remove(&(x, y))
    })?)
}

fn layer_field(corners: &[Blended; 4], strength: f32) -> LayerField {
    LayerField {
        corners: corners.map(|c| LayerCorner {
            gain_stops: c.gain_stops.map(|v| v * strength),
            black: c.black.map(|v| v * strength),
            contrast_stops: c.contrast_stops * strength,
            pivot_stops: c.pivot_stops,
            saturation_stops: c.saturation_stops * strength,
            curve: c.curve.map(|channel| channel.map(|v| v * strength)),
        }),
        matrices: None,
    }
}

/// A tile's corner matrices as the composition is handed them, each taken
/// at `strength`: that part of the way from changing nothing.
fn layer_matrices(corners: &[Affine; 4], strength: f32) -> LayerField {
    LayerField {
        corners: [LayerCorner::IDENTITY; 4],
        matrices: Some(corners.map(|matrix| {
            std::array::from_fn(|o| {
                std::array::from_fn(|i| SAME[o][i] + (matrix[o][i] - SAME[o][i]) * strength)
            })
        })),
    }
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

/// Where one picture of a frame is read back to: its RGBA rows, padded to
/// what a copy wants, and its I420 planes when the sink takes them.
struct Readback {
    picture: wgpu::Buffer,
    planes: Option<wgpu::Buffer>,
    width: u32,
    height: u32,
    padded: u32,
}

/// One picture of the frame, drawn and read back: its RGBA, and its I420
/// when the sink takes it.
async fn shoot(
    gpu: &mut FilmGpu,
    shot: &Shot,
    selection: &[TileKey],
    shapes: &[OverlayMesh],
    back: &Readback,
    timings: &mut Timings,
    shadow_texel: &mut Option<(f32, f32)>,
) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let t = Instant::now();
    let mut encoder = gpu.render_shot(shot, selection, shapes)?;
    encoder.copy_texture_to_buffer(
        gpu.output().as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &back.picture,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(back.padded),
                rows_per_image: Some(back.height),
            },
        },
        gpu.output().size(),
    );
    if let Some(planes) = &back.planes {
        gpu.encode_i420(&mut encoder)?;
        encoder.copy_buffer_to_buffer(gpu.i420_planes(), 0, planes, 0, planes.size());
    }
    gpu.submit(encoder);
    if let Some(texel) = gpu.shadow_texel() {
        let (fine, coarse) = shadow_texel.unwrap_or((texel, texel));
        *shadow_texel = Some((fine.min(texel), coarse.max(texel)));
    }
    timings.draw += ms(t);
    let t = Instant::now();
    let rows = read_back(gpu, &back.picture).await?;
    let mut rgba = Vec::with_capacity((back.width * back.height * 4) as usize);
    for row in rows.chunks(back.padded as usize) {
        rgba.extend_from_slice(&row[..(back.width * 4) as usize]);
    }
    let i420 = match &back.planes {
        Some(planes) => read_back(gpu, planes).await?,
        None => Vec::new(),
    };
    timings.readback += ms(t);
    Ok((rgba, i420))
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
/// of everything. `overlays` is asked, once a frame — for both eyes of a
/// stereo film — for what the frame shows beside the ground and for the
/// light it is lit by ([`Overlays::light`]); `&mut ()` has nothing to show
/// and leaves the light to `order.look`.
pub async fn render(
    sources: &Sources,
    film: &Film,
    order: &Order,
    overlays: &mut dyn Overlays,
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
    // The film's framing, whatever share of the picture an eye is given.
    let aspect = width as f32 / height as f32;
    // For two eyes, `width × height` is an eye's, and the picture holds both.
    let stereo = order.stereo;
    if let Some(stereo) = &stereo {
        stereo.check()?;
    }
    let (width, height) = stereo.map_or((width, height), |s| s.eye_size(width, height));
    let (picture_width, picture_height) =
        stereo.map_or((width, height), |s| s.picture_size((width, height)));

    // The film's grade, before anything is set up: one from its first
    // frame to its last, and part of it is how the picture is exposed.
    let film_grade = match &order.tone {
        Tone::Off => None,
        Tone::Grade(grade) => Some(FilmGrade::clone(grade)),
        Tone::OfTheFilm => sources.film_grade(film).await?,
    };
    let strength = order.tone_strength.clamp(0.0, 1.0);
    let mut look = order.look;
    if let Some(grade) = &film_grade {
        look.exposure_ev += grade.exposure_ev * strength;
        look.contrast *= grade.contrast.powf(strength);
        look.saturation *= grade.saturation.powf(strength);
    }

    // The environment may say which: `WGPU_BACKEND=vulkan` on a card that
    // offers several, `WGPU_ADAPTER_NAME` on a machine with several cards.
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter = wgpu::util::initialize_adapter_from_env_or_default(&instance, None).await?;
    let info = adapter.get_info();
    let adapter_name = format!(
        "{} ({:?}, driver {} {})",
        info.name, info.backend, info.driver, info.driver_info
    );
    // Timestamps where the adapter writes them: every pass is then timed
    // by the card's own clock (`FilmGpu::pass_times`).
    let timestamps = adapter.features() & wgpu::Features::TIMESTAMP_QUERY;
    println!(
        "adapter: {adapter_name}; timestamps {}",
        if timestamps.is_empty() { "none" } else { "on" }
    );
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            required_features: timestamps,
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
            look,
        },
    );
    // Towns lit at night, under a physical atmosphere: the layer of the
    // tile store the film's packs name, its tiles read as any of the
    // store's, put together whole.
    let night_layer = match film.packs.first() {
        Some(file) => Pack::open_table(&file.head)?
            .night_layer()
            .map(str::to_string),
        None => None,
    };
    if let (Some(layer), Some(_)) = (night_layer, look.atmosphere) {
        let lights = night_lights_of(&sources.store.tiles, &layer).await?;
        gpu.set_night_lights(lights.width, lights.height, &lights.luma);
    }
    gpu.probe_holes(order.holes);
    gpu.smoothing_probe(order.smooth_strength, order.wire);
    let mut stitching = order.stitch.then(Stitching::default);
    let padded = (width * 4).div_ceil(256) * 256;
    let back = Readback {
        picture: gpu.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("film picture readback"),
            size: u64::from(padded * height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        }),
        planes: sink.wants_i420().then(|| {
            gpu.device().create_buffer(&wgpu::BufferDescriptor {
                label: Some("film i420 readback"),
                size: gpu.i420_planes().size(),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            })
        }),
        width,
        height,
        padded,
    };
    match stereo {
        None => sink.open(picture_width, picture_height, order.fps)?,
        Some(_) => sink.open_stereo((picture_width, picture_height), (width, height), order.fps)?,
    }

    let mut done = Done {
        width: picture_width,
        height: picture_height,
        grade: film_grade.clone(),
        setup_seconds: began.elapsed().as_secs_f64(),
        adapter: adapter_name,
        ..Done::default()
    };
    // What is resident, and the imagery levels of each one's drape.
    let mut resident: HashMap<TileKey, Held> = HashMap::new();
    let mut textures: HashMap<(u8, u32, u32), (wgpu::Texture, bool)> = HashMap::new();
    // The frame's overlays, the same allocation from one frame to the next.
    let mut shapes = Vec::new();
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
        // Before this pack's first frame: every tile of the store its frames
        // read is asked for once, so the archives they lie in are fetched
        // ahead — each once — and no frame waits on a network. The bytes
        // are let go: what keeps them is below, with the blocks.
        if let Some((terrain_layer, imagery_layer)) = layers {
            let ahead = Instant::now();
            let (terrain, imagery) = tuile_film::referred(&pack, a, b)?;
            let (count, store) = (terrain.len() + imagery.len(), &sources.store.tiles);
            // What the film names and the store does not hold: all of it is
            // said before a frame is drawn, not the first of it when a frame
            // comes to need it.
            let mut absent = Vec::new();
            for (layer, wanted) in [(terrain_layer, terrain), (imagery_layer, imagery)] {
                let wanted: Vec<tuile_film::StoreAt> = wanted.into_iter().collect();
                // A layer the frames read nothing of — a ground with no
                // imagery names none — is not asked.
                if wanted.is_empty() {
                    continue;
                }
                // Their zones' manifests first, all in one exchange with
                // the store: several hundred of them, a kilobyte each, are
                // not worth a round trip apiece.
                store.open_zones(layer, wanted.iter().copied()).await?;
                for some in wanted.chunks(AT_ONCE) {
                    let found = futures_util::future::join_all(
                        some.iter().map(|at| store.tile(layer, at.0, at.1, at.2)),
                    )
                    .await;
                    for (at, tile) in some.iter().zip(found) {
                        if tile?.is_none() {
                            absent.push((layer.to_string(), (at.0, at.1, at.2)));
                        }
                    }
                }
            }
            if !absent.is_empty() {
                return Err(Box::new(AbsentTiles {
                    frames: (a, b),
                    tiles: absent,
                }));
            }
            let seconds = ahead.elapsed().as_secs_f64();
            done.ahead.packs += 1;
            done.ahead.tiles += count as u64;
            done.ahead.seconds += seconds;
            println!("ahead of frames {a}–{b}: {count} tiles of the store asked for in {seconds:.1} s");
        }
        // A tile's own gain: the same wherever the film draws it.
        let corners = |at: (u8, u32, u32)| film_grade.as_ref().and_then(|g| g.field.corners(at));
        // …or the function a tile, for a film that has one: it is drawn
        // in the field's place.
        let matrices = |at: (u8, u32, u32)| {
            film_grade
                .as_ref()
                .and_then(|g| g.matrix.as_ref())
                .and_then(|m| m.given.get(&at))
                .map(|four| layer_matrices(four, strength))
        };
        // Where a drape is composed without the GPU: the field, texel by
        // texel.
        let fielded = |at: (u8, u32, u32), rgba8: &mut [u8], width: u32, height: u32| {
            if let Some(LayerField {
                matrices: Some(four),
                ..
            }) = matrices(at)
            {
                let field = MatrixField {
                    given: [(at, four)].into(),
                };
                for (n, texel) in rgba8.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    let (x, y) = (n as u32 % width, n as u32 / width);
                    let matrix = field.at(
                        at,
                        (x as f32 + 0.5) / width as f32,
                        (y as f32 + 0.5) / height as f32,
                    );
                    let lit = [0, 1, 2].map(|c| linear_of(texel[c]));
                    for (c, made) in through(&matrix, lit).iter().enumerate() {
                        let v = made.clamp(0.0, 1.0);
                        let stored = if v <= 0.003_130_8 {
                            v * 12.92
                        } else {
                            1.055 * v.powf(1.0 / 2.4) - 0.055
                        };
                        texel[c] = (stored * 255.0).round() as u8;
                    }
                }
                return;
            }
            let Some(corners) = corners(at) else {
                return;
            };
            let field = layer_field(&corners, strength);
            let linear: [f32; 256] = std::array::from_fn(|v| {
                let v = v as f32 / 255.0;
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            });
            let blended: [Blended; 4] = std::array::from_fn(|c| {
                let k = &field.corners[c];
                Blended {
                    gain_stops: k.gain_stops,
                    black: k.black,
                    contrast_stops: k.contrast_stops,
                    pivot_stops: k.pivot_stops,
                    saturation_stops: k.saturation_stops,
                    curve: k.curve,
                }
            });
            for (n, texel) in rgba8.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (x, y) = (n as u32 % width, n as u32 / width);
                let local: Local = Blended::mix(
                    &blended,
                    (x as f32 + 0.5) / width as f32,
                    (y as f32 + 0.5) / height as f32,
                )
                .local();
                let graded = local.apply([
                    linear[texel[0] as usize],
                    linear[texel[1] as usize],
                    linear[texel[2] as usize],
                ]);
                for c in 0..3 {
                    let v = graded[c].clamp(0.0, 1.0);
                    let stored = if v <= 0.003_130_8 {
                        v * 12.92
                    } else {
                        1.055 * v.powf(1.0 / 2.4) - 0.055
                    };
                    texel[c] = (stored * 255.0).round() as u8;
                }
            }
        };

        // Frames a, a + every, …: each from a cursor of its own when frames
        // are skipped, since a diff is against the frame before.
        let wanted: Vec<u32> = (a..=b).filter(|f| (f - first) % every == 0).collect();
        let mut walking = (every == 1).then(|| Cursor::new(&pack, a, b)).transpose()?;
        for frame in wanted {
            let t = Instant::now();
            let diff = match walking.as_mut() {
                Some(cursor) => cursor.advance(&pack),
                None => Cursor::new(&pack, frame, frame)?.advance(&pack),
            }
            .ok_or("the pack ended before its last frame")??;
            let mut timings = Timings {
                advance: ms(t),
                ..Timings::default()
            };
            let mut entered = 0usize;

            // The pixels a tile's sphere can span in this frame's picture, as
            // the raster samples it.
            let eye = DVec3::from_array(diff.view.position);
            let rows = f64::from(height * order.supersample.max(1));
            let span = |centre: DVec3, radius: f64| {
                on_screen_px(radius, (centre - eye).length(), rows, diff.view.fovy_rad)
            };

            // What this frame builds: the tiles it draws that the frame
            // before did not, and those come close enough to be read at a
            // level of their drape that was not kept. The latter are let go
            // and entered again before the frame is drawn, never between
            // two frames: nothing is drawn without them.
            let mut entering: Vec<tuile_film::fb::Tile<'_>> = diff
                .enter
                .iter()
                .filter(|tile| !resident.contains_key(&TileKey::of(tile)))
                .copied()
                .collect();
            let closer: HashSet<TileKey> = diff
                .selection
                .iter()
                .filter(|key| {
                    resident.get(key).is_some_and(|held| {
                        held.spared > 0
                            && spared(held.side, span(held.centre, held.radius)) < held.spared
                    })
                })
                .copied()
                .collect();
            if !closer.is_empty() {
                for tile in pack.frame(diff.frame)? {
                    let key = TileKey::of(&tile);
                    if closer.contains(&key) && resident.remove(&key).is_some() {
                        gpu.leave(&key);
                        if let Some(stitching) = stitching.as_mut() {
                            stitching.leave(&key);
                        }
                        entering.push(tile);
                        done.drapes_regrown += 1;
                    }
                }
            }
            let batches = entering.len().div_ceil(ENTERED_AT_ONCE);

            for batch in entering.chunks(ENTERED_AT_ONCE) {
                // Everything the batch reads from the store, asked for at once:
                // its reads wait on one another for nothing. Imagery is let go
                // between batches only: what a batch was told it need not read
                // must still be there when the batch composes with it.
                if textures.len() >= TEXTURES_HELD {
                    textures.clear();
                }
                let (mut terrain_read, mut imagery_read) = (HashMap::new(), HashMap::new());
                if let Some((terrain_layer, imagery_layer)) = layers {
                    let (mut terrain_wanted, mut imagery_wanted) = (HashSet::new(), HashSet::new());
                    for tile in batch {
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
                        if wanted.is_empty() {
                            continue;
                        }
                        // Held since the read ahead, and then nothing is asked;
                        // otherwise their zones' manifests come together.
                        store.open_zones(layer, wanted.iter().copied()).await?;
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

                for tile in batch {
                    let key = TileKey::of(tile);
                    let factor = match tile.base_color_factor() {
                        Some(v) if v.len() == 4 => [v.get(0), v.get(1), v.get(2), v.get(3)],
                        _ => [1.0; 4],
                    };
                    let refs = layers.and_then(|_| refs_of(tile));
                    let terrain_of = refs
                        .as_ref()
                        .map(|r| (r.terrain.level, r.terrain.x, r.terrain.y));
                    let (mesh, texture, origin, imagery_of, drape) = match (
                        refs,
                        layers,
                        scheme.as_ref(),
                    ) {
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
                                if let std::collections::hash_map::Entry::Vacant(e) =
                                    textures.entry(at)
                                {
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
                                        grade: Default::default(),
                                    });
                                    timings.observe += ms(t);
                                    let t = Instant::now();
                                    let decoded = imagery_texture(&placed.tile, scheme, found)?;
                                    let opaque = decoded
                                        .rgba8
                                        .as_chunks::<4>()
                                        .0
                                        .iter()
                                        .all(|p| p[3] == 255);
                                    timings.decode += ms(t);
                                    let t = Instant::now();
                                    let texture = gpu.create_imagery(decoded.width, decoded.height);
                                    gpu.write_rgba(&texture, &decoded.rgba8);
                                    timings.upload += ms(t);
                                    e.insert((texture, opaque));
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
                            let mut kept = (0, 0);
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
                                    let (w, h) = (texels.width, texels.height);
                                    fielded(at, &mut texels.rgba8, w, h);
                                    decoded.insert(at, std::sync::Arc::new(texels));
                                }
                                compose(&refs, factor, |t| decoded[&(t.level, t.x, t.y)].clone())
                                    .map(|texels| {
                                        let texture =
                                            gpu.create_albedo(texels.width, texels.height);
                                        gpu.write_rgba(&texture, &texels.rgba8);
                                        texture
                                    })
                            } else {
                                // Composed whole, as the bake composed it; kept
                                // from the first level this frame can read.
                                let side = refs.composed_side.max(1);
                                let (centre, radius) = sphere(mesh.origin_ecef, &mesh.positions);
                                let spare = spared(side, span(centre, radius) * DRAPE_HEADROOM);
                                kept = (side, spare);
                                let albedo = gpu.create_albedo(side, side);
                                let layers = drape
                                    .into_iter()
                                    .map(|(placed, texture)| DrapeLayer {
                                        texture,
                                        coverage: placed.coverage,
                                        translation: placed.translation,
                                        scale: placed.scale,
                                        grade: LayerGrade::IDENTITY,
                                        field: {
                                            let at =
                                                (placed.tile.level, placed.tile.x, placed.tile.y);
                                            matrices(at).or_else(|| {
                                                corners(at).map(|c| layer_field(&c, strength))
                                            })
                                        },
                                    })
                                    .collect();
                                gpu.compose(&albedo, factor, layers);
                                Some(if spare == 0 {
                                    albedo
                                } else {
                                    done.drapes_reduced += 1;
                                    gpu.tail(&albedo, spare)
                                })
                            };
                            timings.enter += ms(t);
                            done.tiles_from_store += 1;
                            (mesh, texture, Origin::Store, imagery_of, kept)
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
                            (mesh, texture, Origin::Pack, Vec::new(), (0, 0))
                        }
                    };
                    // PROTOTYPE (terrain smoothing), before anything else sees
                    // the mesh: stitching and the seam log are handed the
                    // smoothed one. The same preparation a camera's ground
                    // is read from (`tuile_film::ground`).
                    let mesh = {
                        let began = Instant::now();
                        let (out, did) = tuile_film::ground::prepared(
                            mesh,
                            tuile_film::ground::Prepare {
                                skirts: order.skirts,
                                smooth: order.smooth,
                            },
                        );
                        done.with_normals += u64::from(did.had_normals);
                        done.triangles.0 += did.triangles_before as u64;
                        done.triangles.1 += did.triangles_after as u64;
                        done.smooth_furthest = done.smooth_furthest.max(did.furthest);
                        done.smooth_mean_sum += f64::from(did.mean);
                        done.smoothed_tiles += 1;
                        done.smooth_ms += ms(began);
                        out
                    };
                    let t = Instant::now();
                    observer.tile(&TileIn {
                        frame: diff.frame,
                        key,
                        origin,
                        imagery: &imagery_of,
                        terrain: terrain_of,
                        mesh: &mesh,
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
                    if let Some(stitching) = stitching.as_mut() {
                        stitching.enter(key, terrain_of.map(|t| u32::from(t.0)), &mesh);
                    }
                    timings.enter += ms(t);
                    let (centre, radius) = sphere(mesh.origin_ecef, &mesh.positions);
                    resident.insert(
                        key,
                        Held {
                            levels: imagery_of.iter().map(|at| at.0).collect(),
                            side: drape.0,
                            spared: drape.1,
                            centre,
                            radius,
                        },
                    );
                    entered += 1;
                }
                // More than one batch: this one's drapes and mips are made now,
                // and what they were made from is let go before the next.
                if batches > 1 {
                    let t = Instant::now();
                    let mut made =
                        gpu.device()
                            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                                label: Some("film tiles entering"),
                            });
                    gpu.record_pending(&mut made);
                    gpu.queue().submit([made.finish()]);
                    gpu.device().poll(wgpu::PollType::wait_indefinitely())?;
                    timings.enter += ms(t);
                }
            }

            // The frame's tiles are put on one line where they meet, before
            // anything of it is drawn.
            let t = Instant::now();
            if let Some(stitching) = stitching.as_mut() {
                done.planned += u64::from(!stitching.is_planned(&diff.selection));
                let planning = Instant::now();
                let changed = stitching.frame(&diff.selection);
                done.plan_ms += ms(planning);
                for again in changed {
                    done.restitched += 1;
                    done.resplit += u64::from(again.mesh.is_some());
                    done.most_knots = done.most_knots.max(again.strips.len().saturating_sub(10));
                    done.strip_bytes += 16 * again.strips.len() as u64;
                    gpu.stitch(
                        &again.key,
                        again
                            .mesh
                            .as_ref()
                            .map(|mesh| TileMesh {
                                origin_ecef: mesh.origin_ecef,
                                positions: &mesh.positions,
                                normals: &mesh.normals,
                                uvs: &mesh.uvs,
                                indices: &mesh.indices,
                                index_count: mesh.index_count,
                                base_color_factor: mesh.base_color_factor,
                            })
                            .as_ref(),
                        &again.strips,
                        again.reach,
                    )?;
                }
            }
            timings.stitch += ms(t);
            let t = Instant::now();
            // The frame's light, before anything of the frame is drawn or
            // fitted — the sun's map included — and the same for both eyes:
            // the host's when it places it, else the order's.
            gpu.set_light(frame_light(
                overlays.light(diff.frame),
                order.clock.as_ref(),
                diff.frame,
                &look,
            ));
            let camera = FrameCamera::of(&diff.view, aspect);
            let pair = stereo.as_ref().map(|stereo| stereo.eyes(&camera));
            // The frame's overlays, once for every picture of it: made for
            // the frame's own camera, drawn into each eye through its own.
            shapes.clear();
            let seen = OverlayCamera {
                view: diff.view,
                camera,
                eyes: pair,
                shadow_texel: gpu.shadow_texel_for(&camera, &diff.selection)?,
            };
            overlays.frame(diff.frame, &seen, &mut shapes);
            timings.overlays += ms(t);
            // For two eyes: each eye apart, the pair, the left eye's camera.
            let mut apart = None;
            let (rgba, i420) = match (&stereo, &pair) {
                (None, _) | (_, None) => {
                    shoot(
                        &mut gpu,
                        &Shot::mono(camera),
                        &diff.selection,
                        &shapes,
                        &back,
                        &mut timings,
                        &mut done.shadow_texel,
                    )
                    .await?
                }
                // Both eyes from the same resident tiles, one after the
                // other through the same target, then packed.
                (Some(stereo), Some(pair)) => {
                    let mut eyes = Vec::with_capacity(2);
                    for eye in Eye::BOTH {
                        eyes.push(
                            shoot(
                                &mut gpu,
                                &Shot::eye(camera, pair, eye),
                                &diff.selection,
                                &shapes,
                                &back,
                                &mut timings,
                                &mut done.shadow_texel,
                            )
                            .await?,
                        );
                    }
                    let (left, right) = (&eyes[0], &eyes[1]);
                    let i420 = if back.planes.is_some() {
                        tuile_film::pack_i420(stereo.packing, &left.1, &right.1, width, height)
                    } else {
                        Vec::new()
                    };
                    let packed = (
                        tuile_film::pack_rgba(stereo.packing, &left.0, &right.0, width, height),
                        i420,
                    );
                    apart = Some((eyes, *pair, camera.eye(pair, Eye::Left)));
                    packed
                }
            };
            if order.holes {
                // The probe's magenta, or a part of it under the
                // supersampling filter.
                done.hole_pixels += rgba
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter(|p| {
                        let (r, g, b) = (i32::from(p[0]), i32::from(p[1]), i32::from(p[2]));
                        r > g + 60 && b > g + 60
                    })
                    .count() as u64;
            }
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
                if let Some(stitching) = stitching.as_mut() {
                    stitching.leave(&key);
                }
                resident.remove(&key);
            }

            timings.leave += ms(t);

            let mut layers_drawn: BTreeMap<u8, u32> = BTreeMap::new();
            for level in resident.values().flat_map(|held| &held.levels) {
                *layers_drawn.entry(*level).or_default() += 1;
            }
            let layers: Vec<(u8, u32)> = layers_drawn.into_iter().collect();
            let t = Instant::now();
            observer.frame(&FrameOut {
                frame: diff.frame,
                index,
                width: picture_width,
                height: picture_height,
                rgba: &rgba,
                tiles: diff.selection.len(),
                entered,
                layers: &layers,
                selection: &diff.selection,
                camera: &camera,
                timings,
            });
            timings.observe += ms(t);
            let t = Instant::now();
            match &apart {
                None => sink.picture(index, diff.frame, &rgba, &i420)?,
                Some((eyes, pair, left_camera)) => sink.stereo(
                    index,
                    diff.frame,
                    &rgba,
                    &i420,
                    &crate::sink::Eyes {
                        left: &eyes[0].1,
                        right: &eyes[1].1,
                        pair,
                        left_camera,
                    },
                )?,
            }
            timings.sink += ms(t);
            if index == 0 {
                done.opening = timings;
                done.opening_tiles = entered as u64;
            } else {
                done.timings.add(&timings);
            }
            index += 1;
            if index.is_multiple_of(200) {
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
    // What the card's clock said of every frame, the last ones waited for.
    done.gpu = GpuTimes {
        timestamps: gpu.timestamps(),
        passes: gpu
            .pass_times()?
            .into_iter()
            .map(|(pass, time)| GpuPass {
                pass,
                seconds: time.seconds,
                count: time.count,
            })
            .collect(),
    };
    done.frames = index;
    done.seconds = began.elapsed().as_secs_f64();
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hosts_light_wins_then_the_clock_then_the_look() {
        let look = Look::default();
        let clock = tuile_tape::Clock {
            start_utc: 1_792_410_240.0,
            seconds_per_frame: 60.0,
        };
        let mut host = FrameLight::of(&look);
        host.illumination.compensation_ev = 1.5;
        // The host's, whatever the clock says.
        let lit = frame_light(Some(host), Some(&clock), 3, &look);
        assert_eq!(lit, host);
        // No host: the clock's instant, its sun.
        let lit = frame_light(None, Some(&clock), 3, &look);
        assert_eq!(lit.instant, Some(clock.start_utc + 120.0));
        let sun = tuile_atmosphere::Sun::at_unix_seconds(clock.start_utc + 120.0).direction_ecef;
        assert_eq!(lit.illumination.to_sun_ecef, sun);
        // Neither: the look's.
        assert_eq!(frame_light(None, None, 3, &look), FrameLight::of(&look));
    }
}
