// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A host's overlays through the whole render: a pack made of arithmetic in
//! a store on disk, rendered on the machine's adapter. Skipped when there
//! is none.

use std::sync::Arc;

use glam::DVec3;
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};
use tuile_film::{BakedView, FrameLight, Look, OverlayCamera, OverlayMesh, Overlays, Stereo};
use tuile_film_native::{render, Error, Film, Nothing, Order, Sink, Sources, Tone};
use tuile_pack::{BakedTile, PackWriter, TextureFormat};

const FRAMES: u32 = 3;

/// One flat tile 1 km a side, a grey PNG, under a camera 600 m up looking
/// down at it: frame after frame, the same tile.
fn write_pack(dir: &std::path::Path) -> String {
    let at = Geodetic {
        lon: 0.5f64.to_radians(),
        lat: 42.8f64.to_radians(),
        height: 0.0,
    };
    let centre = geodetic_to_ecef(at);
    let enu = enu_frame(at);
    let mut p = Vec::new();
    for (x, y) in [
        (-500.0, -500.0),
        (500.0, -500.0),
        (500.0, 500.0),
        (-500.0, 500.0),
    ] {
        let q = enu.x_axis * x + enu.y_axis * y;
        p.extend([q.x as f32, q.y as f32, q.z as f32]);
    }
    let up = enu.z_axis.as_vec3();
    let n: Vec<f32> = (0..4).flat_map(|_| [up.x, up.y, up.z]).collect();
    let uv = [0.0f32, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0];
    let idx = [0u32, 1, 2, 0, 2, 3];
    let le = |v: &[f32]| v.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>();
    let mut png = Vec::new();
    image::RgbaImage::from_pixel(4, 4, image::Rgba([120, 110, 90, 255]))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("png");
    let tile = BakedTile {
        id: 1,
        drape: 0,
        origin_ecef: centre.to_array(),
        positions: le(&p),
        normals: le(&n),
        uvs: le(&uv),
        indices: idx.iter().flat_map(|v| v.to_le_bytes()).collect(),
        vertex_count: 4,
        index_count: 6,
        base_color_factor: [1.0; 4],
        texture: Some(png),
        texture_format: TextureFormat::Png,
        refs: None,
    };
    let blob = dir.join("blob");
    let mut writer = PackWriter::new("overlays", centre.to_array(), blob).expect("writer");
    for f in 1..=FRAMES {
        let eye: DVec3 = centre + enu.z_axis * 600.0 + enu.x_axis * f64::from(f);
        let view = BakedView {
            position: eye.to_array(),
            direction: (-enu.z_axis).to_array(),
            up: enu.y_axis.to_array(),
            viewport_px: [64.0, 48.0],
            fovy_rad: 0.8,
        };
        let mut open = writer.begin_frame(f, view);
        if !open.push_known(1, 0) {
            open.push(tile.clone());
        }
        open.end();
    }
    let key = "packs/overlays/1-3.tuilepack";
    let path = dir.join(key);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("packs dir");
    writer.finish_to(&path).expect("finish");
    key.into()
}

/// What a host is asked: each frame, and the camera it is asked with.
#[derive(Default)]
struct Asked(Vec<(u32, OverlayCamera)>);

impl Overlays for Asked {
    fn frame(&mut self, frame: u32, camera: &OverlayCamera, out: &mut Vec<OverlayMesh>) {
        self.0.push((frame, *camera));
        // Something to draw, so that each eye has overlays to draw.
        out.push(OverlayMesh {
            origin_ecef: camera.camera.eye.to_array(),
            positions: vec![[0.0, 0.0, -100.0], [10.0, 0.0, -100.0], [0.0, 10.0, -100.0]],
            colors: vec![[1.0; 4]; 3],
            indices: vec![0, 1, 2],
            ..OverlayMesh::default()
        });
    }
}

fn has_adapter() -> bool {
    pollster::block_on(async {
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle())
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .is_ok()
    })
}

