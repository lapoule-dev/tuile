// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The ground a frame's selection draws, built off the session.
//!
//! A camera kept over the ground is placed against the ground the film
//! draws: the selection's terrain meshes, prepared and stitched as the
//! renderer does ([`tuile_film::ground`]). Building that is CPU work on
//! meshes the session already holds, and nothing in it reads the session
//! again: so a frame's selection is frozen ([`Selected`]) as soon as it has
//! converged, and its ground is built from that ([`GroundDrawer`]) — on
//! another thread, while the session converges the next frame, its tiles
//! on as many threads as there are.
//!
//! # A function of the selection
//!
//! The ground of a selection is the same whichever drawer builds it and
//! whatever it built before: a tile is prepared from its own mesh, planned
//! from the selection's tiles, and drawn from its mesh and its plan. What a
//! drawer keeps from one ground to the next — tiles it prepared, tiles it
//! drew, the places of their vertices — is only what it would build again
//! identically. Which is what lets a bake build its grounds in a pipeline
//! and still place every camera where building them one after the other
//! does.

use std::collections::HashMap;
use std::sync::Arc;

use rayon::prelude::*;
use tuile_core::source::TileId;
use tuile_film::ground::{Ground, Prepare};
use tuile_film::stitching::Stitching;
use tuile_film::TileKey;

use crate::ground::Kept;

/// A frame's selection, frozen for its ground: each selected tile whose
/// terrain the session keeps, in the selection's order, with that terrain.
/// Taken by [`crate::Session::selected_ground`].
#[derive(Clone, Default)]
pub struct Selected {
    tiles: Vec<SelectedTile>,
}

#[derive(Clone)]
pub(crate) struct SelectedTile {
    pub tile: TileId,
    pub kept: Arc<Kept>,
    /// The level of the terrain the tile's surface is from, when the
    /// session's source says.
    pub source: Option<u32>,
}

impl Selected {
    pub(crate) fn new(tiles: Vec<SelectedTile>) -> Self {
        Self { tiles }
    }

    /// The tiles it holds.
    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }
}

/// What a drawer holds of a tile it prepared.
struct Entered {
    kept: Arc<Kept>,
    source: Option<u32>,
    /// The ground it was last drawn by.
    used: u64,
}

/// The bytes of tiles a drawer keeps beyond the last ground's, unless
/// `TUILE_GROUND_PARKED_MIB` says: 256 MiB.
const PARKED_MIB: usize = 256;

/// Builds the grounds of frames' selections, keeping what it prepared.
///
/// A tile is prepared (skirts, smoothing) once and kept while it is drawn;
/// one the selection lets go is kept a while too — a bake's selections
/// move back and forth over the same ground, and a tile that comes back is
/// then neither prepared nor drawn again — within a budget of bytes
/// (`TUILE_GROUND_PARKED_MIB`, default 256), the least recently drawn let
/// go first. A tile that comes back changed is prepared again.
pub struct GroundDrawer {
    how: Option<Prepare>,
    stitching: Stitching,
    entered: HashMap<TileId, Entered>,
    /// Grounds drawn.
    clock: u64,
    parked_budget: usize,
    /// The bytes of the ground last built.
    built: usize,
}

impl Default for GroundDrawer {
    fn default() -> Self {
        Self::new()
    }
}

impl GroundDrawer {
    pub fn new() -> Self {
        let mib = crate::session::count_knob("TUILE_GROUND_PARKED_MIB").unwrap_or(PARKED_MIB);
        Self::with_parked_budget(mib << 20)
    }

    /// A drawer keeping at most `bytes` of tiles beyond each ground's.
    pub fn with_parked_budget(bytes: usize) -> Self {
        Self {
            how: None,
            stitching: Stitching::default(),
            entered: HashMap::new(),
            clock: 0,
            parked_budget: bytes,
            built: 0,
        }
    }

