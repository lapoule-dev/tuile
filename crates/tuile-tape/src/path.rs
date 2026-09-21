// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Flying a camera along a closed polyline on the ellipsoid.
//!
//! Two tapes are built this way — the border of France and a loop around the
//! Pyrenees — and they differ only in where they go and how the camera is
//! pointed once it gets there. Everything between those two decisions is the
//! same geodesy, and it was copied verbatim the first time, which is how a
//! correction to the ellipsoid ends up applied to one tape and not the other.

/// WGS84. `A` is the equatorial radius; `E2` the first eccentricity squared,
/// which is the flattening a sphere ignores.
pub const A: f64 = 6_378_137.0;
pub const E2: f64 = 6.694_379_990_141_32e-3;

/// Geodetic to ECEF on the WGS84 ellipsoid.
///
/// The prime vertical radius `n` is what a sphere leaves out, and leaving it
/// out costs ten kilometres of altitude at French latitudes — a fifth of the
/// height these paths are flown at. Longitude and latitude in **radians**,
/// height in metres above the ellipsoid.
pub fn geodetic_to_ecef(lon: f64, lat: f64, height: f64) -> [f64; 3] {
    let (s, c) = (lat.sin(), lat.cos());
    let n = A / (1.0 - E2 * s * s).sqrt();
    [
        (n + height) * c * lon.cos(),
        (n + height) * c * lon.sin(),
        (n * (1.0 - E2) + height) * s,
    ]
}

/// Great-circle distance in radians between two points given in radians.
pub fn arc(a: (f64, f64), b: (f64, f64)) -> f64 {
    let d = (a.1.sin() * b.1.sin() + a.1.cos() * b.1.cos() * (b.0 - a.0).cos()).clamp(-1.0, 1.0);
    d.acos()
}

/// A point `t` of the way along the great circle from `a` to `b`.
///
/// Spherical interpolation rather than linear on longitude and latitude: linear
/// drifts off the shortest path and, near the poles, moves at a wildly
/// different speed for the same `t`. France is not near a pole, and doing it
/// properly costs four trigonometric calls.
pub fn along(a: (f64, f64), b: (f64, f64), t: f64) -> (f64, f64) {
    let d = arc(a, b);
    if d < 1e-12 {
        return a;
    }
    let (sa, sb) = (((1.0 - t) * d).sin() / d.sin(), (t * d).sin() / d.sin());
    let x = sa * a.1.cos() * a.0.cos() + sb * b.1.cos() * b.0.cos();
    let y = sa * a.1.cos() * a.0.sin() + sb * b.1.cos() * b.0.sin();
    let z = sa * a.1.sin() + sb * b.1.sin();
    (y.atan2(x), z.atan2((x * x + y * y).sqrt()))
}

/// The local east-north-up frame at a point, in radians.
pub fn frame_at(lon: f64, lat: f64) -> ([f64; 3], [f64; 3], [f64; 3]) {
    let east = [-lon.sin(), lon.cos(), 0.0];
    let up = [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()];
    let north = [
        up[1] * east[2] - up[2] * east[1],
        up[2] * east[0] - up[0] * east[2],
        up[0] * east[1] - up[1] * east[0],
    ];
    (east, north, up)
}

/// Where the camera is at one instant along the loop.
pub struct Step {
    /// Longitude and latitude, in radians.
    pub here: (f64, f64),
    /// Local east, north and up at `here`.
    pub east: [f64; 3],
    pub north: [f64; 3],
    pub up: [f64; 3],
    /// Heading, radians clockwise from north — the way the camera is going.
    pub bearing: f64,
}

impl Step {
    /// The horizontal unit vector along the bearing.
    pub fn forward(&self) -> [f64; 3] {
        let (cb, sb) = (self.bearing.cos(), self.bearing.sin());
        [
            self.north[0] * cb + self.east[0] * sb,
            self.north[1] * cb + self.east[1] * sb,
            self.north[2] * cb + self.east[2] * sb,
        ]
    }

