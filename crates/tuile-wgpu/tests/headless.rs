// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Headless render tests: offscreen target, pixel readback, PNG dumps.
//!
//! Every test writes what it rendered to `target/test-renders/*.png` so a
//! human can look at the exact frames the assertions ran on. Tests skip
//! cleanly (with a stderr note) on machines without any GPU adapter.

use glam::{dvec3, DVec3, Mat4, Vec3};
use tuile_core::content::{DecodedMesh, DecodedTexture, DecodedTileContent, MaterialDesc};
use tuile_wgpu::{prepare, GpuContext, TileRenderer, ViewUniform, DEPTH_FORMAT, SAMPLES};

const SIZE: u32 = 256;

fn gpu() -> Option<GpuContext> {
    match pollster::block_on(GpuContext::headless()) {
        Ok(gpu) => Some(gpu),
        Err(e) => {
            eprintln!("SKIP headless render test: {e}");
            None
        }
    }
}

/// A quad in the local XY plane ([-1,1]²), normals +Z, full uv range,
/// textured with a 2×2 checker: red, green / blue, white.
fn quad_content(origin: DVec3) -> DecodedTileContent {
    DecodedTileContent {
        meshes: vec![DecodedMesh {
            positions: vec![
                [-1.0, 1.0, 0.0],
                [1.0, 1.0, 0.0],
                [-1.0, -1.0, 0.0],
                [1.0, -1.0, 0.0],
            ],
            normals: Some(vec![[0.0, 0.0, 1.0]; 4]),
            uvs: Some(vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]]),
            indices: vec![0, 2, 1, 1, 2, 3],
            material: MaterialDesc {
                base_color_factor: [1.0; 4],
                base_color_texture: Some(0),
            },
        }],
        textures: vec![DecodedTexture {
            width: 2,
            height: 2,
            rgba8: vec![
                255, 0, 0, 255, // (0,0) red
                0, 255, 0, 255, // (1,0) green
                0, 0, 255, 255, // (0,1) blue
                255, 255, 255, 255, // (1,1) white
            ],
        }],
        imagery: Vec::new(),
        local_origin_ecef: origin,
        transform_local: Mat4::IDENTITY,
    }
}

struct Frame {
    pixels: Vec<u8>, // tightly packed RGBA8, SIZE×SIZE
}

impl Frame {
    fn px(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * SIZE + x) * 4) as usize;
        [
            self.pixels[i],
            self.pixels[i + 1],
            self.pixels[i + 2],
            self.pixels[i + 3],
        ]
    }

    fn save(&self, name: &str) {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-renders");
        std::fs::create_dir_all(&dir).expect("create test-renders dir");
        let path = dir.join(name);
        image::RgbaImage::from_raw(SIZE, SIZE, self.pixels.clone())
            .expect("image from raw")
            .save(&path)
            .expect("save png");
        eprintln!(
            "rendered frame written to {}",
            path.canonicalize().expect("path").display()
        );
    }
}

/// Renders `tiles` once with the given view and reads back the pixels.
fn render_frame(
    gpu: &GpuContext,
    renderer: &TileRenderer,
    tiles: &[&tuile_wgpu::PreparedTile],
    view: &ViewUniform,
) -> Frame {
    renderer.set_view(&gpu.queue, view);

    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test color"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    // Multisampled, as the session is. The tile pipeline is built for `SAMPLES`
    // and a single-sample pass will not accept it, which is how every test in
    // this file came to fail validation on every run: MSAA reached the renderer
    // long after the harness was written, and nobody had run it since.
    let msaa = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test msaa"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: SAMPLES,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test depth"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: SAMPLES,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let color_view = color.create_view(&Default::default());
    let msaa_view = msaa.create_view(&Default::default());
    let depth_view = depth.create_view(&Default::default());

    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("test pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &msaa_view,
                resolve_target: Some(&color_view),
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.0,
                        g: 0.0,
                        b: 0.05,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        renderer.render(&mut pass, tiles.iter().copied(), false);
    }

    let bytes_per_row = SIZE * 4; // 1024, already 256-aligned
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test readback"),
        size: u64::from(bytes_per_row * SIZE),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &color,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(SIZE),
            },
        },
        wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
    );
    gpu.queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map readback"));
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let pixels = slice.get_mapped_range().to_vec();
    Frame { pixels }
}

