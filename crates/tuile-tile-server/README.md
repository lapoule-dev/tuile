<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-tile-server

Source tiles, kept once.

A tile server's job, minus the HTTP: terrain and imagery tiles as their
source delivered them, stored in PMTiles archives on an object store, one
set of archives per **zone** and per **layer**, served back by tile address.

- [`grid`] maps a source's own addressing onto an archive's quadtree.
- [`layer`] says how a layer is split into zones and cut into epochs.
- [`archive`] writes and reads one immutable archive.
- [`catalog`] is the list of layers, kept in the bucket itself.
- [`content`] makes the store the engine's tile cache.
- [`manifest`] is the list of archives a zone is made of, replaced by
  conditional writes.
- [`store`] puts it together: a mutable buffer in memory, deltas, a
  streaming compaction, and cleanup — safe with any number of writers.
- [`disk`] keeps whole archives on local disk, so a zone's second tile is
  local.
- [`index`] says what an archive holds without its bytes: its tile ids
  and a stamp of each, told to the host's register as it is published.
- [`absences`] is where what a source does not have is kept.
- [`ledger`] is what a register is told, and what it tells a reader back;
  [`register`] is one in memory, and the reference of what one does.
- [`lru`] is the budgeted least-recently-used map the caches share.
- [`projection`] takes a scene's slice of the store to local archives.
- [`tiering`] picks what a compaction merges: similar sizes, contiguous.
- [`service`] adds the upstream: fetched once, stored, served, the hot
  tiles and the source's absences kept in memory.
- [`derived`] makes a layer from another one: each tile the parent of the
  source's four children one level finer, downsampled in linear light and
  encoded as WebP, served and stored like any layer's tiles, and made again
  when a child changes. Declared in the catalog by `derived_from`.

No router and no authentication here: those belong to whoever deploys it.

# Environment

Concurrency limits, each read once per process, a missing, unparsable or
zero value keeping the default:

- `TUILE_PROJECTION_ZONES` (16): zones a projection works on at once.
- `TUILE_PROJECTION_RANGES` (8): byte ranges of one zone a projection
  fetches at once.
- `TUILE_FLUSH_ZONES` (16): zones a store flushes at once.
- `TUILE_UPLOAD_PARTS` (8): parts of one archive upload in flight.
- `TUILE_DEDUP` (32): tiles of one publication checked against the store
  at once.
- `TUILE_EXTRACT_ZONES` (16): zones an extract reads at once.
- `TUILE_EXTRACT_TILES` (32): tiles of one archive an extract reads at once.
- `TUILE_EXTRACT_PUT_PARTS` (8): parts of an extract's upload in flight.

## Where it sits

Depends on `tuile-core` (the `ContentStore` the [`content`] module
implements). Used by `tuile-bake`, which fills it, and by `tuile-repository`
and `tuile-pack-api`, which read and serve it.

## Example

```rust
use tuile_tile_server::grid::Grid;

// A geographic layer is stored one level down in the archive's quadtree.
let id = Grid::Geographic.to_id(3, 10, 2)?;
assert_eq!(Grid::Geographic.from_id(id), Some((3, 10, 2)));
# Ok::<(), tuile_tile_server::grid::OutOfGrid>(())
```

## Features

None.

## Documentation

- [`docs/12-crate-server.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/12-crate-server.md)
- [`docs/22-film-jobs.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/22-film-jobs.md)