    /// The ground `selected` draws, each tile prepared as `how` says, then
    /// stitched to its neighbours when `stitch`.
    pub fn ground(&mut self, selected: &Selected, how: Prepare, stitch: bool) -> Ground {
        let started = tuile_core::metrics::stamp();
        if self.how != Some(how) {
            self.stitching = Stitching::default();
            self.entered.clear();
            self.how = Some(how);
        }
        self.clock += 1;
        let clock = self.clock;
        let key = |tile: TileId| TileKey {
            id: tile.0,
            drape: 0,
        };
        // What has to be prepared: tiles the drawer does not hold, and tiles
        // that came back changed. A tile that came back unchanged — the
        // session let it go and loaded it again — is the tile it holds.
        let mut wanted = Vec::new();
        for t in &selected.tiles {
            match self.entered.get_mut(&t.tile) {
                Some(entered)
                    if entered.source == t.source
                        && (Arc::ptr_eq(&entered.kept, &t.kept)
                            || entered.kept.same_as(&t.kept)) =>
                {
                    entered.kept = Arc::clone(&t.kept);
                    entered.used = clock;
                }
                _ => wanted.push(t),
            }
        }
        let entering: Vec<_> = wanted
            .par_iter()
            .map(|t| {
                let mesh = tuile_film::stitching::encoded(
                    &t.kept.mesh(t.tile),
                    t.kept.origin().to_array(),
                    [1.0; 4],
                );
                let mut mesh = tuile_film::ground::prepared(mesh, how).0;
                // Heights read no normals, and nothing a vertex is stitched
                // by depends on them.
                mesh.normals = Vec::new();
                Stitching::entering(key(t.tile), t.source, &mesh)
            })
            .collect();
        for (t, entering) in wanted.iter().zip(entering) {
            self.stitching.admit(entering);
            self.entered.insert(
                t.tile,
                Entered {
                    kept: Arc::clone(&t.kept),
                    source: t.source,
                    used: clock,
                },
            );
        }
        let keys: Vec<TileKey> = selected.tiles.iter().map(|t| key(t.tile)).collect();
        if stitch {
            self.stitching.frame(&keys);
        }
        let ground = Ground::built_with(&self.stitching, &keys, stitch, |missing, build| {
            missing.par_iter().for_each(build);
        });
        self.built = ground.heap_bytes();
        self.park();
        tuile_core::metrics::metrics()
            .ground_seconds
            .record(started.elapsed());
        ground
    }

    /// Lets go of the tiles the last ground did not draw, least recently
    /// drawn first, until those left take no more than the budget.
    fn park(&mut self) {
        let mut parked: Vec<(u64, TileId, usize)> = self
            .entered
            .iter()
            .filter(|(_, e)| e.used != self.clock)
            .map(|(tile, e)| {
                let bytes = self.stitching.held_bytes(&TileKey {
                    id: tile.0,
                    drape: 0,
                });
                (e.used, *tile, bytes + e.kept.heap_bytes())
            })
            .collect();
        let mut held: usize = parked.iter().map(|p| p.2).sum();
        if held <= self.parked_budget {
            return;
        }
        // Oldest first; by tile among equals, so that the same grounds let
        // go of the same tiles.
        parked.sort_unstable_by_key(|(used, tile, _)| (*used, tile.0));
        for (_, tile, bytes) in parked {
            if held <= self.parked_budget {
                break;
            }
            self.entered.remove(&tile);
            self.stitching.leave(&TileKey {
                id: tile.0,
                drape: 0,
            });
            held -= bytes;
        }
    }

    /// The tiles it holds prepared, and their bytes ([`crate::GroundMemory`]).
    pub(crate) fn prepared(&self) -> (usize, usize) {
        (self.stitching.len(), self.stitching.heap_bytes())
    }

    /// The bytes of the ground last built.
    pub(crate) fn built(&self) -> usize {
        self.built
    }
}
