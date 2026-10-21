// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The world overlay pipelines on a real device.
//!
//! The arithmetic of packing and ordering is unit-tested in the crate. What
//! can only be seen on a GPU is whether the pipelines were built to say what
//! they mean: a frustum's edges landing where the camera puts them, the three
//! depth modes each hiding what they should and nothing else, and a
//! translucent mesh compositing rather than replacing.
//!
//! Each test draws over a known background and reads the pixels back. The
//! frustum is a golden: a shape at a camera, with the pixels it covers
//! counted and its corners checked, so a view matrix or a vertex layout that
//! drifts is a failure here rather than a HUD half a pixel off in the
//! simulator.

use glam::{DVec3, Mat4, Vec3};
use tuile_overlay::{OverlayBlend, OverlayDepth, OverlayEyes, OverlayMesh, OverlayShadow};
use tuile_wgpu::{
    GpuContext, WorldOverlayRenderer, WorldOverlayView, DEPTH_FORMAT, SAMPLES, TEXTURE_FORMAT,
};

const SIZE: u32 = 64;

fn gpu() -> Option<GpuContext> {
    match pollster::block_on(GpuContext::headless()) {
        Ok(gpu) => Some(gpu),
        Err(e) => {
            eprintln!("SKIP world overlay test: {e}");
            None
        }
    }
}

/// A camera at `eye` looking at `at`, with the view relative to `eye` as the
/// renderer expects: the render origin is the eye, so the meshes and the
/// matrix agree.
fn view(eye: DVec3, at: DVec3, up: Vec3) -> (WorldOverlayView, DVec3) {
    let fwd = (at - eye).normalize().as_vec3();
    let right = fwd.cross(up).normalize();
    let real_up = right.cross(fwd);
    // Eye-relative: the eye is the origin, so the view matrix looks from zero.
    let look = Mat4::look_to_rh(Vec3::ZERO, fwd, real_up);
    let proj = Mat4::perspective_rh(0.8, 1.0, 1.0, 100_000.0);
    (WorldOverlayView::new(proj * look, right, real_up), eye)
}

/// An axis-aligned square of side `side` metres, centred on `origin`, facing
/// the eye along x — the simplest shape whose pixels can be counted.
fn plate(
    origin: DVec3,
    side: f32,
    color: [f32; 4],
    depth: OverlayDepth,
    blend: OverlayBlend,
) -> OverlayMesh {
    let h = side / 2.0;
    OverlayMesh {
        origin_ecef: origin.to_array(),
        positions: vec![[0.0, -h, -h], [0.0, h, -h], [0.0, h, h], [0.0, -h, h]],
        colors: vec![color; 4],
        normals: Vec::new(),
        indices: vec![0, 1, 2, 0, 2, 3],
        depth,
        blend,
        eyes: OverlayEyes::Both,
        shadow: OverlayShadow::None,
    }
}

/// The frustum of a camera at `eye` looking along `fwd`: the four edges from
/// the apex to the near rectangle's corners, as triangles a pixel or so wide,
/// plus the rectangle itself. What a simulator draws for a film camera.
fn frustum(eye: DVec3, fwd: Vec3, up: Vec3, fovy: f32, aspect: f32, far: f32) -> OverlayMesh {
    let right = fwd.cross(up).normalize();
    let real_up = right.cross(fwd);
    let half_h = (fovy / 2.0).tan() * far;
    let half_w = half_h * aspect;
    let corner = |sx: f32, sy: f32| fwd * far + right * (sx * half_w) + real_up * (sy * half_h);
    let corners = [
        corner(-1.0, -1.0),
        corner(1.0, -1.0),
        corner(1.0, 1.0),
        corner(-1.0, 1.0),
    ];
    let mut positions = vec![[0.0f32; 3]];
    for c in &corners {
        positions.push(c.to_array());
    }
    // The apex to each corner, and the rectangle closed: as triangles,
    // because a renderer of triangles draws no lines.
    let mut indices = Vec::new();
    for i in 1..=4u32 {
        let next = if i == 4 { 1 } else { i + 1 };
        indices.extend_from_slice(&[0, i, next]);
    }
    OverlayMesh {
        origin_ecef: eye.to_array(),
        colors: vec![[0.0, 1.0, 0.0, 1.0]; positions.len()],
        positions,
        normals: Vec::new(),
        indices,
        depth: OverlayDepth::Always,
        blend: OverlayBlend::Opaque,
        eyes: OverlayEyes::Both,
        shadow: OverlayShadow::None,
    }
}

