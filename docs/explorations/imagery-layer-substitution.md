<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Imagery layer substitution — a fallback specification by zoom gap

Status: design and implementation, 20 Nov. 2026 (Laurent: *"tuile doit avoir la
capacité de charger une layer à la place d'une autre en fonction de son écart de
zoom"*; precised the same day: the behaviour is **optional**, a fallback
specification a host declares, and everything it does is measured).

## 1. What it is for

Some imagery sets are not one acquisition. Their coarse levels come from another
mosaic, with other colours: the near ground is one picture and the far ground,
drawn from the coarse levels, another. A server can publish a **derived layer**
that rebuilds a coarse level from the finer one below it (a level-`z` tile is the
2×2 linear-light downsample of the four level-`z+1` tiles; absent where a child is
absent). Drawn far away, that layer keeps the near colours.

So a client wants **near tiles from the source layer and far tiles from the
substitute layer**. tuile must be able to draw an imagery layer's tiles from a
substitute layer depending on how far, in levels, a tile is from the finest
imagery drawn near the camera.

Nothing here names a provider or a server. A substitute is *any* imagery source
tuile knows (a URL template, a catalog entry, a repository source); the derived
"+1" layer is one possible substitute.

## 2. Optional, and declared: a fallback specification

**The default is no substitution.** tuile behaves exactly as before — the same
requests, the same decisions, byte-identical goldens — and nothing in tuile
expects a substitute layer to exist. Not everybody has a server offering one.

A host activates it by **declaring a fallback specification**: a value, in the
Specification vocabulary of the eviction/prefetch decider
(`budget-eviction-prefetch-spec.md`, §3): integers only, `Eq + Hash`, validated
once, with a serial query form, recorded as a step input, replaceable while
running. Removing it (the empty specification) returns to the default.

```rust
/// tuile_budget::fallback
pub struct FallbackSpec { pub rules: Vec<FallbackRule> }   // empty = none

/// "Dress layer `layer` from its substitute where the tile is at least `gap`
/// levels coarser than the reference level."
pub struct FallbackRule {
    pub layer: LayerId,       // the layer it applies to (a base layer)
    pub gap: u32,             // G: levels below the reference level, ≥ 1
    pub hysteresis: u32,      // H: levels of hysteresis, on the reference and per tile
    pub failures: u32,        // consecutive failures before backing off
    pub backoff: Micros,      // first back-off; doubles, capped at 64×
}
```

- Serial form: `layer=0&gap=4&hysteresis=1` (one rule; several rules joined by
  `;`), the same grammar style as `tuile://budget?…`. `FallbackSpec::from_query`
  and `to_query` round-trip; `validate()` bounds every number (`1 ≤ gap ≤ 16`,
  `hysteresis < gap`, `≤ 8` rules, back-off ≤ 10 min).
- It is a **composable rule the decider evaluates**: when its spec gains a
  `fallbacks: FallbackSpec` field (once `spec.rs` lands on `main`), the rules move
  there unchanged. Until then the host hands it to the session with
  `ClientMessage::Fallbacks(FallbackSpec)` — a message like `Pressure` or
  `Retarget`, in order with the views, recorded with the step events
  (`BudgetEvent` stream), so a session replays.
- The rule is evaluated by `tuile_budget::fallback::FallbackState::apply`, a pure
  function of the spec, the need, the step time and its own state — no clock, no
  float that is not quantised: the same everywhere (native, wasm).

A **substitute source** is separate from the rule: it is given to the loader that
can fetch it (`GlobeOptions::substitute`). A loader with no substitute source
reports none in `DrapeReach::substitutes`, and a rule over a layer that has no
substitute changes nothing. So a host may ship the rule and the source together
(the usual case) or the source alone, switched on later.

## 3. The model: which resource a tile wants

The choice of layer is **an input to the want computation**, not a mode. A tile's
need lists the imagery layers it wears (`ImageryNeed::layers`, one `LayerReach`
per layer). A rule rewrites a far tile's entry for `layer` into the entry of its
**substitute layer** — `layer.substitute()`, `LayerId(layer.0 | 0x8000)` — before
the budget reads the need. From there on everything is the budget's ordinary
machinery:

