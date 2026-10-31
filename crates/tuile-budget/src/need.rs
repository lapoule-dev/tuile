// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! [`NeedSet`]: what the traversal wants this step, the budget's only input
//! about the view.

use super::imagery::{GeoRect, TexelFootprint};
use super::types::{Class, LayerId, TileId};

/// What the traversal wants, built afresh each decision step.
///
/// A tile may appear in several lists; the most urgent membership wins. The
/// order of the entries does not matter: the budget's decisions are stable
/// under any permutation of them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NeedSet {
    /// The tiles the traversal wants drawn this step.
    pub drawn: Vec<(TileId, Need)>,
    /// The children a held REPLACE parent waits on before it can refine:
    /// all of them, resident or not, so that the resident ones are kept
    /// while their siblings load (a swap group).
    pub blockers: Vec<(TileId, Need)>,
    /// Ancestors preloaded so that a zoom-out finds them resident.
    pub ancestors: Vec<(TileId, Need)>,
    /// A coarse prefetch ring around the frustum (rotation).
    pub ring: Vec<(TileId, Need)>,
}

impl NeedSet {
    /// An empty need.
    pub fn new() -> Self {
        NeedSet::default()
    }

    /// Every entry with its membership, most urgent list first.
    pub fn entries(&self) -> impl Iterator<Item = (Membership, TileId, &Need)> + '_ {
        let lists: [(Membership, &'_ [(TileId, Need)]); 4] = [
            (Membership::Drawn, &self.drawn),
            (Membership::Blocker, &self.blockers),
            (Membership::Ancestor, &self.ancestors),
            (Membership::Ring, &self.ring),
        ];
        lists
            .into_iter()
            .flat_map(|(m, l)| l.iter().map(move |(t, n)| (m, *t, n)))
    }
}

/// Which list of the [`NeedSet`] an entry came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Membership {
    /// Wanted on screen.
    Drawn,
    /// Waited on by a held parent.
    Blocker,
    /// A preloaded ancestor.
    Ancestor,
    /// The prefetch ring.
    Ring,
}

/// One tile's need, from the traversal.
///
/// Floats are quantised once when the budget reads them; a non-finite value
/// counts as zero and is reported in the step's statistics.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Need {
    /// The tile's level in its tree.
    pub level: u32,
    /// The tile's parent, which is how the budget knows the fallback chain.
    pub parent: Option<TileId>,
    /// The mesh's class: [`Class::TerrainMesh`] or [`Class::Tiles3dMesh`].
    pub class: Class,
    /// `log2(sse / τ_eff)`: how much the tile is wanted beyond its threshold.
    pub sse_excess: f32,
    /// `1 − off-axis`, in `[0, 1]`: 1 at the centre of the view.
    pub centrality: f32,
    /// Metres from the camera to the tile.
    pub distance: f32,
    /// The imagery the tile wants, chosen independently of `level`. `None`
    /// for a mesh that carries its own textures.
    pub imagery: Option<ImageryNeed>,
    /// The tile's footprint in longitude/latitude, for drape UV transforms.
    pub rect: Option<GeoRect>,
}

impl Need {
    /// A need for a tile at `level`, of mesh class `class`, with neutral
    /// scores.
    pub fn new(level: u32, class: Class) -> Self {
        Need {
            level,
            parent: None,
            class,
            sse_excess: 0.0,
            centrality: 1.0,
            distance: 0.0,
            imagery: None,
            rect: None,
        }
    }

    /// Sets the parent.
    pub fn with_parent(mut self, parent: TileId) -> Self {
        self.parent = Some(parent);
        self
    }
    /// Sets the SSE excess.
    pub fn with_sse_excess(mut self, v: f32) -> Self {
        self.sse_excess = v;
        self
    }
    /// Sets the centrality.
    pub fn with_centrality(mut self, v: f32) -> Self {
        self.centrality = v;
        self
    }
    /// Sets the distance.
    pub fn with_distance(mut self, v: f32) -> Self {
        self.distance = v;
        self
    }
    /// Sets the imagery need.
    pub fn with_imagery(mut self, v: ImageryNeed) -> Self {
        self.imagery = Some(v);
        self
    }
    /// Sets the footprint.
    pub fn with_rect(mut self, v: GeoRect) -> Self {
        self.rect = Some(v);
        self
    }
}

/// How deep one imagery layer reaches over a tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LayerReach {
    /// The layer.
    pub layer: LayerId,
    /// Its source's deepest level over this tile.
    pub max_level: u32,
}

/// The imagery a tile wants (D-A4): one texel footprint on screen,
/// geometric and common to every layer, and how deep each layer it wears
/// reaches. Each layer picks its level, falls back and refreshes on its
/// own.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageryNeed {
    /// How large one level-0 texel looks on screen, at the tile's closest
    /// visible point.
    pub footprint: TexelFootprint,
    /// The layers the tile wears, in stacking order.
    pub layers: Vec<LayerReach>,
}

impl ImageryNeed {
    /// One layer, the base imagery (`LayerId(0)`), reaching `max_level`.
    pub fn base(footprint: TexelFootprint, max_level: u32) -> Self {
        ImageryNeed {
            footprint,
            layers: vec![LayerReach {
                layer: LayerId(0),
                max_level,
            }],
        }
    }
}