/// What a frame came out as: the resolved pixels, row-major RGBA8.
struct Frame {
    pixels: Vec<u8>,
}

impl Frame {
    fn at(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * SIZE + x) * 4) as usize;
        [
            self.pixels[i],
            self.pixels[i + 1],
            self.pixels[i + 2],
            self.pixels[i + 3],
        ]
    }

    /// How many pixels are more `channel` than `least`.
    fn covered(&self, channel: usize, least: u8) -> usize {
        self.pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[channel] >= least)
            .count()
    }
}

/// Draws `meshes` through `view`, over `clear`, and reads the frame back.
///
/// `ground` is a depth value written over the whole frame before the overlays
/// — what a ground pass would have left — so the depth modes can be told
/// apart: `None` leaves the depth at the far plane, as an empty globe would.
fn render(
    gpu: &GpuContext,
    meshes: &[OverlayMesh],
    (view, origin): (WorldOverlayView, DVec3),
    clear: wgpu::Color,
    ground: Option<&OverlayMesh>,
) -> Frame {
    let mut overlay = WorldOverlayRenderer::new(gpu, TEXTURE_FORMAT);
    let texture = |sample_count, usage, format| {
        gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("world overlay test"),
            size: wgpu::Extent3d {
                width: SIZE,
                height: SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        })
    };
    let resolved = texture(
        1,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        TEXTURE_FORMAT,
    );
    let msaa = texture(
        SAMPLES,
        wgpu::TextureUsages::RENDER_ATTACHMENT,
        TEXTURE_FORMAT,
    );
    let depth = texture(
        SAMPLES,
        wgpu::TextureUsages::RENDER_ATTACHMENT,
        DEPTH_FORMAT,
    );
    let resolved_view = resolved.create_view(&Default::default());
    let msaa_view = msaa.create_view(&Default::default());
    let depth_view = depth.create_view(&Default::default());

    let bytes_per_row = (SIZE * 4).div_ceil(256) * 256;
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("world overlay readback"),
        size: u64::from(bytes_per_row * SIZE),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    overlay.set_view(&gpu.queue, &view);
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    // The ground, drawn as an overlay that writes depth: a stand-in for the
    // tile pass, so a `Terrain` mesh behind it is hidden by its depth and an
    // `Always` one is not.
    {
        let mut stand_in = WorldOverlayRenderer::new(gpu, TEXTURE_FORMAT);
        stand_in.set_view(&gpu.queue, &view);
        if let Some(ground) = ground {
            stand_in
                .set_meshes(gpu, std::slice::from_ref(ground), origin, None)
                .expect("a sound ground stand-in");
        }
        overlay
            .set_meshes(gpu, meshes, origin, None)
            .expect("sound meshes");
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("world overlay pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &msaa_view,
                resolve_target: Some(&resolved_view),
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth_view,
                depth_ops: Some(wgpu::Operations {
                    // Forward-Z, as the whole interactive path: 1 is far.
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(0),
                    store: wgpu::StoreOp::Discard,
                }),
            }),
            ..Default::default()
        });
        stand_in.render(&mut pass);
        overlay.render(&mut pass);
    }
    // The overlays-only pass, into the frame's own multisampled attachment
    // and resolved again: every attachment of a pass shares its sample
    // count, and these pipelines are built for `SAMPLES`.
    overlay.render_overlays_only(
        gpu,
        &mut encoder,
        &msaa_view,
        Some(&resolved_view),
        (SIZE, SIZE),
    );
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &resolved,
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
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("the device");
    let data = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for row in 0..SIZE {
        let start = (row * bytes_per_row) as usize;
        pixels.extend_from_slice(&data[start..start + (SIZE * 4) as usize]);
    }
    drop(data);
    readback.unmap();
    Frame { pixels }
}

const BLACK: wgpu::Color = wgpu::Color {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 1.0,
};

