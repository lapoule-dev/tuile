// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! One correction per level.
//!
//! A level of an imagery pyramid is, over the ground a film crosses, one
//! source: one sensor, one season, one way of developing the picture. So the
//! correction is the level's, not the tile's — **every tile of a level is
//! given the same gain**. Two neighbours of one level then differ after the
//! correction by exactly what they differed by before it: nothing is added
//! at an edge, which a correction worked out tile by tile cannot promise.
//!
//! The gains come from the pyramid itself. Wherever a tile and one of its
//! ancestors are both in hand, the tile's tone and the tone of the ancestor
//! over the same ground give one observation: level `L` is so many stops
//! from level `L'`. All the observations together are solved for one gain a
//! level, with an anchor level held at zero — the level everything else is
//! brought to.

use std::collections::BTreeMap;

/// The tone of a rectangle of an image of tightly packed RGBA8, as a linear
/// colour, and the share of its texels that counted — texels too dark or
/// too bright to say anything of the ground are left out.
///
/// The mean is taken over the values as stored, and that mean is then made
/// linear. A pyramid is built by averaging stored values, so this is the
/// one measure a level and the level it was reduced from agree on exactly;
/// a mean of linear values reads every reduction as a darkening, a
/// twentieth of a stop a level over broken ground, and a chain of levels
/// then drifts by what no eye could find between any two of them.
///
/// `rect` is `(x0, y0, x1, y1)` in fractions of the image.
pub fn tone_of(
    rgba: &[u8],
    width: u32,
    height: u32,
    rect: (f32, f32, f32, f32),
) -> ([f32; 3], f32) {
    let px = |f: f32, n: u32| ((f.clamp(0.0, 1.0) * n as f32).round() as u32).min(n);
    let (x0, y0) = (px(rect.0, width), px(rect.1, height));
    let (x1, y1) = (
        px(rect.2, width).max(x0 + 1).min(width),
        px(rect.3, height).max(y0 + 1).min(height),
    );
    let (mut sum, mut counted, mut texels) = ([0.0f64; 3], 0u32, 0u32);
    for y in y0..y1 {
        for x in x0..x1 {
            texels += 1;
            let i = ((y * width + x) * 4) as usize;
            let Some(texel) = rgba.get(i..i + 3) else {
                continue;
            };
            let brightest = texel.iter().copied().max().unwrap_or(0);
            let darkest = texel.iter().copied().min().unwrap_or(0);
            if brightest <= 6 || darkest >= 250 {
                continue;
            }
            for c in 0..3 {
                sum[c] += f64::from(texel[c]);
            }
            counted += 1;
        }
    }
    if counted == 0 {
        return ([0.0; 3], 0.0);
    }
    (
        sum.map(|s| linear_of_stored((s / f64::from(counted)) as f32)),
        counted as f32 / texels.max(1) as f32,
    )
}

/// A stored value, 0 to 255 and not necessarily whole, as linear light.
fn linear_of_stored(value: f32) -> f32 {
    let v = (value / 255.0).clamp(0.0, 1.0);
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn stored_of_linear(value: f32) -> f32 {
    let v = value.clamp(0.0, 1.0);
    let stored = if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (stored * 255.0).round()
}

/// Multiplies an image of tightly packed RGBA8 by `multipliers`, in linear
/// light, leaving alpha alone: what a renderer's composition does to a
/// layer, for the one that composes without a GPU. A multiplier of exactly
/// one leaves its channel's bytes as they are.
pub fn apply_multipliers(rgba: &mut [u8], multipliers: [f32; 3]) {
    if multipliers == [1.0; 3] {
        return;
    }
    let mut curves = [[0u8; 256]; 3];
    for (curve, by) in curves.iter_mut().zip(multipliers) {
        for (stored, out) in curve.iter_mut().enumerate() {
            *out = if by == 1.0 {
                stored as u8
            } else {
                stored_of_linear(linear_of_stored(stored as f32) * by) as u8
            };
        }
    }
    for texel in rgba.chunks_exact_mut(4) {
        for c in 0..3 {
            texel[c] = curves[c][texel[c] as usize];
        }
    }
}

/// One tile seen against one of its ancestors: the gain, in stops, that
/// brings the tile's tone to the ancestor's over the same ground.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observation {
    pub level: u8,
    pub ancestor: u8,
    pub gain: [f32; 3],
    /// How far it is believed, 0 to 1.
    pub weight: f32,
}