- the substitute drape is its **own resource**,
  `ResourceId::Drape { tile, layer: L.substitute(), level }`: its own identity in
  the arena, the ledger, the GPU pool (`ImageryLayer::source` carries the
  substitute bit, so `(source, coord)` differs) and the drape identity (the layer
  is mixed in). It is charged, evicted, refreshed (its own layer epoch) like any
  drape;
- a layer id with the bit set sorts **right after its base layer** (`LayerId`'s
  order is `(base, substitute?)`): the stacking order of overlays is untouched.

### 3.1 The reference level

The **reference level** `R` of a layer is the finest imagery level drawn near the
camera: the maximum, over the tiles **drawn** this step (`Membership::Drawn`) that
wear the layer, of the level the tile needs **at scale 1** —
`imagery::needed_level(None, footprint, clamp)` on the unrelaxed footprint,
clamped to `[pinned_level, terrain_level + imagery_cap] ∩ [0, source max]`.

- The footprint is the one the need already carries: the finest any view gives the
  tile (spec §13.3). So `R` is defined over all the views of the session at once —
  both eyes of a stereo pair, which see the same near ground. A tile near *any*
  eye is near.
- Unrelaxed, so a pressure relaxation (which coarsens every tile together) does not
  move a tile from one layer to the other.
- Integers and comparisons against powers of two only (`needed_level`): no `log2`,
  the same level on every platform.

A tile's **gap** is `R − ℓ(t)`, `ℓ(t)` its own level computed the same way.

### 3.2 The rule, and its hysteresis

`G = gap` (**default 4**): a tile is drawn from the substitute when its gap is at
least `G` — its texels are at least `2^G` (16×) larger than the nearest ground's,
far enough out that it is a band of distant ground, never the ground under the
camera. `G` is a configuration number, not a switch: a larger `G` substitutes less
of the view, `G = 1` everything but the finest level.

Two hystereses, both `H = hysteresis` levels (**default 1**):

- **On `R`.** `R` rises at once when the measured maximum `m` rises, and falls only
  when `m < R − H` (then to `m`). A camera hovering where the nearest tile hesitates
  between two levels keeps one `R`.
- **Per tile.** A tile enters the substitute at `gap ≥ G` and leaves it only at
  `gap < G − H`. A tile at the boundary does not alternate.

**Batch has no hysteresis**: a film's choice is a function of its frame
(`R` = the near level of that frame, entry exactly at `gap ≥ G`), so a frame
baked twice, or out of order, names the same tiles.

Further limits, all in favour of the source:

- **The floor is never substituted** (`level ≤ pinned_level`): the pinned floor is
  the guarantee against black ground and must not depend on an optional server.
- A tile whose level is past the substitute's own `max_level` keeps the source.
- A tile that wears no level (no imagery need) is untouched.

### 3.3 Never black ground

The fallback chain is the budget's and stays whole:

- **A tile waiting for its substitute is dressed by whatever is held**: its own
  source drape, or an ancestor's drape *of either layer of the family*.
  `best_drape_for` ranks the candidates of the base layer and its substitute
  together — the ordinary rank (coarsest at or above the need, else sharpest,
  nearest owner) — and the wanted layer wins a tie. Without a rule no substitute
  drape exists, so every answer is the one it was.
- **New before old.** While a tile switches (source → substitute or back), it
  wears the drape it had until the drape it now wants is drawable — the same
  "stand-in from the family" rule — and the old one is kept from eviction because
  it is drawn. The want for the new drape is made (a stand-in from the other layer
  of the family is always "off" in `tile_wants`), so the switch completes.
- The floor drape under every drape comes from the **source** layer (see §4).

### 3.4 Absent and unreachable substitutes