/// A stereo film asks its host for a frame's overlays once — not once an
/// eye — with the frame's own camera between the eyes, and its pair.
/// Every picture, kept.
#[derive(Default)]
struct Kept(Vec<Vec<u8>>);

impl Sink for Kept {
    fn picture(&mut self, _: u32, _: u32, rgba: &[u8], _: &[u8]) -> Result<(), Error> {
        self.0.push(rgba.to_vec());
        Ok(())
    }
}

/// A host that places the light of some frames, and is asked which.
#[derive(Default)]
struct Lit {
    lights: Vec<(u32, FrameLight)>,
    asked: Vec<u32>,
    /// The sun's texel each frame's overlays were made with.
    texels: Vec<Option<f32>>,
}

impl Overlays for Lit {
    fn frame(&mut self, _: u32, camera: &OverlayCamera, _: &mut Vec<OverlayMesh>) {
        self.texels.push(camera.shadow_texel);
    }

    fn light(&mut self, frame: u32) -> Option<FrameLight> {
        self.asked.push(frame);
        self.lights
            .iter()
            .find(|(f, _)| *f == frame)
            .map(|(_, light)| *light)
    }
}

async fn test_film(dir: &std::path::Path) -> (Sources, Film) {
    let key = write_pack(dir);
    std::fs::write(dir.join("catalog.json"), br#"{"layers": []}"#).expect("catalog");
    let store: Arc<dyn tuile_repository::Objects> = Arc::new(
        tuile_farm::ObjectRunStore::local(dir, tuile_farm::Tuning::default()).expect("store"),
    );
    let sources = Sources::open(store.clone(), store, &dir.join("cache"))
        .await
        .expect("sources");
    let film = Film::of(sources.packs.objects.as_ref(), &[key.as_str()])
        .await
        .expect("film");
    (sources, film)
}

fn mean(rgba: &[u8]) -> f64 {
    let sum: u64 = rgba
        .chunks(4)
        .map(|p| u64::from(p[0]) + u64::from(p[1]) + u64::from(p[2]))
        .sum();
    sum as f64 / (rgba.len() / 4 * 3) as f64
}

/// The host's light lights the frames it is given for, once a frame and
/// for both eyes alike; the others, and a host that says nothing, are lit
/// by the order's look exactly as before.
#[tokio::test(flavor = "multi_thread")]
async fn a_hosts_light_lights_its_frames_and_only_those() {
    if !has_adapter() {
        eprintln!("no GPU adapter: skipped");
        return;
    }
    let dir = tempfile::tempdir().expect("dir");
    let (sources, film) = test_film(dir.path()).await;
    // The ground faces up, at 42.8° N: a sun high over it, and one set.
    let up = geodetic_to_ecef(Geodetic {
        lon: 0.5f64.to_radians(),
        lat: 42.8f64.to_radians(),
        height: 0.0,
    })
    .normalize()
    .as_vec3();
    let mut dim = FrameLight::of(&Look::default());
    dim.illumination.to_sun_ecef = -up.as_dvec3();
    dim.illumination.sun_rgb = [3.0; 3];
    dim.illumination.sky_sh[0] = [0.05; 3];
    let top_bottom = Stereo {
        packing: tuile_film::Packing::TopBottom,
        ..Stereo::default()
    };
    for stereo in [None, Some(top_bottom)] {
        let order = Order {
            supersample: 1,
            tone: Tone::Off,
            stereo,
            look: Look {
                to_sun: up,
                ..Look::default()
            },
            ..Order::default()
        };
        let mut ordered = Kept::default();
        render(&sources, &film, &order, &mut (), &mut ordered, &mut ())
            .await
            .expect("render");
        // The order's own light, said by the host: the same film, byte for
        // byte.
        let mut same = Lit {
            lights: (1..=FRAMES)
                .map(|f| (f, FrameLight::of(&order.look)))
                .collect(),
            ..Lit::default()
        };
        let mut said = Kept::default();
        render(&sources, &film, &order, &mut same, &mut said, &mut ())
            .await
            .expect("render");
        assert_eq!(same.asked, (1..=FRAMES).collect::<Vec<_>>(), "{stereo:?}");
        assert!(
            ordered.0 == said.0,
            "{stereo:?}: the order's light, said again"
        );
        // Frame 2 under a sun that has set: darker, and only frame 2.
        let mut host = Lit {
            lights: vec![(2, dim)],
            ..Lit::default()
        };
        let mut lit = Kept::default();
        render(&sources, &film, &order, &mut host, &mut lit, &mut ())
            .await
            .expect("render");
        assert_eq!(host.asked, (1..=FRAMES).collect::<Vec<_>>(), "{stereo:?}");
        assert!(
            lit.0[0] == ordered.0[0] && lit.0[2] == ordered.0[2],
            "{stereo:?}"
        );
        let (day, dusk) = (mean(&ordered.0[1]), mean(&lit.0[1]));
        assert!(
            dusk < day * 0.5,
            "{stereo:?}: {dusk} under the host's light, {day} under the order's"
        );
        if stereo.is_some() {
            // Both eyes under the one light — the left on top, the right
            // under it: each as dark as the other.
            let half = lit.0[1].len() / 2;
            let (a, b) = (mean(&lit.0[1][..half]), mean(&lit.0[1][half..]));
            assert!((a - b).abs() < 0.1 * a.max(b), "{a} and {b}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stereo_film_asks_for_each_frames_overlays_once_with_the_whole_camera() {
    if !has_adapter() {
        eprintln!("no GPU adapter: skipped");
        return;
    }
    let dir = tempfile::tempdir().expect("dir");
    let key = write_pack(dir.path());
    std::fs::write(dir.path().join("catalog.json"), br#"{"layers": []}"#).expect("catalog");
    let store: Arc<dyn tuile_repository::Objects> = Arc::new(
        tuile_farm::ObjectRunStore::local(dir.path(), tuile_farm::Tuning::default())
            .expect("store"),
    );
    let sources = Sources::open(store.clone(), store, &dir.path().join("cache"))
        .await
        .expect("sources");
    let film = Film::of(sources.packs.objects.as_ref(), &[key.as_str()])
        .await
        .expect("film");
    for stereo in [None, Some(Stereo::default())] {
        let order = Order {
            supersample: 1,
            tone: Tone::Off,
            stereo,
            ..Order::default()
        };
        let mut asked = Asked::default();
        let done = render(&sources, &film, &order, &mut asked, &mut Nothing, &mut ())
            .await
            .expect("render");
        assert_eq!(done.frames, FRAMES);
        let frames: Vec<u32> = asked.0.iter().map(|(f, _)| *f).collect();
        assert_eq!(frames, (1..=FRAMES).collect::<Vec<_>>(), "{stereo:?}");
        for (frame, camera) in &asked.0 {
            // The frame's own camera: the eye the pack baked, not an eye
            // moved sideways.
            assert_eq!(
                camera.camera.eye.to_array(),
                camera.view.position,
                "{frame}"
            );
            assert_eq!(camera.eyes.is_some(), stereo.is_some(), "{frame}");
            assert!(camera.near() > 0.0);
            if let Some(stereo) = stereo {
                assert_eq!(camera.eyes, Some(stereo.eyes(&camera.camera)));
            }
        }
    }
}

/// The sun's map is fitted to the frame's own sun: a host's low sun on one
/// frame gives that frame's overlays another texel than the order's high
/// one gives the others — the map is fitted after the light is placed.
#[tokio::test(flavor = "multi_thread")]
async fn the_suns_map_follows_the_frames_sun() {
    if !has_adapter() {
        eprintln!("no GPU adapter: skipped");
        return;
    }
    let dir = tempfile::tempdir().expect("dir");
    let (sources, film) = test_film(dir.path()).await;
    let enu = enu_frame(Geodetic {
        lon: 0.5f64.to_radians(),
        lat: 42.8f64.to_radians(),
        height: 0.0,
    });
    let up = enu.z_axis.as_vec3();
    let order = Order {
        supersample: 1,
        tone: Tone::Off,
        look: Look {
            to_sun: up,
            shadow: 0.8,
            ..Look::default()
        },
        ..Order::default()
    };
    let mut low = FrameLight::of(&order.look);
    low.illumination.to_sun_ecef = (up * 0.15 + enu.x_axis.as_vec3()).normalize().as_dvec3();
    let mut host = Lit {
        lights: vec![(2, low)],
        ..Lit::default()
    };
    render(&sources, &film, &order, &mut host, &mut Nothing, &mut ())
        .await
        .expect("render");
    let texels: Vec<f32> = host.texels.iter().map(|t| t.expect("a texel")).collect();
    assert_eq!(texels.len(), FRAMES as usize);
    // Frames 1 and 3 under the order's sun, alike; frame 2 under its own.
    let high = texels[0];
    assert!((texels[2] - high).abs() < 0.01 * high, "{texels:?}");
    assert!((texels[1] - high).abs() > 0.05 * high, "{texels:?}");
}

/// A host that places one frame's light and draws one opaque square the
/// whole picture's centre, as given, over everything.
struct Balanced(FrameLight);

impl Overlays for Balanced {
    fn frame(&mut self, _: u32, camera: &OverlayCamera, out: &mut Vec<OverlayMesh>) {
        // 100 m ahead, 10 m a side: the middle of the picture.
        let c = &camera.camera;
        let ahead = -c.view.row(2).truncate() * 100.0;
        let (r, u) = (
            c.view.row(0).truncate() * 5.0,
            c.view.row(1).truncate() * 5.0,
        );
        out.push(OverlayMesh {
            origin_ecef: c.eye.to_array(),
            positions: vec![
                (ahead - r - u).to_array(),
                (ahead + r - u).to_array(),
                (ahead + r + u).to_array(),
                (ahead - r + u).to_array(),
            ],
            colors: vec![[0.2, 0.4, 0.6, 1.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            depth: tuile_film::OverlayDepth::Always,
            blend: tuile_film::OverlayBlend::Opaque,
            ..OverlayMesh::default()
        });
    }

    fn light(&mut self, frame: u32) -> Option<FrameLight> {
        (frame == 2).then_some(self.0)
    }
}

/// A frame's white balance and exposure move the ground's colour, and
/// leave the overlays' as the host gave them.
#[tokio::test(flavor = "multi_thread")]
async fn a_frames_white_balance_and_exposure_are_the_grounds_not_the_overlays() {
    if !has_adapter() {
        eprintln!("no GPU adapter: skipped");
        return;
    }
    let dir = tempfile::tempdir().expect("dir");
    let (sources, film) = test_film(dir.path()).await;
    let order = Order {
        supersample: 1,
        tone: Tone::Off,
        ..Order::default()
    };
    // A light half a stop brighter than daylight, its white cool and the
    // camera adapted half way to it: the picture comes out warmer.
    let mut warm = FrameLight::of(&order.look);
    warm.illumination.ev100 += 0.5;
    warm.illumination.white = [0.8, 1.0, 1.6];
    warm.illumination.adaptation = 0.5;
    let mut kept = Kept::default();
    render(
        &sources,
        &film,
        &order,
        &mut Balanced(warm),
        &mut kept,
        &mut (),
    )
    .await
    .expect("render");
    let (width, height) = (64usize, 48usize);
    let at = |picture: &[u8], x: usize, y: usize| {
        let i = (y * width + x) * 4;
        [picture[i], picture[i + 1], picture[i + 2]]
    };
    // The centre is the overlay's; a corner is the ground's.
    let (centre, corner) = ((width / 2, height / 2), (3, 3));
    let (plain, balanced) = (&kept.0[0], &kept.0[1]);
    assert_eq!(
        at(plain, centre.0, centre.1),
        at(balanced, centre.0, centre.1),
        "the overlay as given"
    );
    let (a, b) = (
        at(plain, corner.0, corner.1),
        at(balanced, corner.0, corner.1),
    );
    let ratio = |p: [u8; 3]| f64::from(p[0]) / f64::from(p[2].max(1));
    assert!(ratio(b) > ratio(a) * 1.1, "warmer: {a:?} then {b:?}");
    assert!(b[0] < a[0], "darker: {a:?} then {b:?}");
}

/// Every picture's RGBA and I420, kept; the RGBA asked for or not.
struct Both {
    rgba: bool,
    pictures: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Sink for Both {
    fn wants_i420(&self) -> bool {
        true
    }
    fn wants_rgba(&self) -> bool {
        self.rgba
    }
    fn picture(&mut self, _: u32, _: u32, rgba: &[u8], i420: &[u8]) -> Result<(), Error> {
        self.pictures.push((rgba.to_vec(), i420.to_vec()));
        Ok(())
    }
}

/// The frame's picture, as an observer is handed it, and whether it was.
#[derive(Default)]
struct Looks(Vec<Option<Vec<u8>>>);

impl tuile_film_native::Observer for Looks {
    fn frame(&mut self, frame: &tuile_film_native::FrameOut<'_>) {
        self.0.push(frame.rgba.map(<[u8]>::to_vec));
    }
    fn wants_picture(&self) -> bool {
        true
    }
}

/// A sink of I420 alone, under no observer that looks at the picture, is
/// handed no RGBA — it is never read back — and the very same I420, byte
/// for byte, mono and stereo; an observer that looks is handed the
/// picture, the same as a sink that reads it.
#[tokio::test(flavor = "multi_thread")]
async fn the_picture_is_read_back_only_for_what_reads_it() {
    if !has_adapter() {
        eprintln!("no GPU adapter: skipped");
        return;
    }
    let dir = tempfile::tempdir().expect("dir");
    let (sources, film) = test_film(dir.path()).await;
    let top_bottom = Stereo {
        packing: tuile_film::Packing::TopBottom,
        ..Stereo::default()
    };
    for stereo in [None, Some(top_bottom)] {
        let order = Order {
            supersample: 1,
            tone: Tone::Off,
            stereo,
            ..Order::default()
        };
        let mut read = Both {
            rgba: true,
            pictures: Vec::new(),
        };
        render(&sources, &film, &order, &mut (), &mut read, &mut ())
            .await
            .expect("render");
        let mut unread = Both {
            rgba: false,
            pictures: Vec::new(),
        };
        render(&sources, &film, &order, &mut (), &mut unread, &mut ())
            .await
            .expect("render");
        assert_eq!(read.pictures.len(), FRAMES as usize, "{stereo:?}");
        assert_eq!(unread.pictures.len(), FRAMES as usize, "{stereo:?}");
        for ((rgba, i420), (none, same)) in read.pictures.iter().zip(&unread.pictures) {
            assert!(!rgba.is_empty() && !i420.is_empty(), "{stereo:?}");
            assert!(none.is_empty(), "{stereo:?}: RGBA read back for nothing");
            assert!(i420 == same, "{stereo:?}: the I420 moved");
        }
        // An observer that looks: it, and only it, is handed the picture.
        let mut looks = Looks::default();
        let mut unread = Both {
            rgba: false,
            pictures: Vec::new(),
        };
        render(&sources, &film, &order, &mut (), &mut unread, &mut looks)
            .await
            .expect("render");
        let seen: Vec<Vec<u8>> = looks.0.into_iter().map(|p| p.expect("a picture")).collect();
        let sunk: Vec<&Vec<u8>> = read.pictures.iter().map(|p| &p.0).collect();
        assert!(seen.iter().eq(sunk), "{stereo:?}: the observer's picture");
    }
}
