// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A closed loop around the Pyrenees, flown straight down.
//!
//! The chain runs some 430 km from the Atlantic to the Mediterranean, and it is
//! not a parallel: the western end sits near 43.3° N and the eastern end near
//! 42.45°. A rectangle in latitude and longitude would therefore cross the
//! crest twice and miss both ends. The path below is the crest itself, offset
//! north and then south, so the loop hugs the range instead of boxing it.
//!
//! **Nadir, and that is what makes it different from `border-tape`.** That one
//! flies pitched 50° below horizontal and uses the local vertical as screen-up,
//! which works precisely because a direction tipped 50° off the vertical is
//! never parallel to it. Straight down, it IS parallel, and the frame is
//! undefined. So here the bearing carries screen-up: the ground reads like a
//! map with the direction of travel toward the top of the picture.
//!
//! ```text
//! cargo run -p tuile-tape --bin pyrenees-tape -- pyrenees.mcap [minutes] [fps] [altitude_m] [offset_deg] [tilt_deg]
//! ```

use tuile_tape::path::{geodetic_to_ecef, ClosedSpline};
use tuile_tape::{Frame, Tape};

const MINUTES: f64 = 2.0;
const FPS: f64 = 24.0;

/// Metres above the ellipsoid.
///
/// 90 km, et non 50. À 50 km en visée verticale le cadre couvrait 41 km de
/// piémont : l'image se lisait comme une carte satellite, sans relief et sans
/// mouvement. Vérifié en la regardant.
const ALTITUDE: f64 = 90_000.0;

/// Inclinaison de la visée par rapport à la verticale, en degrés.
///
/// Zéro serait le nadir. Vingt degrés, c'est « légèrement penché dans le sens
/// de la marche » : à 90 km le cadre porte alors d'environ 4 km derrière la
/// caméra à 82 km devant elle, ce qui donne du relief, de la profondeur, et le
/// sens du vol — au lieu d'un plan.
///
/// Et ça rend la verticale locale utilisable comme haut d'écran : à 20° elle
/// n'est plus parallèle à la visée (elle l'est exactement au nadir), donc elle
/// tient l'horizon droit. Voir `path::Step::look`.
const TILT: f64 = 20.0;

/// How far off the crest each flank is flown, in degrees of latitude.
///
/// 0.40° is about 44 km. At 50 km up with a 45° vertical field the frame covers
/// some 41 km, so the crest sits at the edge of the picture and the piedmont
/// fills it — which is what flying *around* the massif means rather than along
/// it.
const OFFSET: f64 = 0.40;

/// The crest, west to east. Coarse on purpose: great-circle interpolation
/// between a handful of points follows the range closely enough for a camera
/// 50 km up, and a denser line would buy nothing a tile is large enough to see.
const CREST: [(f64, f64); 6] = [
    (-1.60, 43.30), // Hendaye, the Atlantic end
    (-0.75, 42.95), // Somport
    (0.15, 42.75),  // Gavarnie
    (1.00, 42.65),  // Andorra
    (1.95, 42.50),  // the Cerdanya
    (3.05, 42.45),  // Cap Cerbère, the Mediterranean end
];