/// What the solve is told.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelParams {
    /// The level held at zero. If no tile of it was observed, the nearest
    /// level that was.
    pub anchor: u8,
    /// Two levels less than this apart, on every channel, are one source
    /// resampled: they are given one gain, exactly.
    pub dead_zone_stops: f32,
    /// No level is moved by more than this.
    pub clamp_stops: f32,
    /// How much of the saturation a grade's fit finds is kept, as a power:
    /// the fit matches colour in linear light, where vivid colours weigh
    /// more than the eye gives them, and overshoots. Three quarters is
    /// what brought the most vivid twentieth of a graded source to its
    /// reference's, measured in CIELAB on rendered frames.
    pub saturation_share: f32,
}

impl Default for LevelParams {
    fn default() -> Self {
        Self {
            anchor: 10,
            dead_zone_stops: 0.08,
            clamp_stops: 2.0,
            saturation_share: 0.75,
        }
    }
}

/// A gain per level, in stops, to bring every level to the anchor's tone.
#[derive(Debug, Clone, PartialEq)]
pub struct LevelGains {
    pub anchor: u8,
    pub gains: BTreeMap<u8, [f32; 3]>,
    /// The source each level is of: levels that are one source resampled
    /// name the same level, the coarsest of them.
    pub sources: BTreeMap<u8, u8>,
    /// For each pair of levels observed: how many observations, their
    /// agreed gain, and what the solve leaves of it. What a reader looks at
    /// to know whether "one gain a level" holds on this ground.
    pub pairs: Vec<PairReport>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PairReport {
    pub level: u8,
    pub ancestor: u8,
    pub observations: usize,
    /// The observations' weighted median, per channel.
    pub gain: [f32; 3],
    /// How far the observations spread around it: the median distance,
    /// worst channel. Large, and the level is not one source here.
    pub spread: f32,
    /// What is left between the two levels once both are corrected.
    pub residual: [f32; 3],
}

fn weighted_median(values: &mut [(f32, f32)]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.0.total_cmp(&b.0));
    let half = values.iter().map(|v| v.1).sum::<f32>() / 2.0;
    let mut run = 0.0;
    for (value, weight) in values.iter() {
        run += weight;
        if run >= half {
            return *value;
        }
    }
    values[values.len() - 1].0
}

impl LevelGains {
    /// The gain of a level, or nothing for a level that was never observed.
    pub fn of(&self, level: u8) -> [f32; 3] {
        self.gains.get(&level).copied().unwrap_or([0.0; 3])
    }

    /// What a level's linear colour is multiplied by, the correction taken
    /// at `strength` — 0 for none of it, 1 for all. A level that is not to
    /// move is `[1.0; 3]`, exactly.
    pub fn multipliers(&self, level: u8, strength: f32) -> [f32; 3] {
        self.of(level).map(|stops| {
            if stops == 0.0 || strength == 0.0 {
                1.0
            } else {
                2f32.powf(stops * strength)
            }
        })
    }

