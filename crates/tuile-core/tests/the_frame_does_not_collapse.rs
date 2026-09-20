// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Le symptôme, bout en bout : une frame qui s'effondre quand rien n'a bougé.
//!
//! Le test unitaire voisin (`nearest_content_distance_does_not_jump…`) prouve
//! la cause — une distance qui saute. Celui-ci prouve ce qu'on voit à l'écran,
//! et il le prouve à travers la traversée publique, sans rien savoir de la
//! fonction fautive. Les deux sont nécessaires : un correctif qui stabiliserait
//! `d_near` en cassant le disque autrement laisserait le premier vert.
//!
//! **Mesuré sur un vrai plan**, le tour des Pyrénées à 50 km d'altitude, le
//! 20 septembre 2026. Entre deux frames consécutives — 317 mètres de vol — la
//! sélection passe de 270 tuiles à 26, et la profondeur atteinte de 12 à 10.
//! Cinquante-deux frames sur 2880 sont dans cet état, toutes à la couture de
//! la boucle au-dessus de l'Atlantique, et c'est ce que l'œil lit comme une
//! texture qui fait du yoyo sur la mer.
//!
//! Aucune autre porte n'y est pour rien : `deferred_subtrees`, `held_but_drawn`
//! et `requested` valent zéro sur les deux frames. Seule la tarification
//! change.

use std::collections::HashSet;

use glam::{dvec2, dvec3};
use tuile_core::tileset::Tileset;
use tuile_core::traversal::{traverse, Config, ResidencyView, TraversalOutput, ViewState};
use url::Url;

/// Deux branches que la caméra frôle, et une tour qu'elle regarde.
///
/// Les deux premières reproduisent la configuration mesurée : au sommet de
/// l'arbre, la caméra est *à l'intérieur* des volumes, donc les distances
/// candidates sont sous-métriques et leur ordre bascule pour un déplacement
/// d'un mètre. L'une mène à du contenu tout près, l'autre très loin.
///
/// La tour est le sol : une chaîne de tuiles de plus en plus fines, dans l'axe
/// de la caméra. C'est elle qu'on compte, et c'est elle qui s'effondre quand la
/// tarification bascule.
fn a_globe_in_miniature() -> Tileset {
    let mut tower = String::from(
        r#"{ "boundingVolume": { "sphere": [0, 0, -500, 400] },
             "geometricError": 512, "refine": "REPLACE",
             "content": { "uri": "t0.glb" }, "children": ["#,
    );
    // Huit niveaux, l'erreur géométrique divisée par deux à chaque fois : de
    // quoi que la profondeur atteinte ait de la place pour varier.
    let mut close = String::new();
    for level in 1..=8 {
        let error = 512.0 / f64::from(1 << level);
        let radius = 400.0 / f64::from(1 << level);
        tower.push_str(&format!(
            r#"{{ "boundingVolume": {{ "sphere": [0, 0, -500, {radius}] }},
                 "geometricError": {error}, "refine": "REPLACE",
                 "content": {{ "uri": "t{level}.glb" }}, "children": ["#
        ));
        close.push_str("]}");
    }
    tower.push_str(&close);
    tower.push_str("]}");

    let json = format!(
        r#"{{ "asset": {{ "version": "1.1" }}, "geometricError": 4000,
             "root": {{
               "boundingVolume": {{ "sphere": [0, 0, 0, 100000] }},
               "geometricError": 2000, "refine": "REPLACE",
               "children": [
                 {{ "boundingVolume": {{ "sphere": [-1000, 0, 0, 1000.5] }},
                    "geometricError": 1000, "refine": "REPLACE",
                    "children": [{{
                      "boundingVolume": {{ "sphere": [0, 0, -200, 10] }},
                      "geometricError": 0,
                      "content": {{ "uri": "near.glb" }} }}] }},
                 {{ "boundingVolume": {{ "sphere": [1002, 0, 0, 1001.5] }},
                    "geometricError": 1000, "refine": "REPLACE",
                    "children": [{{
                      "boundingVolume": {{ "sphere": [50000, 0, 0, 10] }},
                      "geometricError": 0,
                      "content": {{ "uri": "far.glb" }} }}] }},
                 {tower}
               ] }} }}"#
    );
    let base = Url::parse("file:///t/tileset.json").expect("url");
    Tileset::from_json_bytes(json.as_bytes(), &base).expect("tileset")
}

fn looking_down_from(x: f64) -> ViewState {
    ViewState::perspective(
        dvec3(x, 0.0, 0.0),
        dvec3(0.0, 0.0, -1.0),
        dvec3(0.0, 1.0, 0.0),
        dvec2(1920.0, 1440.0),
        45f64.to_radians(),
    )
}

fn pass(ts: &Tileset, x: f64) -> TraversalOutput {
    // Le disque uniforme, qui est la façon dont un rendu par lot tarife une
    // frame : tout le champ à la finesse de sa tuile la plus proche, parce
    // qu'une frontière de niveau de détail est un mur en travers de l'image.
    let config = Config {
        uniform_detail: true,
        uniform_detail_radius: 8.0,
        ..Config::default()
    };
    let mut out = TraversalOutput::default();
    traverse(
        ts,
        &ResidencyView::default(),
        &[looking_down_from(x)],
        &config,
        0,
        &HashSet::new(),
        &mut out,
    );
    out
}

/// Un mètre de vol ne peut pas emporter la moitié de l'image.
///
/// La caméra avance d'un mètre, en ligne droite, en regardant la même chose.
/// Rien dans la scène n'a changé. Ce que la traversée décide de montrer doit
/// rester à peu près le même — c'est la moindre des choses qu'on attende, et
/// c'est ce qui sépare un film d'une suite d'images.
///
/// On mesure la **profondeur atteinte** et le **nombre de demandes**, pas
/// `selected` : sélectionner exige de la résidence, et avec une résidence vide
/// les deux passes rendent zéro. Comparer zéro à zéro, c'est un test qui passe
/// sans rien prouver — le premier jet de celui-ci le faisait.
#[test]
fn a_metre_of_flight_does_not_halve_the_frame() {
    let ts = a_globe_in_miniature();
    let before = pass(&ts, 0.0);
    let after = pass(&ts, 1.0);

    let (da, db) = (before.stats.max_depth, after.stats.max_depth);
    assert!(
        da.abs_diff(db) <= 1,
        "un mètre de vol fait passer la profondeur de {da} à {db} : \
         la scène n'a pas bougé, et chaque niveau vaut quatre fois les tuiles"
    );

    let (ra, rb) = (before.stats.requested, after.stats.requested);
    let (lo, hi) = (ra.min(rb), ra.max(rb));
    assert!(
        hi <= lo * 2,
        "un mètre de vol fait passer les demandes de {ra} à {rb}"
    );
}

/// Et ce n'est aucune des portes qui refusent délibérément.
///
/// Sans ça, le test ci-dessus passerait pour un effondrement légitime : le
/// culling, les sous-arbres différés et les tenus-mais-dessinés sont trois
/// façons parfaitement correctes de réduire une sélection. Ici elles sont
/// toutes muettes, donc ce qui reste est la tarification elle-même.
#[test]
fn nothing_legitimate_explains_the_collapse() {
    let ts = a_globe_in_miniature();
    for x in [0.0, 1.0] {
        let out = pass(&ts, x);
        assert_eq!(out.stats.deferred_subtrees, 0, "x={x}");
        assert_eq!(out.stats.held_but_drawn, 0, "x={x}");
    }
}