- **Absent (404, or the provider's "no imagery" picture).** Exactly like an absent
  tile: the loader stands on the substitute's parent, then its parent, up to the
  substitute's minimum level, and the drape is composed from that ancestor. If the
  whole substitute ancestry is absent, the mosaic tile is left out and the
  **source floor** under the drape covers the ground. **The source layer is not
  retried for that tile** — the rule said "substitute here", and a mosaic of two
  acquisitions inside one tile is the patchwork the substitute exists to avoid; the
  floor is the safety net, not a retry. Counted:
  `tuile_layer_ancestor_fallbacks_total{layer_role="substitute"}`.
- **Unreachable (a transport error, not an answer).** The drape load fails, the
  tile keeps what it wears (never black), and the budget's retry back-off applies.
  The fallback state counts consecutive failures of substitute drapes; after
  `failures` (default 3) the rule is **suspended** for `backoff` (default 5 s,
  doubling to 64× while it keeps failing): tiles return to the source, so an
  unreachable substitute never degrades the default path. The first success
  resets the count. Counted: `tuile_layer_backoffs_total`,
  `tuile_layer_fallback_suspended`.

## 4. The loader

`tuile_planetary::GlobeOptions::substitute: Option<SubstituteImagery>` — an
`Arc<dyn ImageryProvider>`, any provider: a URL template, a catalog asset, a
repository source, wrapped in the host's cache like the source. The loader keeps
a second **lane** for it: its own decoded cache, its own in-flight table, its own
provenance entries, its own generation. `load_drape(tile, L.substitute(), level)`
composes from that lane: the mosaic from the substitute, the floor from the source
(the pinned floor is one set of tiles, already held). `holds()` asks the lane's
store for its tiles. `drapes()` reports, beside the layers, the substitute's reach
(`DrapeReach::substitutes`).

No substitute configured: the lane does not exist, `substitutes` is empty, and not
one request goes to any substitute (tested).

## 5. Surfaces

| Host | Where | Shape |
|---|---|---|
| Viewer config file | `[[ion.imagery]]` keys `substitute = "<key of another layer>"`, `substitute_gap`, `substitute_hysteresis` | the substitute is another layer of the list (catalog asset), stored under its own namespace |
| Web options (same code as desktop) | `sources.imagery[i].substitute = { "layer": "<key>", "gap": 4, "hysteresis": 1 }` | the referenced layer may be a catalog asset or a URL template |
| A host's catalog (`ViewerHost::imagery`) | `ImageryChoice::with_substitute(SubstituteChoice { asset, cache, gap, hysteresis })` | — |
| Viewer at runtime | steering `tuile://fallback?gap=4&hysteresis=1`, `tuile://fallback?show=off`; switching layers sends the new layer's specification | a `ClientMessage::Fallbacks` on the stream; web crosses the worker wire (tag 8) |
| Bevy | `GlobeSources::substitute: Option<Substitute { imagery: ImagerySpec, rule }>` | resolved like the imagery; the rule sent before the first view |
| Bake / film | `GlobeConfig::with_substitute(asset, rule)` — `substitute_asset_id` resolved through the host's `Sources` (refused without them), and `SessionConfig::fallbacks` | the session sends the rule before the first frame |

The film path names substitute tiles **by reference**: a pack's `ImageryRef` whose
`level` has bit 7 set (`StoreTile::SUBSTITUTE`, `0x80 | level`) is a tile of the
scene's **substitute** imagery store, with its digest from the substitute lane's
provenance. Levels are below 32, so the bit was free; a pack baked without a
substitute is byte-identical. `tuile_film::from_store` reads the level without
the bit and stamps the layer as a substitute.

**Not done in this change (follow-up):** the pack's header names one imagery store
(`store_layers`); a render by reference (`tuile-film-native`) still has to be
given the substitute's store and look a tile with the bit up there. Until then a
pack with substitute tiles renders with a digest mismatch on those tiles — loud,
never a wrong picture. The `tuile-bake` command line has no `--substitute` flag
yet: the film product configures it through the library (`GlobeConfig`).

## 6. Budget compatibility (eviction/prefetch decider)

The decider being designed in parallel (`budget-eviction-prefetch-spec.md`) values
resources; it never decides *which* resource a place wants — the need does. A
substitute is a different resource of the same place and content (`c` in §1 of
that spec: "one drape per imagery layer" — the family is one content). So:

- the deficit `need_c(i) − worn_c(i)` reads the **level**, whichever layer of the
  family is worn: a tile wearing its source drape while its substitute loads at the
  same level has no level deficit, and the want for the substitute is made by the
  "stand-in is off" rule above (a layer deficit, not a level one);
- eviction, prefetch bands, protection and law 1 apply to substitute drapes as to
  any drape; the floor (law 2) is the source's.

When `Specification` lands, `FallbackSpec` becomes its `fallbacks` field, its
digest is hashed with the specification's, and `ClientMessage::Fallbacks` becomes
part of `FrameInput::with_spec`.

## 7. Telemetry

One label scheme with the decider's (`observe::labels`): `layer_role`, `level`,
`gap`, `outcome`, `reason`. `layer_role` is a closed set of two values, `source`
and `substitute` — the *role* of a layer, never its name — added to
`tuile_telemetry::resource::ALLOWED_LABELS`. Not `layer`: the tile server labels
its series by layer **name** (`tuile_tiles_*{layer=…}`), and allow-listing that
key would send those names out. Bounded: 2 × 25 levels at most, 2 × 17 gaps.

| Series | Type | Labels | What |
|---|---|---|---|
| `tuile_layer_requests_total` | counter | `layer_role` | imagery tiles asked of the provider |
| `tuile_layer_responses_total` | counter | `layer_role`, `outcome` = `hit` \| `absent` \| `error` | their answers |
| `tuile_layer_bytes_total` | counter | `layer_role` | bytes received |
| `tuile_layer_fetch_seconds_{count,sum,peak}` | timer | `layer_role` | latency |
| `tuile_layer_ancestor_fallbacks_total` | counter | `layer_role` | absent tiles stood on an ancestor |
| `tuile_by_level{series="drapes_source"\|"drapes_substitute"}` | gauge | `level` | drapes drawn, by layer and level |
| `tuile_layer_switches_total` | counter | `reason` = `to_substitute` \| `to_source`, `gap` | substitution decisions, with the gap |
| `tuile_layer_backoffs_total` | counter | | suspensions of an unreachable substitute |
| `tuile_layer_fallback_suspended` | gauge | | 1 while suspended |
| `tuile_layer_fallback_active` | gauge | | 1 when a fallback specification is in force |
| `tuile_layer_reference_level` | gauge | | `R` of the first rule |

Native and web publish the same series through `tuile_metrics::publish`, the
registry and the OTLP encoder (tested against a fake receiver).

## 8. Known limits

- The substitute lane's store presence is not followed by the store log
  (`TileLoader::store_events`): a presence answer about a substitute drape can be
  stale, which costs a load from the wrong quota, never a picture.
- The viewer's switch-progress check reads a tile's imagery generation off its
  source tiles only (a substitute drape's floor is the source's), so a layer
  switch is seen to finish whether or not the far tiles wear a substitute.
- `R` is one per session over all views (§3.1); two views of unrelated places in
  one session would share it.

## 9. Tests

Red first (each reverted behaviour fails its test):

- substitute chosen exactly when `gap ≥ G` (and only for a tile with a substitute,
  above the floor, within the substitute's reach);
- hysteresis: a camera oscillating at the boundary does not flip a tile; `R` does
  not fall on a one-level dip;
- absent substitute → ancestor of the substitute, then the source floor; never an
  empty drape;
- swap source ↔ substitute never leaves a step without a drape on the tile
  (budget simulation, both directions);
- the budget charges substitute drapes separately (`ResourceId` with the
  substitute layer in the ledger, distinct from the source drape);
- default spec: no substitute request at all, decisions digest unchanged;
- activating and deactivating at runtime, in-process (native) and across the
  worker wire (web: `ClientMessage::Fallbacks` round-trips);
- Batch: no hysteresis, a function of the frame; the bake names substitute tiles
  in the pack with the substitute bit.
- an unreachable substitute backs off and returns tiles to the source.

Goldens: unchanged with no substitute configured (byte-identical).