    /// Solves the observations for one gain a level.
    pub fn solve(observations: &[Observation], params: &LevelParams) -> Self {
        // One agreed gain per pair of levels: the weighted median of its
        // observations, which a few tiles of changed ground do not move.
        let seen: Vec<&Observation> = observations
            .iter()
            .filter(|o| o.weight > 0.0 && o.level != o.ancestor)
            .collect();
        let agreed = |of: &[&Observation]| {
            let mut gain = [0.0f32; 3];
            let mut spread = 0.0f32;
            for c in 0..3 {
                let mut values: Vec<(f32, f32)> =
                    of.iter().map(|o| (o.gain[c], o.weight)).collect();
                gain[c] = weighted_median(&mut values);
                let mut off: Vec<(f32, f32)> = of
                    .iter()
                    .map(|o| ((o.gain[c] - gain[c]).abs(), o.weight))
                    .collect();
                spread = spread.max(weighted_median(&mut off));
            }
            (gain, spread)
        };
        let mut by_pair: BTreeMap<(u8, u8), Vec<&Observation>> = BTreeMap::new();
        for o in &seen {
            by_pair.entry((o.level, o.ancestor)).or_default().push(o);
        }
        struct Edge {
            level: u8,
            ancestor: u8,
            gain: [f32; 3],
            observations: usize,
            spread: f32,
        }
        let edges: Vec<Edge> = by_pair
            .iter()
            .map(|((level, ancestor), of)| {
                let (gain, spread) = agreed(of);
                Edge {
                    level: *level,
                    ancestor: *ancestor,
                    gain,
                    observations: of.len(),
                    spread,
                }
            })
            .collect();
        let mut levels: Vec<u8> = edges.iter().flat_map(|e| [e.level, e.ancestor]).collect();
        levels.sort_unstable();
        levels.dedup();
        let anchor = levels
            .iter()
            .copied()
            .min_by_key(|l| l.abs_diff(params.anchor))
            .unwrap_or(params.anchor);

        // Levels that are one source resampled are one source: a pair whose
        // agreed gain is nothing joins its two levels, and joined levels are
        // given the same gain, exactly. Nothing drifts along a chain of them,
        // and what little two of them disagree by is not shared out among
        // the others.
        let mut source: BTreeMap<u8, u8> = levels.iter().map(|l| (*l, *l)).collect();
        fn root(source: &BTreeMap<u8, u8>, mut level: u8) -> u8 {
            while source[&level] != level {
                level = source[&level];
            }
            level
        }
        for e in &edges {
            if e.gain.iter().all(|g| g.abs() < params.dead_zone_stops) {
                let (a, b) = (root(&source, e.level), root(&source, e.ancestor));
                source.insert(a.max(b), a.min(b));
            }
        }
        let source: BTreeMap<u8, u8> = levels.iter().map(|l| (*l, root(&source, *l))).collect();

        // Between two sources, every observation from the one to the other
        // speaks, whichever levels it was made at.
        let mut between: BTreeMap<(u8, u8), Vec<&Observation>> = BTreeMap::new();
        for o in &seen {
            let (a, b) = (source[&o.level], source[&o.ancestor]);
            if a != b {
                between.entry((a, b)).or_default().push(o);
            }
        }
        let steps: Vec<(u8, u8, [f32; 3], f32)> = between
            .iter()
            .map(|((a, b), of)| (*a, *b, agreed(of).0, of.iter().map(|o| o.weight).sum()))
            .collect();

        // gain(source) − gain(the other) = step, in the least-squares sense,
        // the anchor's source held at zero. A handful of unknowns: relaxed
        // in place until it stops moving.
        let held = source[&anchor];
        let mut of_source: BTreeMap<u8, [f32; 3]> =
            source.values().map(|s| (*s, [0.0; 3])).collect();
        let sources: Vec<u8> = of_source.keys().copied().collect();
        for _ in 0..400 {
            for this in &sources {
                if *this == held {
                    continue;
                }
                let (mut sum, mut weight) = ([0.0f32; 3], 0.0f32);
                for (a, b, gain, w) in &steps {
                    let (other, sign) = if a == this {
                        (b, 1.0)
                    } else if b == this {
                        (a, -1.0)
                    } else {
                        continue;
                    };
                    let of_other = of_source[other];
                    for c in 0..3 {
                        sum[c] += w * (of_other[c] + sign * gain[c]);
                    }
                    weight += w;
                }
                if weight > 0.0 {
                    of_source.insert(*this, sum.map(|s| s / weight));
                }
            }
        }
        let mut gains: BTreeMap<u8, [f32; 3]> =
            levels.iter().map(|l| (*l, of_source[&source[l]])).collect();
        // Bounded, and kept to a 1024th of a stop: the same numbers wherever
        // this ran, and a level that is not to move does not move at all.
        for gain in gains.values_mut() {
            *gain = gain.map(|g| {
                (g.clamp(-params.clamp_stops, params.clamp_stops) * 1024.0).round() / 1024.0
            });
        }
        let pairs = edges
            .iter()
            .map(|e| {
                let (of_level, of_ancestor) = (gains[&e.level], gains[&e.ancestor]);
                let mut residual = [0.0f32; 3];
                for c in 0..3 {
                    residual[c] = e.gain[c] - (of_level[c] - of_ancestor[c]);
                }
                PairReport {
                    level: e.level,
                    ancestor: e.ancestor,
                    observations: e.observations,
                    gain: e.gain,
                    spread: e.spread,
                    residual,
                }
            })
            .collect();
        Self {
            anchor,
            gains,
            sources: source,
            pairs,
        }
    }

    /// The gains as a reader of the store finds them: a small JSON object.
    pub fn to_json(&self) -> String {
        let levels: Vec<String> = self
            .gains
            .iter()
            .map(|(level, g)| format!("\"{level}\":[{:.4},{:.4},{:.4}]", g[0], g[1], g[2]))
            .collect();
        format!(
            "{{\"version\":1,\"anchor\":{},\"unit\":\"stops\",\"levels\":{{{}}}}}",
            self.anchor,
            levels.join(",")
        )
    }