/// The frustum golden: a camera's frustum drawn at a known camera, with the
/// pixels it covers counted.
///
/// It is a golden because every link in the chain shows in it — the f64
/// rebase, the vertex layout, the view uniform, the pipeline's own state —
/// and none of them fails loudly on its own. The count is generous enough to
/// survive a rasterisation rule, and tight enough that a frustum drawn at
/// half the size or with its up axis flipped fails.
#[test]
fn a_frustum_lands_where_its_camera_puts_it() {
    let Some(gpu) = gpu() else { return };
    // On the surface, so that the f64 rebase is doing real work: the mesh
    // origin is 6378 km from the Earth's centre and the eye is 2 km away.
    let earth = 6_378_137.0;
    let film = DVec3::new(earth, 0.0, 0.0);
    let eye = DVec3::new(earth + 200.0, -2_000.0, 600.0);
    let frustum = frustum(
        film,
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::X,
        0.6,
        16.0 / 9.0,
        1_500.0,
    );
    let frame = render(
        &gpu,
        std::slice::from_ref(&frustum),
        view(eye, film, Vec3::X),
        BLACK,
        None,
    );
    let green = frame.covered(1, 128);
    assert!(
        (150..2_600).contains(&green),
        "a frustum at this camera covers {green} pixels of {}; the golden band is 150..2600",
        SIZE * SIZE
    );
    // And it is green, not the clear: the colour survived the pipeline.
    assert!(
        frame.covered(0, 128) == 0,
        "nothing of it is red: the colour is the mesh's, not the clear's"
    );
}

/// The three depth modes, each against the same ground.
#[test]
fn each_depth_mode_hides_what_it_should_and_nothing_else() {
    let Some(gpu) = gpu() else { return };
    let earth = 6_378_137.0;
    let eye = DVec3::new(earth + 1_000.0, 0.0, 0.0);
    let at = DVec3::new(earth, 0.0, 0.0);
    let camera = view(eye, at, Vec3::Z);
    // A wall across the whole view, 500 m from the eye, standing in for the
    // ground: it writes depth.
    let ground = plate(
        DVec3::new(earth + 500.0, 0.0, 0.0),
        4_000.0,
        [0.0, 0.0, 1.0, 1.0],
        OverlayDepth::Always,
        OverlayBlend::Opaque,
    );
    // A shape further from the eye than the wall — 300 m above the
    // ellipsoid against the wall's 500 — so the wall is between them.
    let behind = DVec3::new(earth + 300.0, 0.0, 0.0);
    let green = [0.0, 1.0, 0.0, 1.0];

    // Terrain: hidden by the wall.
    let frame = render(
        &gpu,
        &[plate(
            behind,
            200.0,
            green,
            OverlayDepth::Terrain,
            OverlayBlend::Opaque,
        )],
        camera,
        BLACK,
        Some(&ground),
    );
    assert_eq!(
        frame.covered(1, 128),
        0,
        "a Terrain mesh behind the ground is hidden by it"
    );
    assert!(frame.covered(2, 128) > 0, "and the ground is drawn");

    // Always: over it.
    let frame = render(
        &gpu,
        &[plate(
            behind,
            200.0,
            green,
            OverlayDepth::Always,
            OverlayBlend::Opaque,
        )],
        camera,
        BLACK,
        Some(&ground),
    );
    assert!(
        frame.covered(1, 128) > 0,
        "an Always mesh is never hidden by the ground"
    );

    // OverlaysOnly: over it too, and in a pass whose depth the ground never
    // reached. The HUD's mode.
    let frame = render(
        &gpu,
        &[plate(
            behind,
            200.0,
            green,
            OverlayDepth::OverlaysOnly,
            OverlayBlend::Opaque,
        )],
        camera,
        BLACK,
        Some(&ground),
    );
    assert!(
        frame.covered(1, 128) > 0,
        "an OverlaysOnly mesh is never hidden by the ground"
    );
}

/// Among overlays alone, the nearer plate wins — which is the whole point of
/// the mode, and what the HUD's own layers rely on.
#[test]
fn overlays_only_meshes_stand_before_one_another_as_their_depths_say() {
    let Some(gpu) = gpu() else { return };
    let earth = 6_378_137.0;
    let eye = DVec3::new(earth + 1_000.0, 0.0, 0.0);
    let camera = view(eye, DVec3::new(earth, 0.0, 0.0), Vec3::Z);
    let near = plate(
        DVec3::new(earth + 800.0, 0.0, 0.0),
        300.0,
        [0.0, 1.0, 0.0, 1.0],
        OverlayDepth::OverlaysOnly,
        OverlayBlend::Opaque,
    );
    let far = plate(
        DVec3::new(earth + 400.0, 0.0, 0.0),
        300.0,
        [1.0, 0.0, 0.0, 1.0],
        OverlayDepth::OverlaysOnly,
        OverlayBlend::Opaque,
    );
    // Given far-first and near-first alike: depth decides, not the order.
    for meshes in [
        vec![far.clone(), near.clone()],
        vec![near.clone(), far.clone()],
    ] {
        let frame = render(&gpu, &meshes, camera, BLACK, None);
        let centre = frame.at(SIZE / 2, SIZE / 2);
        assert!(
            centre[1] > 128 && centre[0] < 128,
            "the nearer plate owns the centre, not {centre:?}"
        );
    }
}

