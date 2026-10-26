// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Every shader of the interactive path, held to WebGL2.
//!
//! A browser without WebGPU runs this renderer on wgpu's GL backend, which
//! translates WGSL to GLSL ES 3.00 and runs it under WebGL2's limits. Nothing
//! on a desktop exercises that translation, so a shader that leaves it — a
//! storage buffer, a compute stage, one sampler too many — would compile and
//! render everywhere a developer looks and fail only in someone's browser.
//!
//! So each module is parsed, validated with no optional capability at all,
//! and written out as GLSL ES 3.00 for WebGL, entry point by entry point; and
//! the ground shader at the slot budget a WebGL2 device reports is held to
//! WebGL2's sixteen texture units per fragment stage.

/// The texture units a WebGL2 fragment stage is guaranteed: `MAX_TEXTURE_IMAGE_UNITS`.
const WEBGL2_TEXTURE_UNITS: u32 = 16;

/// Parses, validates and translates `source` for WebGL2; returns the GLSL of
/// each entry point.
fn webgl2(name: &str, source: &str) -> Vec<String> {
    let module = naga::front::wgsl::parse_str(source)
        .unwrap_or_else(|e| panic!("{name}: {}", e.emit_to_string(source)));
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    )
    .validate(&module)
    .unwrap_or_else(|e| {
        panic!(
            "{name} needs more than WebGL2 offers: {}",
            e.emit_to_string(source)
        )
    });
    let options = naga::back::glsl::Options {
        version: naga::back::glsl::Version::Embedded {
            version: 300,
            is_webgl: true,
        },
        ..Default::default()
    };
    assert!(!module.entry_points.is_empty(), "{name}: no entry point");
    module
        .entry_points
        .iter()
        .map(|entry| {
            assert_ne!(
                entry.stage,
                naga::ShaderStage::Compute,
                "{name}: a compute stage cannot run on WebGL2"
            );
            let pipeline = naga::back::glsl::PipelineOptions {
                shader_stage: entry.stage,
                entry_point: entry.name.clone(),
                multiview: None,
            };
            let mut glsl = String::new();
            naga::back::glsl::Writer::new(
                &mut glsl,
                &module,
                &info,
                &options,
                &pipeline,
                naga::proc::BoundsCheckPolicies::default(),
            )
            .and_then(|mut writer| writer.write().map(|_| ()))
            .unwrap_or_else(|e| panic!("{name}/{}: not GLSL ES 3.00: {e}", entry.name));
            glsl
        })
        .collect()
}

/// The ground shader, at every slot budget a device can produce — the WebGL2
/// one included — the overlay, and the wireframe lines.
#[test]
fn every_viewer_shader_runs_on_webgl2() {
    for slots in [
        tuile_core::raster::MIN_IMAGERY_SLOTS,
        webgl2_slots(),
        tuile_core::raster::USEFUL_IMAGERY_SLOTS,
    ] {
        webgl2(
            &format!("ground ({slots} slots)"),
            &crate::renderer::ground_wgsl(slots),
        );
    }
    webgl2("overlay", include_str!("overlay.wgsl"));
    // A stencil-less host's depth, into tuile's companion and back: one
    // sample a pixel there, `frag_depth` written from a depth texture.
    webgl2("depth crossing", &crate::host::crossing_wgsl(false));
    webgl2("lines", include_str!("lines.wgsl"));
    // The physical atmosphere: the sky past the ground, in either depth
    // convention, and its sky-view tables drawn by a render pass — WebGL2
    // has no compute to make them in.
    webgl2("sky", &crate::renderer::sky_wgsl(1.0));
    webgl2("sky (reverse depth)", &crate::renderer::sky_wgsl(0.0));
    webgl2(
        "sky view",
        &format!(
            "@group(0) @binding(0) var<uniform> atmosphere: Atmosphere;\n\
             @group(0) @binding(1) var transmittance_lut: texture_2d<f32>;\n\
             @group(0) @binding(2) var multi_scattering_lut: texture_2d<f32>;\n\
             @group(0) @binding(6) var irradiance_lut: texture_2d<f32>;\n\
             @group(0) @binding(7) var night_lights_lut: texture_2d<f32>;\n\
             @group(0) @binding(8) var lut_sampler: sampler;\n\
             @group(0) @binding(9) var star_table: texture_2d<f32>;\n\
             {}\n{}\n{}",
            tuile_atmosphere::hillaire::WGSL,
            tuile_atmosphere::gpu::SKY_VIEW_WGSL,
            tuile_atmosphere::gpu::SKY_VIEW_RENDER_WGSL
        ),
    );
    // The frame's light, made every frame, drawn by two render passes.
    webgl2(
        "frame light",
        &tuile_atmosphere::frame_light::wgsl(tuile_atmosphere::gpu::SkyViewPass::Render),
    );
    // A host's shapes in the world, with the light it shares with the film:
    // the same text the film compiles, so a WebGL2 browser draws the HUD.
    webgl2(
        "world overlay",
        &[
            tuile_overlay::gpu::LIGHT_WGSL,
            include_str!("world_overlay.wgsl"),
        ]
        .concat(),
    );
}

/// The imagery slots a WebGL2 device gets: its units, less the physical
/// atmosphere's tables, as [`crate::GpuContext::new`] counts them.
fn webgl2_slots() -> u32 {
    tuile_core::raster::imagery_slots(WEBGL2_TEXTURE_UNITS - tuile_atmosphere::gpu::GROUND_TEXTURES)
}

/// At the budget WebGL2 reports, the ground's fragment stage fits the
/// texture units WebGL2 guarantees: the imagery slots, the tile's own base
/// colour and the atmosphere's tables, each a sampler of its own once
/// translated.
#[test]
fn the_ground_fits_webgl2_texture_units() {
    let slots = webgl2_slots();
    let glsl = webgl2("ground", &crate::renderer::ground_wgsl(slots));
    let fragment = glsl
        .iter()
        .find(|g| g.contains("out vec4") || g.contains("gl_FragColor"))
        .expect("a fragment stage");
    let samplers = fragment.matches("uniform highp sampler2D").count() as u32;
    assert!(
        samplers <= WEBGL2_TEXTURE_UNITS,
        "{samplers} samplers in the fragment stage; WebGL2 guarantees {WEBGL2_TEXTURE_UNITS}"
    );
    assert!(samplers > slots, "every imagery slot is a sampler");
}