    /// Reads [`Self::to_json`] back. `None` if this is not one.
    pub fn from_json(text: &str) -> Option<Self> {
        let anchor = text
            .split("\"anchor\":")
            .nth(1)?
            .split([',', '}'])
            .next()?
            .trim()
            .parse()
            .ok()?;
        let body = text.split("\"levels\":{").nth(1)?;
        let mut gains = BTreeMap::new();
        for entry in body.split(']') {
            let Some((level, values)) = entry.split_once(":[") else {
                continue;
            };
            let level: u8 = level
                .trim_matches(|c: char| !c.is_ascii_digit())
                .parse()
                .ok()?;
            let v: Vec<f32> = values
                .split(',')
                .filter_map(|n| n.trim().parse().ok())
                .collect();
            if v.len() != 3 {
                return None;
            }
            gains.insert(level, [v[0], v[1], v[2]]);
        }
        Some(Self {
            anchor,
            gains,
            sources: BTreeMap::new(),
            pairs: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::linear_of;

    fn seen(level: u8, ancestor: u8, gain: f32) -> Observation {
        Observation {
            level,
            ancestor,
            gain: [gain, gain * 0.5, -gain],
            weight: 1.0,
        }
    }

    #[test]
    fn levels_chain_to_the_anchor_and_those_between_changes_do_not_drift() {
        // 10 is the anchor. 11 and 12 are 10 resampled, give or take noise;
        // 13 is another source, a stop from 12; 14 is 13 resampled.
        let mut o = Vec::new();
        for noise in [-0.03, 0.01, 0.04, 0.02, -0.01] {
            o.push(seen(11, 10, noise));
            o.push(seen(12, 11, noise));
            o.push(seen(13, 12, 1.0 + noise));
            o.push(seen(14, 13, noise));
        }
        let solved = LevelGains::solve(&o, &LevelParams::default());
        assert_eq!(solved.anchor, 10);
        assert_eq!(solved.of(10), [0.0; 3]);
        // Nothing, exactly, where nothing changed.
        assert_eq!(solved.of(11), [0.0; 3]);
        assert_eq!(solved.of(12), [0.0; 3]);
        // The stop, and only the stop, from 13 on.
        let (at_13, at_14) = (solved.of(13), solved.of(14));
        assert!(
            (at_13[0] - 1.01).abs() < 0.02 && (at_13[2] + 1.01).abs() < 0.02,
            "{at_13:?}"
        );
        assert_eq!(at_13, at_14);
        // What is left is the noise there was, and no more.
        assert!(solved
            .pairs
            .iter()
            .all(|p| p.residual.iter().all(|r| r.abs() < 0.05)));
    }

    #[test]
    fn what_two_levels_of_one_source_disagree_by_is_not_shared_out() {
        // 11 and 12 are one source. 13 is seen a stop from 12 and a stop
        // and a fifth from 11: a disagreement about 13, not about 11 or 12.
        let mut o = Vec::new();
        for _ in 0..10 {
            o.push(seen(11, 10, 0.0));
            o.push(seen(12, 11, 0.0));
            o.push(seen(13, 12, 1.0));
            o.push(seen(13, 11, 1.2));
        }
        let solved = LevelGains::solve(&o, &LevelParams::default());
        assert_eq!(solved.of(11), [0.0; 3]);
        assert_eq!(solved.of(12), [0.0; 3]);
        assert!(
            (1.0..=1.2).contains(&solved.of(13)[0]),
            "{:?}",
            solved.of(13)
        );
    }

    #[test]
    fn a_few_tiles_of_changed_ground_do_not_move_a_level() {
        let mut o: Vec<Observation> = (0..20).map(|_| seen(13, 12, 0.5)).collect();
        // Snow, cloud, a lake: wildly off, and few.
        o.extend((0..5).map(|_| seen(13, 12, -3.0)));
        let solved = LevelGains::solve(
            &o,
            &LevelParams {
                anchor: 12,
                ..Default::default()
            },
        );
        assert!((solved.of(13)[0] - 0.5).abs() < 1e-3, "{:?}", solved.of(13));
        assert!(solved.pairs[0].spread < 0.01);
    }

    #[test]
    fn levels_seen_only_against_far_ancestors_are_still_solved() {
        // No tile of 13 has its parent in hand; it is seen against 10, and
        // 15 against 13.
        let o = [
            seen(13, 10, 0.8),
            seen(15, 13, -0.3),
            seen(8, 4, 0.2),
            seen(10, 8, 0.4),
        ];
        let solved = LevelGains::solve(&o, &LevelParams::default());
        assert!((solved.of(13)[0] - 0.8).abs() < 1e-3);
        assert!((solved.of(15)[0] - 0.5).abs() < 1e-3);
        // Coarser than the anchor: brought to it from the other side.
        assert!((solved.of(8)[0] + 0.4).abs() < 1e-3);
        assert!((solved.of(4)[0] + 0.6).abs() < 1e-3);
    }

    #[test]
    fn an_anchor_nobody_saw_is_the_nearest_level_seen() {
        let solved = LevelGains::solve(&[seen(15, 13, 0.5)], &LevelParams::default());
        assert_eq!(solved.anchor, 13);
        assert_eq!(solved.of(13), [0.0; 3]);
    }

    #[test]
    fn multipliers_are_applied_in_linear_light_and_one_is_nothing() {
        let solved = LevelGains::solve(&[seen(13, 10, 1.0)], &LevelParams::default());
        // +1, +0.5, −1 stop at level 13; nothing at the anchor.
        assert_eq!(solved.multipliers(10, 1.0), [1.0; 3]);
        assert_eq!(solved.multipliers(13, 0.0), [1.0; 3]);
        let by = solved.multipliers(13, 1.0);
        assert!((by[0] - 2.0).abs() < 1e-3 && (by[2] - 0.5).abs() < 1e-3);

        let mut texel = [100u8, 100, 100, 77];
        apply_multipliers(&mut texel, [2.0, 1.0, 0.5]);
        // Twice the light of 100 is 138 as stored; half of it, 71.
        assert_eq!(texel, [138, 100, 71, 77]);
        let mut bright = [240u8, 240, 240, 255];
        apply_multipliers(&mut bright, [2.0, 2.0, 2.0]);
        assert_eq!(bright, [255, 255, 255, 255]);
    }

    #[test]
    fn gains_are_their_json() {
        let solved = LevelGains::solve(
            &[seen(13, 10, 0.8), seen(15, 13, -0.3)],
            &LevelParams::default(),
        );
        let read = LevelGains::from_json(&solved.to_json()).expect("read back");
        assert_eq!(read.anchor, solved.anchor);
        for (level, gain) in &solved.gains {
            for c in 0..3 {
                assert!((read.of(*level)[c] - gain[c]).abs() < 1e-3);
            }
        }
        assert_eq!(LevelGains::from_json("{}"), None);
    }

    #[test]
    fn a_level_and_its_reduction_are_one_tone() {
        // A checkerboard of 40 and 200, and the level above it: each texel
        // the mean of four stored values, as a pyramid is built.
        let fine: Vec<u8> = (0..16 * 16)
            .flat_map(|i| {
                let v = if (i % 16 + i / 16) % 2 == 0 { 40 } else { 200 };
                [v, v, v, 255]
            })
            .collect();
        let coarse: Vec<u8> = (0..8 * 8).flat_map(|_| [120, 120, 120, 255]).collect();
        let (of_fine, _) = tone_of(&fine, 16, 16, (0.0, 0.0, 1.0, 1.0));
        let (of_coarse, _) = tone_of(&coarse, 8, 8, (0.0, 0.0, 1.0, 1.0));
        assert!((of_fine[0] / of_coarse[0]).log2().abs() < 1e-4);
    }

    #[test]
    fn the_tone_of_a_rectangle_leaves_out_what_says_nothing() {
        // Left half mid grey, right half black: the black is not ground.
        let mut rgba = Vec::new();
        for _ in 0..4 {
            for x in 0..4 {
                let v = if x < 2 { 128 } else { 0 };
                rgba.extend([v, v, v, 255]);
            }
        }
        let (tone, counted) = tone_of(&rgba, 4, 4, (0.0, 0.0, 1.0, 1.0));
        assert!((tone[0] - linear_of(128)).abs() < 1e-5);
        assert!((counted - 0.5).abs() < 1e-6);
        let (_, none) = tone_of(&rgba, 4, 4, (0.5, 0.0, 1.0, 1.0));
        assert_eq!(none, 0.0);
    }
}
