// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The ground material: the imagery mosaic, blended in one pass.
//
// A transposition of `crates/tuile-wgpu/src/shader.wgsl` rather than a new
// design — deliberately, and to the line. This page exists to disagree with the
// native viewer only where a *renderer* differs; a mosaic composited by some
// other rule would make every visual difference ambiguous, which is exactly the
// property that makes a differential test worthless.
//
// What the engine sends per layer, and what this consumes:
//
//   coverage    [u_min, v_min, u_max, v_max]  in the TILE's uv space
//   placement   [translation.xy, scale.xy]    texture_uv = tile_uv * scale + translation
//
// A layer finer than the tile covers a sub-rectangle of it and the others cover
// the rest; a layer coarser than it covers all of it. Outside its coverage a
// layer contributes nothing, and masking on that is what lets several layers be
// blended in one pass without bleeding into each other.

import * as THREE from "three";

/// Slots in the mosaic, taken from the table the engine sends.
///
/// Not a constant any more. It used to be `12`, hand-copied from
/// `tuile_core::raster::MAX_IMAGERY_LAYERS` with a comment saying it must
/// match — and nothing checked that it did. The engine now ships the packed
/// table itself, two `vec4` per slot, so the count is a property of the data
/// rather than a promise in a comment.
function slotsIn(table) {
  return table.length / 8;
}

const VERTEX = /* glsl */ `
  varying vec2 vUv;
  varying vec3 vNormal;

  void main() {
    vUv = uv;
    // Normals are already in the tile's local frame, and the model matrix is a
    // pure translation (the rebase) — so no normal matrix is needed, and using
    // one would only spend precision.
    vNormal = normal;
    gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
  }
`;

const fragment = (slots) => /* glsl */ `
  precision highp float;

  varying vec2 vUv;
  varying vec3 vNormal;

  uniform sampler2D uLayers[${slots}];
  uniform vec4 uCoverage[${slots}];
  uniform vec4 uPlacement[${slots}];
  uniform vec3 uBaseColor;
  uniform vec3 uSunDir;
  uniform float uAmbient;

  // Blends one layer over what is already there.
  //
  // Branch-free, for the same two reasons as the native shader. The coverage
  // test uses step() rather than an if, because a discarded layer must still
  // not diverge the control flow the texture fetch sits in; and an unused slot
  // needs no test of its own, because it carries an empty coverage rectangle
  // and so masks itself out — which is why unused slots are bound to a 1x1
  // white texture rather than left dangling. That costs a guaranteed cache hit
  // and keeps this straight-line.
  vec3 blendLayer(vec3 dst, sampler2D tex, vec2 uv, vec4 coverage, vec4 placement) {
    vec2 above = step(coverage.xy, uv);
    vec2 below = step(uv, coverage.zw);
    float mask = above.x * above.y * below.x * below.y;
    // Outside the coverage rectangle this reads past the layer's own edge and
    // the clamping sampler smears it — which never shows, because that is
    // exactly where the mask is zero.
    //
    // texture2D, not texture: this is GLSL ES 1.00, which is what THREE
    // compiles a ShaderMaterial as unless told otherwise, and it is the version
    // that goes with the varying/gl_FragColor used above. texture() belongs to
    // ES 3.00 and does not exist here — mixing the two compiles nowhere.
    vec4 texel = texture2D(tex, uv * placement.zw + placement.xy);
    return mix(dst, texel.rgb, mask * texel.a);
  }

  void main() {
    vec3 ground = uBaseColor;

    // Unrolled, like the native shader, and generated rather than typed: a
    // sampler array indexed by a loop variable is only legal with a
    // constant-expression index, so every index has to be a literal. Written by
    // hand it was twelve lines that had to be kept in step with a count living
    // in another language.
${Array.from(
      { length: slots },
      (_, i) => `    ground = blendLayer(ground, uLayers[${i}], vUv, uCoverage[${i}], uPlacement[${i}]);`,
    ).join("\n")}

    vec3 n = normalize(vNormal);
    float lambert = max(dot(n, -uSunDir), 0.0);
    gl_FragColor = vec4(ground * (uAmbient + (1.0 - uAmbient) * lambert), 1.0);
  }
`;

/// The 1x1 white texture unused slots sample.
///
/// One instance for the whole page: it is bound in eleven slots of most tiles,
/// and allocating it per material would be thousands of identical single-pixel
/// textures.
let whitePixel = null;
function white() {
  if (!whitePixel) {
    whitePixel = new THREE.DataTexture(new Uint8Array([255, 255, 255, 255]), 1, 1, THREE.RGBAFormat);
    whitePixel.needsUpdate = true;
  }
  return whitePixel;
}

/// The colour a tile is drawn in where **no layer covers it**.
///
/// Not black. A black quad on good geometry is visually identical to a
/// rendering fault and gets argued about as one; this says "the imagery is not
/// here yet" in a way nobody mistakes for a bug.
const BARE_GROUND = [0.55, 0.95, 0.55];

/// Builds the material for one tile from the layers the engine sent.
///
/// `resolve(coord)` returns the page's texture for an imagery coordinate, or
/// `null` if its pixels have not crossed yet — layers cross once per
/// coordinate, so a tile drapes textures it never received itself.
/// Builds the material for one tile from the table the engine packed.
///
/// `table` is `imagery_layer_table`'s output, flattened: two `vec4` per slot,
/// coverage then placement, already padded to the full slot count with an
/// empty rectangle. Nothing here decides how many slots there are, what an
/// unused one looks like, or what an identity placement is — those were four
/// hand-written copies of rules the engine already states, and every one of
/// them could drift without a test noticing.
export function groundMaterial({ table, emptyCoverage, layers, resolve, sunDir }) {
  const slots = slotsIn(table);
  const textures = [];
  const coverage = [];
  const placement = [];

  for (let i = 0; i < slots; i++) {
    const texture = layers?.[i] ? resolve(layers[i].coord) : null;
    textures.push(texture ?? white());
    // The engine's rectangle, unless the texture has not decoded yet — a slot
    // pointing at the white 1x1 would paint white ground over good imagery, so
    // it is masked out with the engine's own sentinel rather than a local
    // guess at one. `[0,0,0,0]` would not do: it passes at exactly uv (0,0),
    // which is a corner every tile has.
    const off = 8 * i;
    coverage.push(
      texture
        ? new THREE.Vector4(table[off], table[off + 1], table[off + 2], table[off + 3])
        : new THREE.Vector4(...emptyCoverage),
    );
    placement.push(
      new THREE.Vector4(table[off + 4], table[off + 5], table[off + 6], table[off + 7]),
    );
  }

  return new THREE.ShaderMaterial({
    uniforms: {
      uLayers: { value: textures },
      uCoverage: { value: coverage },
      uPlacement: { value: placement },
      uBaseColor: { value: new THREE.Vector3(...BARE_GROUND) },
      uSunDir: { value: sunDir.clone() },
      uAmbient: { value: 0.35 },
    },
    vertexShader: VERTEX,
    fragmentShader: fragment(slots),
  });
}