    /// `forward`, tipped `pitch` radians below the horizontal.
    ///
    /// At a pitch of zero this is the horizon; at π/2 it is straight down, and
    /// then `up` can no longer serve as screen-up — it is parallel to the
    /// direction. A nadir tape has to use [`Step::forward`] for that instead.
    pub fn look(&self, pitch: f64) -> [f64; 3] {
        let f = self.forward();
        let (cp, sp) = (pitch.cos(), pitch.sin());
        [
            f[0] * cp - self.up[0] * sp,
            f[1] * cp - self.up[1] * sp,
            f[2] * cp - self.up[2] * sp,
        ]
    }
}

/// A closed curve through the waypoints, with no corners.
///
/// A polyline turns instantly at each vertex. Flown at eight kilometres a
/// second that reads as a jolt — the heading snaps, and so does everything the
/// camera is pointed by. What is wanted is a curve whose tangent is continuous
/// *including across the seam* where the loop closes.
///
/// Catmull–Rom, from the `splines` crate rather than written here. The
/// construction is six lines — control points a third of the way along the
/// neighbours' chord — and that is exactly why it gets copied wrong: simple
/// enough to think you know it, subtle enough that a bad seam does not show on
/// a drawing. It interpolates its waypoints, so the path stays **on** the crest
/// it was given, where a B-spline would smooth them away.
///
/// The curve is flattened into a fine polyline and handed to [`ClosedPath`],
/// which already walks a polyline at constant ground speed. That is deliberate:
/// a spline's own parameter is not arc length, and stepping it uniformly would
/// trade the corners for a camera that speeds up and slows down instead.
pub struct ClosedSpline;

impl ClosedSpline {
    /// Waypoints in **degrees**, closed automatically. `per_span` is how finely
    /// each span is flattened — 64 puts the sampling error far below a tile.
    pub fn from_degrees(waypoints: &[(f64, f64)], per_span: usize) -> ClosedPath {
        use splines::{Interpolation, Key, Spline};

        let n = waypoints.len();
        assert!(n >= 3, "une boucle demande au moins trois points");
        let per_span = per_span.max(2);

        // Catmull–Rom a besoin d'un voisin de chaque côté du segment qu'il
        // évalue. Pour refermer la boucle on prolonge donc la liste de part et
        // d'autre avec les points d'en face : la couture devient un segment
        // comme un autre, et c'est précisément là qu'un raccord se rate.
        //
        // Une spline par coordonnée, et ce n'est pas un contournement : la
        // construction de Catmull–Rom est composante par composante, donc deux
        // splines scalaires décrivent exactement la même courbe qu'une spline
        // vectorielle. Le crate n'interpole que ce qui implémente son trait
        // `Interpolate`, et `f64` l'implémente là où `[f64; 2]` ne l'implémente
        // pas sans une de ses features optionnelles.
        // Paramétrisation **centripète**, pas uniforme.
        //
        // Catmull–Rom avec des `t` entiers suppose des segments de longueurs
        // comparables. Les nôtres ne le sont pas du tout : les flancs font des
        // centaines de kilomètres, les caps quelques dizaines. La courbe déborde
        // alors dans les virages serrés, et le cap saute — mesuré 3,12° par
        // frame sur la boucle pyrénéenne, un à-coup parfaitement visible.
        //
        // Espacer les `t` selon la racine de la longueur de corde — l'exposant
        // 0,5 de Catmull–Rom centripète — supprime ce débordement, et c'est le
        // résultat classique : cette variante-là ne produit ni boucle ni
        // rebroussement, quelles que soient les longueurs.
        let idx = |i: i64| i.rem_euclid(n as i64) as usize;
        let chord = |i: i64| {
            let a = waypoints[idx(i)];
            let b = waypoints[idx(i + 1)];
            ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2))
                .sqrt()
                .sqrt()
                .max(1e-9)
        };
        // Les temps des clés, du voisin d'avant au deuxième voisin d'après, pour
        // que la couture ait de part et d'autre exactement ce qu'un segment
        // ordinaire a.
        let mut times = Vec::with_capacity(n + 3);
        let mut t = -chord(-1);
        times.push(t);
        for i in 0..=(n as i64 + 1) {
            t += chord(i - 1);
            times.push(t);
        }
        let span = (times[1], times[n + 1]);

        let spline_of = |pick: fn((f64, f64)) -> f64| {
            let keys: Vec<_> = (-1..=(n as i64 + 1))
                .enumerate()
                .map(|(k, i)| {
                    Key::new(times[k], pick(waypoints[idx(i)]), Interpolation::CatmullRom)
                })
                .collect();
            Spline::from_vec(keys)
        };
        let lon = spline_of(|p| p.0);
        let lat = spline_of(|p| p.1);

        let total = n * per_span;
        let flat: Vec<(f64, f64)> = (0..total)
            .map(|k| {
                let u = k as f64 / total as f64;
                let t = span.0 + u * (span.1 - span.0);
                let at = |s: &Spline<f64, f64>| {
                    s.sample(t)
                        .expect("t reste dans l'intervalle couvert par les clés")
                };
                (at(&lon), at(&lat))
            })
            .collect();
        ClosedPath::from_degrees(&flat)
    }
}