/// The loop: the crest offset north, west to east, round the eastern cap, back
/// along the southern flank, round the western cap. Closed by `ClosedSpline`.
///
/// **Les caps portent leurs propres points, et c'est le sujet.** Sans eux la
/// boucle passe d'un flanc à l'autre par un segment droit de `2 × offset` en
/// latitude : un virage à angle droit. Une spline l'arrondit, mais elle
/// concentre alors tout le changement de cap sur la longueur d'un seul
/// segment — un demi-tour au lieu d'un arc. En posant un point au-delà de
/// l'extrémité, à la latitude de la crête, le demi-tour devient une courbe qui
/// a la place de tourner.
///
/// La saillie vaut `offset`, donc le cap est aussi large que la boucle est
/// haute : c'est ce qui rend l'arc à peu près circulaire plutôt qu'écrasé.
fn loop_points(offset: f64) -> Vec<(f64, f64)> {
    let first = CREST[0];
    let last = CREST[CREST.len() - 1];
    let mut points: Vec<(f64, f64)> = CREST
        .iter()
        .map(|&(lon, lat)| (lon, lat + offset))
        .collect();
    // Cap est, au large de Cerbère.
    points.push((last.0 + offset, last.1));
    points.extend(CREST.iter().rev().map(|&(lon, lat)| (lon, lat - offset)));
    // Cap ouest, au large d'Hendaye.
    points.push((first.0 - offset, first.1));
    points
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| "pyrenees.mcap".to_owned());
    let mut next = |fallback: f64| -> Result<f64, String> {
        args.next().map_or(Ok(fallback), |a| {
            a.parse().map_err(|_| format!("nombre attendu, reçu {a:?}"))
        })
    };
    let minutes = next(MINUTES)?;
    let fps = next(FPS)?;
    let altitude = next(ALTITUDE)?;
    let offset = next(OFFSET)?;
    let tilt = next(TILT)?;

    // Une courbe, pas une polyligne : douze sommets, c'est douze ruptures de
    // cap, et à huit kilomètres par seconde ça se voit comme des à-coups.
    let around = ClosedSpline::from_degrees(&loop_points(offset), 64);
    // Sous l'horizontale : le complément de l'inclinaison depuis la verticale.
    let pitch = (90.0 - tilt).to_radians();
    let total = (minutes * 60.0 * fps).round() as usize;

    let mut tape = Tape::recording(&path)?;
    for n in 0..total {
        let step = around.at(n as f64 / total as f64);
        tape.push(Frame {
            position: geodetic_to_ecef(step.here.0, step.here.1, altitude),
            direction: step.look(pitch),
            // La verticale locale : elle tient l'horizon droit, et l'inclinaison
            // la rend utilisable — au nadir strict elle serait parallèle à la
            // visée et le cadrage serait indéfini.
            up: step.up,
            fovy: 45f64.to_radians(),
        });
    }

    let written = tape.finish()?;
    println!(
        "{path}: {written} frames — {minutes:.0} min autour de {:.0} km de Pyrénées à {:.0} km, \
         penchée de {tilt:.0}° sur la verticale, flancs à {offset:.2}° de la crête",
        around.length_m() / 1000.0,
        altitude / 1000.0
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_tape::path::ClosedSpline;

    /// Le cap ne doit jamais sauter d'une frame à l'autre.
    ///
    /// C'est la mesure de « sans coude », prise sur la trajectoire réellement
    /// volée plutôt que sur une figure d'essai : 2880 pas, à la cadence du
    /// film. Un sommet de polyligne y apparaîtrait comme un pas unique portant
    /// tout l'angle du virage ; une courbe étale le même virage sur des
    /// dizaines de pas.
    ///
    /// Le seuil est en degrés PAR FRAME. Le virage le plus serré de la boucle
    /// est un demi-tour de cap, et il a tout le cap pour se faire : à 8 km/s
    /// sur un arc d'une cinquantaine de kilomètres de rayon, ça fait moins d'un
    /// demi-degré par frame.
    fn worst_turn_per_frame(points: &[(f64, f64)], frames: usize) -> f64 {
        let path = ClosedSpline::from_degrees(points, 64);
        let bearing = |i: usize| path.at(i as f64 / frames as f64).bearing;
        (0..frames)
            .map(|i| {
                let d = bearing((i + 1) % frames) - bearing(i);
                let d = (d + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU)
                    - std::f64::consts::PI;
                d.abs().to_degrees()
            })
            .fold(0.0f64, f64::max)
    }

    #[test]
    fn the_flown_path_never_jerks() {
        let worst = worst_turn_per_frame(&loop_points(OFFSET), 2880);
        assert!(
            worst < 2.0,
            "le cap saute de {worst:.2}° sur une frame — c'est un à-coup visible"
        );
    }

    /// Et les caps sont ce qui rend ça vrai : sans les points qui les
    /// arrondissent, le demi-tour se fait sur un seul segment et le cap saute.
    ///
    /// Vérifié en retirant les deux points, ce qui doit faire échouer la mesure
    /// ci-dessus — sinon elle ne prouverait rien.
    #[test]
    fn without_the_end_caps_the_turn_is_abrupt() {
        let mut bare: Vec<(f64, f64)> = CREST.iter().map(|&(a, b)| (a, b + OFFSET)).collect();
        bare.extend(CREST.iter().rev().map(|&(a, b)| (a, b - OFFSET)));
        let with_caps = worst_turn_per_frame(&loop_points(OFFSET), 2880);
        let without = worst_turn_per_frame(&bare, 2880);
        assert!(
            without > with_caps * 1.5,
            "les caps ne changent rien au virage: {without:.2}° sans, {with_caps:.2}° avec"
        );
    }
}