fn looking_down_z(distance: f32) -> ViewUniform {
    let proj = Mat4::perspective_rh(60f32.to_radians(), 1.0, 0.1, 100.0);
    let view = Mat4::look_at_rh(Vec3::new(0.0, 0.0, distance), Vec3::ZERO, Vec3::Y);
    ViewUniform {
        view_proj: (proj * view).to_cols_array(),
        sun_dir: [0.0, 0.0, -1.0, 0.0],
        params: [0.25, 0.0, 0.0, 0.0],
        // Off: these tests assert exact colours, and air would change them.
        atmosphere: Default::default(),
    }
}

#[test]
fn textured_quad_renders_with_correct_colors() {
    let Some(gpu) = gpu() else { return };
    // Far-away f64 origin, camera relative to the same origin: this is the
    // rebasing protocol working end to end on the GPU.
    let origin = dvec3(6.4e6, 1.0e6, 2.0e6);
    let tile = prepare(&gpu, &quad_content(origin), origin);
    let renderer = TileRenderer::new(&gpu, wgpu::TextureFormat::Rgba8UnormSrgb);

    let frame = render_frame(&gpu, &renderer, &[&tile], &looking_down_z(2.5));
    frame.save("headless-quad.png");

    // Background untouched in the corner.
    let corner = frame.px(4, 4);
    assert!(corner[0] < 20 && corner[1] < 20, "corner = {corner:?}");

    // The quad spans ~±0.69 NDC at distance 2.5 (fov 60°): sample inside
    // each texture quadrant, away from the bilinear seams.
    // Screen top-left ↔ uv (0,0) → red.
    let tl = frame.px(SIZE / 2 - 40, SIZE / 2 - 40);
    let tr = frame.px(SIZE / 2 + 40, SIZE / 2 - 40);
    let bl = frame.px(SIZE / 2 - 40, SIZE / 2 + 40);
    let br = frame.px(SIZE / 2 + 40, SIZE / 2 + 40);
    assert!(
        tl[0] > 150 && tl[1] < 90 && tl[2] < 90,
        "top-left = {tl:?} (want red)"
    );
    assert!(tr[1] > 150 && tr[0] < 90, "top-right = {tr:?} (want green)");
    assert!(
        bl[2] > 150 && bl[0] < 90,
        "bottom-left = {bl:?} (want blue)"
    );
    assert!(
        br[0] > 150 && br[1] > 150 && br[2] > 150,
        "bottom-right = {br:?} (want white)"
    );
}

#[test]
fn depth_test_orders_overlapping_quads() {
    let Some(gpu) = gpu() else { return };
    let origin = DVec3::ZERO;
    // A red-ish textured quad at z=0 and a plain white quad behind it at
    // z=-1, shifted right so both are partially visible.
    let front = prepare(&gpu, &quad_content(origin), origin);
    let mut behind_content = quad_content(origin);
    behind_content.meshes[0].material.base_color_texture = None;
    behind_content.transform_local =
        Mat4::from_translation(Vec3::new(2.0, 0.0, -1.0)) * Mat4::from_scale(Vec3::splat(1.0));
    let behind = prepare(&gpu, &behind_content, origin);

    let renderer = TileRenderer::new(&gpu, wgpu::TextureFormat::Rgba8UnormSrgb);
    // Draw the far quad LAST: only the depth test can order them correctly.
    let frame = render_frame(&gpu, &renderer, &[&front, &behind], &looking_down_z(3.0));
    frame.save("headless-depth.png");

    // Center belongs to the front textured quad (red quadrant at its
    // top-left); a point far right belongs to the white quad.
    let front_px = frame.px(SIZE / 2 - 30, SIZE / 2 - 30);
    assert!(front_px[0] > 120, "front quad pixel = {front_px:?}");
    let white_px = frame.px(SIZE - 20, SIZE / 2);
    assert!(
        white_px[0] > 150 && white_px[1] > 150 && white_px[2] > 150,
        "behind quad pixel = {white_px:?} (want white)"
    );
}