/// A closed polyline, walked at constant ground speed.
pub struct ClosedPath {
    points: Vec<(f64, f64)>,
    legs: Vec<f64>,
    perimeter: f64,
}

impl ClosedPath {
    /// From points in **degrees**, closed automatically.
    pub fn from_degrees(points: &[(f64, f64)]) -> Self {
        let points: Vec<(f64, f64)> = points
            .iter()
            .map(|&(lon, lat)| (lon.to_radians(), lat.to_radians()))
            .collect();
        // Chaque segment reçoit des frames au prorata de sa longueur, pour que
        // la caméra avance à vitesse constante plutôt que de sprinter sur les
        // courts segments et de ramper sur les longs.
        let legs: Vec<f64> = (0..points.len())
            .map(|i| arc(points[i], points[(i + 1) % points.len()]))
            .collect();
        let perimeter = legs.iter().sum();
        Self {
            points,
            legs,
            perimeter,
        }
    }

    /// The loop's length, in metres.
    pub fn length_m(&self) -> f64 {
        self.perimeter * A
    }

    /// The point `fraction` of the way round, in `[0, 1)`.
    pub fn position(&self, fraction: f64) -> (f64, f64) {
        let mut remaining = self.perimeter * fraction.rem_euclid(1.0);
        let mut leg = 0;
        while leg < self.legs.len() && remaining > self.legs[leg] {
            remaining -= self.legs[leg];
            leg += 1;
        }
        let leg = leg.min(self.legs.len() - 1);
        let t = if self.legs[leg] > 1e-12 {
            remaining / self.legs[leg]
        } else {
            0.0
        };
        along(
            self.points[leg],
            self.points[(leg + 1) % self.points.len()],
            t,
        )
    }

