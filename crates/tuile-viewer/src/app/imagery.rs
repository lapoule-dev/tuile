// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Changing the imagery under a running session.
//!
//! The window's part of a switch is small, and all of it is here: ask, say
//! so, take the answer when it comes, and watch the new picture cross the
//! screen. Everything that keeps the ground covered meanwhile is the engine's
//! — `tuile_planetary::SwitchableImagery` and the geometry server's refresh —
//! and the window could not uncover a tile if it tried: it never drops one.
//!
//! One switch at a time. A second one asked for while the first is still
//! resolving waits its turn and replaces any other that was waiting: only the
//! last layer a person asked for matters, and two resolutions racing each
//! other would settle on whichever happened to finish last.

use std::time::Instant;

use super::App;
use crate::embed::ImageryChoice;

/// Which layer is draped, and a change on its way.
pub(super) struct Imagery {
    /// The host's layers, in its order.
    pub(super) layers: &'static [ImageryChoice],
    /// The layer being served: an index into `layers`.
    pub(super) layer: usize,
    /// The imagery generation being served, as tiles are stamped with it:
    /// what a drawn tile's `imagery_source` is compared against.
    pub(super) generation: u32,
    /// The layer asked for and still resolving.
    asking: Option<usize>,
    /// The layer asked for while another was resolving.
    waiting: Option<usize>,
    /// A switch that has been made and has not finished crossing the screen:
    /// when it was asked for, and the most imagery the GPU held on the way.
    turning: Option<Turning>,
}

struct Turning {
    asked: Instant,
    /// Imagery bytes on the GPU when the switch was made, and the most seen
    /// since: for a while the old picture and the new are both there.
    before: usize,
    peak: usize,
}

impl Imagery {
    pub(super) fn new(layers: &'static [ImageryChoice], layer: usize) -> Self {
        Self {
            layers,
            layer,
            generation: 0,
            asking: None,
            waiting: None,
            turning: None,
        }
    }

    /// The layer after the one a person last asked for, round the list;
    /// `None` when there is nothing to go round.
    pub(super) fn next(&self) -> Option<usize> {
        let from = self.waiting.or(self.asking).unwrap_or(self.layer);
        (self.layers.len() > 1).then(|| (from + 1) % self.layers.len())
    }

    /// Notes that `layer` was asked for. Returns whether to start resolving
    /// it now — `false` when it is already the layer, or when another is
    /// resolving and this one has taken the place in the queue.
    pub(super) fn ask(&mut self, layer: usize) -> bool {
        if layer >= self.layers.len() {
            return false;
        }
        if self.asking.is_some() {
            self.waiting = Some(layer);
            return false;
        }
        if layer == self.layer {
            return false;
        }
        self.asking = Some(layer);
        true
    }

    /// Notes how the resolution that was running ended. Returns the layer to
    /// resolve next, if one was waiting and is still a change.
    pub(super) fn answered(&mut self, layer: usize, generation: Option<u64>) -> Option<usize> {
        self.asking = None;
        if let Some(generation) = generation {
            self.layer = layer;
            // Truncated as the engine truncates it: see `ImageryLayer::source`.
            self.generation = generation as u32;
        }
        let next = self.waiting.take()?;
        self.ask(next).then_some(next)
    }
}

impl App {
    /// Drapes the host's layer `layer`, if it is not already draped.
    pub(super) fn drape(&mut self, layer: usize) {
        let Some(choice) = self.imagery.layers.get(layer) else {
            return;
        };
        if !self.imagery.ask(layer) {
            return;
        }
        self.switcher.ask(layer, choice);
        self.say(&format!("imagery: {}…", choice.name));
    }

    /// Takes the answer to a switch that was asked for, on whatever turn of
    /// the loop it arrives.
    pub(super) fn take_the_imagery(&mut self) {
        while let Some(switched) = self.switcher.poll() {
            let name = self
                .imagery
                .layers
                .get(switched.layer)
                .map_or("?", |l| l.name.as_str());
            let next = match switched.outcome {
                Ok(generation) => {
                    let before = self.imagery_bytes();
                    self.imagery.turning = Some(Turning {
                        asked: switched.asked,
                        before,
                        peak: before,
                    });
                    self.say(&format!("imagery: {name}"));
                    self.imagery.answered(switched.layer, Some(generation))
                }
                // Nothing changed, and nothing was taken off the screen to
                // find that out.
                Err(why) => {
                    tracing::error!("imagery {name:?} is not available: {why}");
                    self.say(&format!("imagery {name} is not available"));
                    self.imagery.answered(switched.layer, None)
                }
            };
            if let Some(next) = next {
                self.switcher.ask(next, &self.imagery.layers[next]);
            }
        }
    }