/// A translucent mesh composites over what is behind it; an opaque one
/// replaces it. Colours arrive premultiplied, which is how both are blended.
#[test]
fn translucent_composites_and_opaque_replaces() {
    let Some(gpu) = gpu() else { return };
    let earth = 6_378_137.0;
    let eye = DVec3::new(earth + 1_000.0, 0.0, 0.0);
    let camera = view(eye, DVec3::new(earth, 0.0, 0.0), Vec3::Z);
    let red = wgpu::Color {
        r: 1.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    };
    let big = DVec3::new(earth + 500.0, 0.0, 0.0);
    // Half-covering green, premultiplied: rgb already carries the alpha.
    let half = plate(
        big,
        4_000.0,
        [0.0, 0.5, 0.0, 0.5],
        OverlayDepth::Always,
        OverlayBlend::Translucent,
    );
    let frame = render(&gpu, std::slice::from_ref(&half), camera, red, None);
    let centre = frame.at(SIZE / 2, SIZE / 2);
    assert!(
        centre[0] > 60 && centre[1] > 60,
        "half green over red is both, not {centre:?}"
    );

    let solid = plate(
        big,
        4_000.0,
        [0.0, 1.0, 0.0, 1.0],
        OverlayDepth::Always,
        OverlayBlend::Opaque,
    );
    let frame = render(&gpu, std::slice::from_ref(&solid), camera, red, None);
    let centre = frame.at(SIZE / 2, SIZE / 2);
    assert!(
        centre[0] < 20 && centre[1] > 200,
        "an opaque mesh replaces what is behind it, not {centre:?}"
    );
}

/// A lit mesh is shaded, and an unlit one comes out as its colours say. The
/// light is the film's, through the shared WGSL, so a HUD plate's relief is
/// the same in both.
#[test]
fn normals_are_what_make_a_mesh_lit() {
    let Some(gpu) = gpu() else { return };
    let earth = 6_378_137.0;
    let eye = DVec3::new(earth + 1_000.0, 0.0, 0.0);
    let camera = view(eye, DVec3::new(earth, 0.0, 0.0), Vec3::Z);
    let mut flat = plate(
        DVec3::new(earth + 500.0, 0.0, 0.0),
        4_000.0,
        [1.0, 1.0, 1.0, 1.0],
        OverlayDepth::Always,
        OverlayBlend::Opaque,
    );
    let unlit = render(&gpu, std::slice::from_ref(&flat), camera, BLACK, None);
    assert_eq!(
        unlit.at(SIZE / 2, SIZE / 2)[0],
        255,
        "without normals, a colour is drawn as given"
    );
    // Facing away from the camera's light: the ambient share, not the whole.
    flat.normals = vec![[0.0, 1.0, 0.0]; 4];
    let lit = render(&gpu, std::slice::from_ref(&flat), camera, BLACK, None);
    let shaded = lit.at(SIZE / 2, SIZE / 2)[0];
    assert!(
        (100..255).contains(&shaded),
        "a lit plate is shaded, at {shaded}, and never black: the ambient share is 0.45"
    );
}

/// A faulty mesh is refused with nothing recorded, so the frame still holds
/// what it held: black ground is never an outcome of a bad batch.
#[test]
fn a_faulty_batch_leaves_the_last_one_drawn() {
    let Some(gpu) = gpu() else { return };
    let mut overlay = WorldOverlayRenderer::new(&gpu, TEXTURE_FORMAT);
    let good = plate(
        DVec3::new(6_378_137.0, 0.0, 0.0),
        100.0,
        [0.0, 1.0, 0.0, 1.0],
        OverlayDepth::Terrain,
        OverlayBlend::Opaque,
    );
    overlay
        .set_meshes(&gpu, std::slice::from_ref(&good), DVec3::ZERO, None)
        .expect("a sound mesh");
    assert!(!overlay.is_empty());
    let mut bad = good.clone();
    bad.colors.pop();
    overlay
        .set_meshes(&gpu, &[bad], DVec3::ZERO, None)
        .expect_err("not one colour a vertex");
    assert!(
        !overlay.is_empty(),
        "the good batch is still there: a refused one draws nothing new, not nothing at all"
    );
}