    /// Where the camera is `fraction` of the way round, in `[0, 1)`.
    pub fn at(&self, fraction: f64) -> Step {
        let here = self.position(fraction);
        // Le cap se prend sur une avance en longueur d'arc GLOBALE, et non à
        // l'intérieur du segment courant.
        //
        // Pris dans le segment, le cap vaut la direction de ce segment : il est
        // donc constant sur toute sa longueur, puis saute au suivant. Une
        // polyligne aplatie à 512 segments parcourue en 2880 frames garde alors
        // le même cap pendant six frames avant de tourner d'un coup — un
        // escalier, et c'est la caméra qui le subit, pas seulement la mesure.
        // Mesuré sur la boucle pyrénéenne : 3,1° de saut sur une frame.
        //
        // Un millième de tour d'avance, c'est court devant le rayon des virages
        // et long devant un segment : le cap redevient une fonction continue de
        // la position, quelle que soit la finesse de l'aplatissement.
        const LOOKAHEAD: f64 = 1e-3;
        let ahead = self.position(fraction + LOOKAHEAD);
        let (east, north, up) = frame_at(here.0, here.1);
        let bearing = {
            let (dlon, dlat) = (ahead.0 - here.0, ahead.1 - here.1);
            (dlon * here.1.cos()).atan2(dlat)
        };
        Step {
            here,
            east,
            north,
            up,
            bearing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le tour doit faire le tour : parcourir la fraction 0 à 1 doit revenir au
    /// point de départ, et pas s'arrêter au dernier sommet.
    #[test]
    fn a_closed_path_comes_back() {
        let square = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
        let path = ClosedPath::from_degrees(&square);
        let start = path.at(0.0).here;
        let end = path.at(0.999_9).here;
        assert!(arc(start, end) * A < 40_000.0, "le tour ne se referme pas");
    }

    /// À vitesse constante : une même fraction du tour couvre la même distance,
    /// qu'elle tombe sur un segment long ou sur un segment court.
    ///
    /// La figure est volontairement déséquilibrée — deux côtés de mille
    /// kilomètres, deux de cinquante. Sur un carré, le prorata par segment et
    /// une interpolation naïve sur l'index des points donnent exactement la
    /// même chose, puisque tous les segments ont la même longueur : le premier
    /// état de ce test était donc vert dans les deux cas et ne prouvait rien.
    /// Vérifié en retirant le prorata, ce qui doit faire échouer celui-ci.
    #[test]
    fn the_speed_is_constant() {
        // Deux côtés longs en longitude, deux côtés courts en latitude.
        let oblong = [(0.0, 0.0), (10.0, 0.0), (10.0, 0.5), (0.0, 0.5)];
        let path = ClosedPath::from_degrees(&oblong);
        let d = |a: f64, b: f64| arc(path.at(a).here, path.at(b).here) * A;
        // Une fenêtre au milieu d'un côté long, une au milieu d'un côté court.
        let (on_long, on_short) = (d(0.20, 0.21), d(0.48, 0.49));
        assert!(
            (on_long - on_short).abs() / on_long < 0.05,
            "vitesses inégales: {on_long:.0} m sur un long côté, \
             {on_short:.0} m sur un court"
        );
    }

    /// Le piège du nadir, épinglé : à 90° la visée est l'opposé de la verticale
    /// locale, donc `up` ne peut plus servir de haut d'écran — il lui est
    /// parallèle. Un tape nadir doit prendre le cap à la place.
    #[test]
    fn straight_down_is_parallel_to_the_local_vertical() {
        let path = ClosedPath::from_degrees(&[(0.0, 43.0), (1.0, 43.0), (1.0, 44.0)]);
        let step = path.at(0.1);
        let look = step.look(std::f64::consts::FRAC_PI_2);
        let dot: f64 = (0..3).map(|i| look[i] * step.up[i]).sum();
        assert!(
            dot < -0.999,
            "la visée nadir n'est pas l'opposé de up: {dot}"
        );
        let with_forward: f64 = (0..3).map(|i| look[i] * step.forward()[i]).sum();
        assert!(
            with_forward.abs() < 1e-9,
            "le cap n'est pas perpendiculaire à la visée nadir: {with_forward}"
        );
    }

    /// Le plus grand saut de cap entre deux échantillons réguliers, en degrés.
    ///
    /// C'est la mesure d'un coude : une polyligne tourne d'un coup au sommet,
    /// donc un seul pas porte tout l'angle ; une courbe sans coude étale le même
    /// virage sur beaucoup de pas.
    fn worst_turn(path: &ClosedPath, samples: usize) -> f64 {
        let bearing = |i: usize| path.at(i as f64 / samples as f64).bearing;
        (0..samples)
            .map(|i| {
                let d = bearing((i + 1) % samples) - bearing(i);
                // Ramené dans [-π, π] : sans ça, passer par le nord compte
                // comme un virage de 360°.
                let d = (d + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU)
                    - std::f64::consts::PI;
                d.abs().to_degrees()
            })
            .fold(0.0f64, f64::max)
    }

    /// Les mêmes sommets, en polyligne : les coudes sont là, et c'est ce que
    /// mesure `worst_turn`. Ce test existe pour que le suivant prouve quelque
    /// chose — sans lui, un seuil généreux passerait dans les deux cas.
    #[test]
    fn a_polyline_turns_abruptly_at_its_vertices() {
        let loop_ = [
            (0.0, 43.0),
            (1.0, 43.2),
            (2.0, 43.0),
            (2.0, 42.5),
            (1.0, 42.3),
            (0.0, 42.5),
        ];
        let worst = worst_turn(&ClosedPath::from_degrees(&loop_), 720);
        assert!(
            worst > 30.0,
            "une polyligne devrait avoir des coudes: {worst:.1}°"
        );
    }

    /// Et la courbe ne doit pas en avoir : « sans coude » au sens
    /// mathématique, c'est-à-dire tangente continue, y compris à la couture où
    /// la boucle se referme — le raccord qu'on rate d'habitude.
    /// Le seuil est RELATIF à la polyligne, pas absolu.
    ///
    /// Une boucle fermée tourne de 360° quoi qu'il arrive : ce qui distingue une
    /// courbe d'une polyligne n'est pas de tourner moins, c'est d'étaler le
    /// virage au lieu de le concentrer sur un pas. Un seuil en degrés ne dit
    /// donc rien sans la figure — le premier état de ce test échouait à 3,4°
    /// sur une courbe parfaitement lisse, simplement parce que l'hexagone
    /// d'essai a des virages serrés.
    #[test]
    fn a_spline_has_no_corner_anywhere() {
        let loop_ = [
            (0.0, 43.0),
            (1.0, 43.2),
            (2.0, 43.0),
            (2.0, 42.5),
            (1.0, 42.3),
            (0.0, 42.5),
        ];
        let polyline = worst_turn(&ClosedPath::from_degrees(&loop_), 720);
        let spline = worst_turn(&ClosedSpline::from_degrees(&loop_, 64), 720);
        assert!(
            spline * 5.0 < polyline,
            "la courbe concentre encore son virage: {spline:.1}° contre {polyline:.1}° \
             pour la polyligne des mêmes sommets"
        );
    }

    /// Et elle passe bien PAR les points, sinon elle ne suit plus la crête
    /// qu'on lui a donnée — c'est ce qui distingue Catmull–Rom d'une B-spline.
    #[test]
    fn a_spline_goes_through_its_waypoints() {
        let loop_ = [
            (0.0, 43.0),
            (1.0, 43.2),
            (2.0, 43.0),
            (2.0, 42.5),
            (1.0, 42.3),
            (0.0, 42.5),
        ];
        let path = ClosedSpline::from_degrees(&loop_, 64);
        for &(lon, lat) in &loop_ {
            let target = (lon.to_radians(), lat.to_radians());
            let nearest = (0..2000)
                .map(|i| arc(path.at(i as f64 / 2000.0).here, target) * A)
                .fold(f64::INFINITY, f64::min);
            assert!(
                nearest < 2_000.0,
                "le point ({lon}, {lat}) est à {nearest:.0} m de la courbe"
            );
        }
    }

    /// Et le cas ordinaire reste valide : à 50° sous l'horizontale, la
    /// verticale locale fait un haut d'écran parfaitement utilisable.
    #[test]
    fn a_pitched_look_still_admits_the_vertical_as_up() {
        let path = ClosedPath::from_degrees(&[(0.0, 43.0), (1.0, 43.0), (1.0, 44.0)]);
        let step = path.at(0.1);
        let look = step.look(50f64.to_radians());
        let dot: f64 = (0..3).map(|i| look[i] * step.up[i]).sum();
        assert!(
            dot.abs() < 0.95,
            "visée et verticale trop colinéaires: {dot}"
        );
    }
}
