// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The host's 3D Tiles tilesets switched on and off, from the menu, a
//! steering URL or a script.
//!
//! Off, a tileset costs nothing: its layer is dropped — its pump and what
//! it held on the GPU, its stream, and with the stream its geometry server,
//! which ends when its client hangs up. The globe under it was never
//! unloaded (its tiles are selected and loaded under the tileset, which
//! only keeps them out of the pixels it draws), so the frame after is the
//! frame of a session without the tileset, with no instant of bare ground.
//! Its credits go with it, and the credits bar with them when it was the
//! last.
//!
//! On, it is opened again as at the start — its root asked fresh, a server
//! of its own — and drawn once its tiles arrive; until then the globe is
//! there, as it was.
//!
//! Each is off until the person turns it on, unless the session was started
//! with it (`--tileset`, the settings page, `TUILE_TILESETS`). The choice is
//! kept between sessions (`shell::keep_tilesets_on`), where the session
//! keeps anything (not a headless run).

use super::App;

/// One of the session's tilesets, as the menu lists it.
pub(super) struct Slot {
    pub asset: i64,
    /// What the menu calls it.
    pub name: String,
    /// Whether it is meant to be drawn.
    pub on: bool,
    /// Its server on its way up, after it was switched on.
    opening: Option<crate::surface::Opening>,
    /// Whether its last opening failed: it is not tried again until it is
    /// switched off and on.
    failed: bool,
}

/// The session's tilesets, in the order the host was asked for them.
#[derive(Default)]
pub(super) struct Tilesets {
    pub slots: Vec<Slot>,
    /// Whether the choice is kept for the next session.
    pub keeps: bool,
}

impl Tilesets {
    /// The tilesets `offered`, those among `on` switched on, and any of
    /// `on` not offered added after them.
    pub(super) fn new(offered: &[crate::embed::TilesetChoice], on: &[i64], keeps: bool) -> Self {
        Self {
            slots: offered_and(offered, on)
                .into_iter()
                .map(|choice| Slot {
                    on: on.contains(&choice.asset),
                    asset: choice.asset,
                    name: choice.name,
                    opening: None,
                    failed: false,
                })
                .collect(),
            keeps,
        }
    }

    /// Which are on, a bit each by their order.
    pub(super) fn bits(&self) -> u32 {
        self.slots
            .iter()
            .take(32)
            .enumerate()
            .filter(|(_, slot)| slot.on)
            .fold(0, |bits, (index, _)| bits | (1 << index))
    }

    /// The asset numbers switched on.
    pub(super) fn on(&self) -> Vec<i64> {
        self.slots
            .iter()
            .filter(|slot| slot.on)
            .map(|slot| slot.asset)
            .collect()
    }
}

/// `offered`, and after them each of `on` that is not among them, named by
/// its asset number.
pub(crate) fn offered_and(
    offered: &[crate::embed::TilesetChoice],
    on: &[i64],
) -> Vec<crate::embed::TilesetChoice> {
    let mut all = offered.to_vec();
    for &asset in on {
        if !all.iter().any(|c| c.asset == asset) {
            all.push(crate::embed::TilesetChoice::new(
                &format!("3D Tiles {asset}"),
                asset,
            ));
        }
    }
    all
}

impl App {
    /// Draws the tileset `asset` over the globe, or stops drawing it.
    pub(super) fn switch_tileset(&mut self, asset: i64, on: bool) {
        let Some(slot) = self
            .tilesets
            .slots
            .iter_mut()
            .find(|slot| slot.asset == asset)
        else {
            self.say(&format!("3D Tiles {asset} is not one of this session's"));
            return;
        };
        if slot.on == on {
            return;
        }
        slot.on = on;
        slot.failed = false;
        if on {
            tracing::info!(asset, "3D Tiles switched on");
            // Opened on the next turn of the loop, once there is a session.
        } else {
            tracing::info!(asset, "3D Tiles switched off");
            slot.opening = None;
            // Its pump, its GPU copies, its stream; its server ends with the
            // stream. The globe beneath was loaded all along.
            self.surfaces.retain(|layer| layer.surface.asset != asset);
        }
        if self.tilesets.keeps {
            crate::shell::keep_tilesets_on(&self.tilesets.on());
        }
        self.draw_soon();
    }

    /// Marks the tilesets the session's launch could not open as failed, so
    /// that they are not opened again behind the message that said so.
    pub(super) fn note_the_tilesets_up(&mut self, failures: &[(i64, String)]) {
        for slot in &mut self.tilesets.slots {
            if failures.iter().any(|(asset, _)| *asset == slot.asset) {
                slot.failed = true;
            }
        }
    }

    /// Takes the tilesets that have come up, and opens the ones switched on
    /// that are neither up nor on their way.
    pub(super) fn take_the_tilesets(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let origin = self.controller.camera.position;
        let mut failures = Vec::new();
        for slot in &mut self.tilesets.slots {
            if let Some(opening) = slot.opening.as_mut() {
                match opening.try_recv() {
                    Ok(Ok(surface)) => {
                        slot.opening = None;
                        if slot.on {
                            tracing::info!(asset = slot.asset, "3D Tiles tileset up again");
                            self.surfaces
                                .push(crate::surface::Layer::new(surface, origin));
                        }
                    }
                    Ok(Err(why)) => {
                        slot.opening = None;
                        slot.failed = true;
                        failures.push(format!("3D Tiles asset {}: {why}", slot.asset));
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                        slot.opening = None;
                        slot.failed = true;
                        failures.push(format!("3D Tiles asset {}: its opening ended", slot.asset));
                    }
                }
                continue;
            }
            let up = self
                .surfaces
                .iter()
                .any(|layer| layer.surface.asset == slot.asset);
            if slot.on && !slot.failed && !up {
                slot.opening = Some(session.open_tileset(slot.asset));
            }
        }
        if !failures.is_empty() {
            for why in &failures {
                tracing::error!("{why}");
            }
            self.messages.say(
                format!("{}: a 3D Tiles tileset is not shown", self.title),
                failures.join("\n"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::embed::TilesetChoice;

    /// What is offered is off unless it is among those on; one on that is
    /// not offered is listed after the offered ones, by its number.
    #[test]
    fn offered_tilesets_are_off_unless_asked_for() {
        let offered = [TilesetChoice::new("Mesh", 7), TilesetChoice::new("City", 8)];
        let none = Tilesets::new(&offered, &[], true);
        assert_eq!(none.bits(), 0);
        assert!(none.on().is_empty());
        let some = Tilesets::new(&offered, &[8, 9], true);
        assert_eq!(some.bits(), 0b110);
        assert_eq!(some.on(), vec![8, 9]);
        let names: Vec<&str> = some.slots.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Mesh", "City", "3D Tiles 9"]);
        assert_eq!(Tilesets::new(&[], &[], false).bits(), 0);
    }
}