    /// Imagery bytes on the GPU right now: distinct textures, counted once.
    fn imagery_bytes(&self) -> usize {
        self.active.as_ref().map_or(0, |active| {
            active
                .gpu
                .imagery
                .lock()
                .map_or(0, |textures| textures.live().1)
        })
    }

    /// Watches a switch cross the screen, and says when it has.
    ///
    /// `old` is how many of the tiles just drawn still wear another layer's
    /// imagery. A switch is over on the first frame that draws none — which is
    /// the number worth reporting, since it is what a person waits for: not
    /// the request, which is a few milliseconds, but the last tile turning.
    pub(super) fn note_the_turn_over(&mut self, old: usize) {
        if self.imagery.turning.is_none() {
            return;
        }
        let now = self.imagery_bytes();
        let Some(turning) = self.imagery.turning.as_mut() else {
            return;
        };
        turning.peak = turning.peak.max(now);
        if old > 0 {
            return;
        }
        const MIB: f64 = 1024.0 * 1024.0;
        // In the journal, which a session started from an icon still writes:
        // this is the one number about a switch worth keeping.
        crate::journal::note(&format!(
            "imagery turned over to {:?} in {:.2} s: {} tiles on screen, imagery on the GPU \
             {:.0} MiB before, {:.0} MiB at most, {:.0} MiB after",
            self.imagery.layers[self.imagery.layer].key,
            turning.asked.elapsed().as_secs_f64(),
            self.drawn.0,
            turning.before as f64 / MIB,
            turning.peak as f64 / MIB,
            now as f64 / MIB,
        ));
        self.imagery.turning = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layers() -> &'static [ImageryChoice] {
        Box::leak(Box::new([
            ImageryChoice::new("a", "A", 1, ""),
            ImageryChoice::new("b", "B", 2, ""),
            ImageryChoice::new("c", "C", 3, ""),
        ]))
    }

    /// One resolution at a time; what is asked meanwhile waits, and only the
    /// last of those is kept.
    #[test]
    fn a_switch_asked_during_another_waits_and_the_last_one_wins() {
        let mut imagery = Imagery::new(layers(), 0);
        assert!(!imagery.ask(0), "already draped");
        assert!(imagery.ask(1));
        assert!(!imagery.ask(2), "one is resolving");
        assert!(!imagery.ask(0), "and this replaces the one waiting");
        // The first lands: the layer and its generation are now served, and
        // the one that waited — layer 0 — is a change again.
        assert_eq!(imagery.answered(1, Some(1)), Some(0));
        assert_eq!((imagery.layer, imagery.generation), (1, 1));
        assert_eq!(imagery.answered(0, Some(2)), None);
        assert_eq!((imagery.layer, imagery.generation), (0, 2));
    }

    /// A layer that could not be resolved changes nothing.
    #[test]
    fn a_switch_that_fails_leaves_the_layer_as_it_was() {
        let mut imagery = Imagery::new(layers(), 2);
        assert!(imagery.ask(0));
        assert_eq!(imagery.answered(0, None), None);
        assert_eq!((imagery.layer, imagery.generation), (2, 0));
        assert!(imagery.ask(0), "and it can be asked for again");
    }

    /// The key goes round the list from the layer last asked for, so two
    /// presses in quick succession are two steps and not the same step twice.
    #[test]
    fn next_goes_round_from_the_layer_last_asked_for() {
        let mut imagery = Imagery::new(layers(), 2);
        assert_eq!(imagery.next(), Some(0));
        assert!(imagery.ask(0));
        assert_eq!(imagery.next(), Some(1));
        assert!(!imagery.ask(1));
        assert_eq!(imagery.next(), Some(2));
        assert_eq!(Imagery::new(&layers()[..1], 0).next(), None);
        assert!(!Imagery::new(layers(), 0).ask(9), "not a layer");
    }
}