/// A 4×1 strip: red on its west half, green on its east. Sampling it tells you
/// *where* in the texture a fragment landed, which a flat colour cannot.
fn ramp_texture() -> std::sync::Arc<DecodedTexture> {
    std::sync::Arc::new(DecodedTexture {
        width: 4,
        height: 1,
        rgba8: vec![
            255, 0, 0, 255, // texel 0 red
            255, 0, 0, 255, // texel 1 red
            0, 255, 0, 255, // texel 2 green
            0, 255, 0, 255, // texel 3 green
        ],
    })
}

/// Screen x of a point at tile-uv `u`, for the quad under `looking_down_z(2.5)`.
///
/// The quad is the local `[-1, 1]` square, which a 60° lens at 2.5 puts at
/// ±`1 / (2.5 · tan 30°)` in NDC.
fn screen_x(u: f32) -> u32 {
    let half = 1.0 / (2.5 * (30f32.to_radians()).tan());
    let ndc = -half + 2.0 * half * u;
    (((ndc + 1.0) * 0.5) * SIZE as f32) as u32
}

/// The whole layered model, end to end on the GPU: two imagery tiles draping
/// one quad, each masked to its own half and each mapped into its texture by its
/// own affine transform.
///
/// The ramp is what makes this an assertion about *placement* rather than about
/// colour. A flat texture per layer would pass with the transform ignored
/// entirely; sampling a texture that differs across its own width means the four
/// probes only read red, green, red, green if each layer both covers the right
/// half of the tile and stretches correctly across it.
#[test]
fn two_imagery_layers_cover_their_own_halves_of_a_tile() {
    let Some(gpu) = gpu() else { return };
    let origin = DVec3::ZERO;
    let mut content = quad_content(origin);
    // The ground is the layers; nothing underneath them contributes.
    content.meshes[0].material.base_color_texture = None;
    content.textures.clear();

    let texture = ramp_texture();
    content.imagery = vec![
        // West half of the tile, stretched over the whole strip.
        tuile_core::raster::ImageryLayer {
            coord: tuile_core::raster::ImageryCoord {
                level: 4,
                x: 2,
                y: 3,
            },
            texture: std::sync::Arc::clone(&texture),
            coverage: [0.0, 0.0, 0.5, 1.0],
            translation: [0.0, 0.0],
            scale: [2.0, 1.0],
        },
        // East half: same stretch, shifted a tile-width west.
        tuile_core::raster::ImageryLayer {
            coord: tuile_core::raster::ImageryCoord {
                level: 4,
                x: 3,
                y: 3,
            },
            texture: std::sync::Arc::clone(&texture),
            coverage: [0.5, 0.0, 1.0, 1.0],
            translation: [-1.0, 0.0],
            scale: [2.0, 1.0],
        },
    ];

    let tile = prepare(&gpu, &content, origin);
    let renderer = TileRenderer::new(&gpu, wgpu::TextureFormat::Rgba8UnormSrgb);
    let frame = render_frame(&gpu, &renderer, &[&tile], &looking_down_z(2.5));
    frame.save("headless-imagery-layers.png");

    let y = SIZE / 2;
    for (u, want) in [
        (0.05, "red"),
        (0.45, "green"),
        (0.55, "red"),
        (0.95, "green"),
    ] {
        let px = frame.px(screen_x(u), y);
        let ok = match want {
            "red" => px[0] > 150 && px[1] < 90,
            _ => px[1] > 150 && px[0] < 90,
        };
        assert!(ok, "tile u={u} rendered {px:?}, want {want}");
    }
}

