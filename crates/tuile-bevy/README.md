<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-bevy

The streamed globe of tuile — OGC 3D Tiles terrain with imagery draped on
it — inside a Bevy application, drawn on the application's own device,
into its own colour and depth, sharing depth with everything else it
draws.

Nothing of the ground is ported. tuile-wgpu's renderer and its graphics
budget's executor run on Bevy's `RenderDevice` and `RenderQueue`, from a
system in Bevy's 3D schedule, through `tuile_wgpu::HostPass`: one ground
shader and one set of rules against black ground, shared with every other
tuile client.

```no_run
use bevy::prelude::*;
use tuile_bevy::{GlobeSources, TuileCamera, TuilePlugin, XyzTemplate};

let terrain = "http://127.0.0.1:8092/terrain/layer.json".parse().expect("a URL");
let imagery = XyzTemplate::geographic("http://127.0.0.1:8092/imagery/{z}/{x}/{y}.png");
App::new()
    .add_plugins(DefaultPlugins)
    .add_plugins(TuilePlugin::new(GlobeSources::addresses(terrain, imagery)))
    .add_systems(Startup, |mut commands: Commands| {
        commands.spawn((
            Camera3d::default(),
            TuileCamera::looking(10.0, 0.0, 3_000.0, 0.0, 45.0, 35.0),
        ));
    })
    .run();
```

# What the host does

- **Drives the camera** — a [`TuileCamera`] on its `Camera3d`, an `f64`
  pose the plugin turns into the camera's transform and lens every frame
  (see [`camera`] for the floating origin under it).
- **Places its own entities** with a [`GeoAnchor`], in `f64`, and draws
  them in its own pipelines: Bevy's main pass runs after the globe's and
  is depth-tested against the ground.
- **Stands its units on the ground and picks it** with [`TuileGround`]:
  heights from the drawn triangles, batched, and screen rays — see
  [`ground`].
- **Says what it will need** with [`PreloadViews`]: poses traversed this
  frame and never drawn.
- **Sets the sky** with a [`TuileAtmosphere`]: on or off, where the light
  comes from ([`Lights`]: the real sun and Moon, a world's own day, or
  directions it sets), the air, the clock — and reads back where the sun
  is for its own lights. See [`atmosphere`].
- **Sizes the globe's share** of the device with a [`TuileBudget`], and
  chooses whether imagery is held compressed
  ([`ImageryCompression`], off unless asked).

# In a browser

The same plugin, on WebGPU or WebGL2 (one Bevy build each). The session
runs on the page's thread and decodes in tuile's web decode workers: see
`web` (wasm32 only) for the three things the page does. On WebGL2 a camera draws
single-sampled (`Msaa::Off`): it can neither read a multisampled depth nor
shade per sample, which sharing a stencil-less depth needs.

# Logging

The engine traces every tile decision under the target `tuile_det`, at
`info`, for determinism checks. A host's console wants
`tuile_det=off` in its `LogPlugin` filter.

The camera's depth must be bindable for the ground to share it; the plugin
adds `TEXTURE_BINDING` to a [`TuileCamera`]'s `Camera3d` itself. Its
perspective's `far` is the host's frustum culling distance for its own
meshes and nothing else: Bevy's projection has no far plane.