/// The claim the whole change rests on: an imagery tile draped by several
/// geometry tiles is uploaded once.
///
/// Asserted on the GPU-side count rather than on a byte total, because the byte
/// total is what a per-tile figure gets wrong — and counting distinct textures
/// is the thing a per-tile figure cannot say at all.
#[test]
fn imagery_shared_between_tiles_is_uploaded_once() {
    let Some(gpu) = gpu() else { return };
    let origin = DVec3::ZERO;
    let shared = tuile_core::raster::ImageryCoord {
        level: 7,
        x: 11,
        y: 13,
    };
    let texture = ramp_texture();
    let layer = tuile_core::raster::ImageryLayer {
        coord: shared,
        texture: std::sync::Arc::clone(&texture),
        coverage: [0.0, 0.0, 1.0, 1.0],
        translation: [0.0, 0.0],
        scale: [1.0, 1.0],
    };

    let mut first = quad_content(origin);
    first.imagery = vec![layer.clone()];
    let mut second = quad_content(origin);
    second.imagery = vec![layer.clone()];

    let a = prepare(&gpu, &first, origin);
    let b = prepare(&gpu, &second, origin);
    let (count, bytes) = gpu.imagery.lock().expect("imagery").live();
    assert_eq!(
        count, 1,
        "two tiles draping one imagery tile uploaded {count}"
    );
    assert!(bytes > 0, "the shared texture reports no memory");

    // And it is the *last* holder that frees it — not the first to be dropped.
    drop(a);
    assert_eq!(
        gpu.imagery.lock().expect("imagery").live().0,
        1,
        "dropping one of two holders freed the shared texture"
    );
    drop(b);
    assert_eq!(
        gpu.imagery.lock().expect("imagery").live().0,
        0,
        "dropping the last holder left the texture behind"
    );
}

/// A synthetic globe for the atmosphere tests, in render space.
///
/// The eye sits at the origin looking down `-Z`; the planet's centre is one
/// radius below, so "up" at the origin is `+Z` and the sun shining straight down
/// lights the air fully. Everything is stated rather than derived from a real
/// ECEF camera, because what is under test here is the *shader* — the Rust that
/// fills these fields has its own tests in `tuile-atmosphere`.
fn air_at(eye_height_m: f32, strength: f32) -> tuile_atmosphere::AerialPerspective {
    let radius = tuile_core::geo::WGS84_A as f32;
    tuile_atmosphere::AerialPerspective {
        eye: [0.0, 0.0, 0.0, eye_height_m],
        earth: [0.0, 0.0, -(radius + eye_height_m), radius],
        // Straight down: with up at `+Z`, the air over the target is fully lit.
        sun: [0.0, 0.0, -1.0, 1.0],
        mie: [
            tuile_atmosphere::aerial::MIE_SCATTERING,
            tuile_atmosphere::aerial::MIE_SCALE_HEIGHT,
            tuile_atmosphere::aerial::MIE_ANISOTROPY,
            strength,
        ],
        ..Default::default()
    }
}

/// How dark the test surface is, linear.
///
/// Dark on purpose. Over *white* ground the air's two effects nearly cancel —
/// extinction takes blue out of the surface while in-scattering puts blue back —
/// and the shift is a couple of percent. Over dark ground there is almost
/// nothing to take away and the air's own colour is all that is left, which is
/// why it is distant forest and distant mountains that go blue and distant snow
/// that does not. Testing on white would have measured the cancellation.
const DARK_SURFACE: f32 = 0.1;

/// A dark quad `metres` away **horizontally**, filling the frame.
///
/// Horizontally rather than straight ahead so the geometry stays honest: a point
/// 80 km away across a curved planet really does stand higher above the
/// tangent plane than one 2 km away, and the shader reads that height. Placing
/// the quad along the view axis would have buried it below the surface at the
/// far distance and the test would have been measuring a clamp.
fn dark_quad_at(metres: f32) -> (DecodedTileContent, ViewUniform) {
    let mut content = quad_content(DVec3::ZERO);
    content.meshes[0].material.base_color_texture = None;
    content.meshes[0].material.base_color_factor = [DARK_SURFACE, DARK_SURFACE, DARK_SURFACE, 1.0];
    content.textures.clear();
    // Out along -X, turned to face the eye, and scaled to fill the frame at
    // whatever distance it sits.
    content.transform_local = Mat4::from_translation(Vec3::new(-metres, 0.0, 0.0))
        * Mat4::from_rotation_y(std::f32::consts::FRAC_PI_2)
        * Mat4::from_scale(Vec3::splat(metres));

    let proj = Mat4::perspective_rh(60f32.to_radians(), 1.0, metres * 0.01, metres * 10.0);
    let view = Mat4::look_at_rh(Vec3::ZERO, Vec3::new(-metres, 0.0, 0.0), Vec3::Z);
    let uniform = ViewUniform {
        view_proj: (proj * view).to_cols_array(),
        // A headlight, and full ambient besides: lighting then contributes
        // exactly the same at both distances, so any difference between the two
        // frames is the air and nothing else.
        sun_dir: [-1.0, 0.0, 0.0, 0.0],
        params: [1.0, 0.0, 0.0, 0.0],
        atmosphere: Default::default(),
    };
    (content, uniform)
}

/// Aerial perspective, end to end through the shader: distant ground has to come
/// back hazier and bluer than near ground.
///
/// The blue/red *ratio* is what is asserted, not brightness. Brightness alone
/// would also move if the shader merely dimmed things with distance, and a
/// distance fog is exactly what this must not be — the cue people read is the
/// colour shift, because that is what real air does and what a grey fade does
/// not.
#[test]
fn distant_ground_comes_back_hazier_and_bluer_than_near_ground() {
    let Some(gpu) = gpu() else { return };
    let renderer = TileRenderer::new(&gpu, wgpu::TextureFormat::Rgba8UnormSrgb);

    let sample = |metres: f32, strength: f32, name: &str| -> [u8; 4] {
        let (content, mut uniform) = dark_quad_at(metres);
        uniform.atmosphere = air_at(1_000.0, strength);
        let tile = prepare(&gpu, &content, DVec3::ZERO);
        let frame = render_frame(&gpu, &renderer, &[&tile], &uniform);
        frame.save(name);
        frame.px(SIZE / 2, SIZE / 2)
    };

    let near = sample(2_000.0, 1.0, "headless-air-near.png");
    let far = sample(80_000.0, 1.0, "headless-air-far.png");
    let no_air = sample(80_000.0, 0.0, "headless-air-off.png");

    // Off means off: with no air the quad must come back the neutral grey it
    // actually is, or the default is quietly tinting every scene that has no
    // planet in it.
    let [r, g, b, _] = no_air.map(i32::from);
    let spread = r.max(g).max(b) - r.min(g).min(b);
    assert!(
        spread <= 1,
        "with the atmosphere off the quad should be neutral, got {no_air:?}"
    );
    assert!(
        far != no_air,
        "the atmosphere changed nothing at 80 km: {far:?}"
    );

    // On, and blue: over dark ground the air's own colour is most of what comes
    // back, and the air is blue.
    assert!(
        far[2] > far[0] + 8,
        "80 km of air left the ground neutral: {far:?}"
    );
    let blueness = |px: [u8; 4]| f32::from(px[2]) / f32::from(px[0]).max(1.0);
    assert!(
        blueness(far) > blueness(near),
        "80 km ({far:?}, blue/red {:.3}) is no bluer than 2 km ({near:?}, {:.3})",
        blueness(far),
        blueness(near)
    );
    // And it must *lighten* as well as blue: haze adds light to dark ground.
    // Extinction alone would darken it, which is a fog, not distance.
    assert!(
        far[2] > near[2],
        "the far quad is not brighter in blue: {far:?} against {near:?}"
    );
}

#[test]
fn untextured_mesh_without_normals_gets_computed_normals() {
    let Some(gpu) = gpu() else { return };
    let origin = DVec3::ZERO;
    let mut content = quad_content(origin);
    content.meshes[0].normals = None;
    content.meshes[0].material.base_color_texture = None;
    content.textures.clear();

    let tile = prepare(&gpu, &content, origin);
    let renderer = TileRenderer::new(&gpu, wgpu::TextureFormat::Rgba8UnormSrgb);
    let frame = render_frame(&gpu, &renderer, &[&tile], &looking_down_z(2.5));
    frame.save("headless-normals.png");

    // Facing the light head-on: fully lit white (not just the ambient 25%).
    let center = frame.px(SIZE / 2, SIZE / 2);
    assert!(
        center[0] > 200 && center[1] > 200 && center[2] > 200,
        "center = {center:?} (computed normals should give full lambert)"
    );
}
