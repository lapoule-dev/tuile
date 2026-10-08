<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Les rendus qu'on garde

Gitignoré : sept cents mégaoctets par film, et ils sont reproductibles — le
pack vit dans R2, la trajectoire dans `crates/tuile-tape/src/bin/`. Ce
répertoire existe pour qu'un film survive à un nettoyage de `/tmp`, pas pour
faire office d'archive.

Chaque entrée dit **de quoi refaire le film**, parce qu'un mp4 sans ses
paramètres ne se compare à rien.

**Bande, pack, film : les trois vont ensemble.** La cuisson dépose désormais
sa trajectoire à côté du pack — `packs/<digest>/<plage>.mcap` — parce que
c'est elle qui définit les deux autres. Les films d'avant le 19 septembre 2026
n'en ont pas : leur bande a été fabriquée dans le conteneur et jetée, et la
seule façon de retrouver leur tracé est de rejouer les caméras du pack avec
`tuile-bake --tape-from`. Les générateurs évoluent, donc recuire la même
chaîne d'arguments ne donne plus le même vol.

## `synthetic-orbit-1280x720-ss2-webgpu.mp4`

Premier film du moteur **wgpu-film** (branche `spike/wgpu-film`), rendu dans
le navigateur : deux Web Workers (wasm + WebGPU), H.264 par WebCodecs, mp4
assemblé par `tuile-mp4`. Pas de vraie scène : un pack synthétique.

- pack `cargo run --release -p tuile-film --example synthetic_pack -- synthetic.tuilepack 240`
  (grille 16 × 16 km à 42,8° N 0,5° E, orbite de 5 km à 2 500 m, drapé changé
  à la frame 121), scène `synthetic`, 7,3 Mo, 512 tuiles
- page `examples/film-web/www/?pack=synthetic.tuilepack`, Chrome sur M2
- 1280×720, suréchantillonnage 2× (4 éch./pixel), 30 images/s, 16 Mbit/s,
  `avc1.640028` ; 240 frames en **4,9 s** (49 images/s), dont ~20 ms/frame
  d'attente encodeur + GPU, 3,5 ms de travail CPU dans `next()`

## `pyrenees-2min-nadir-50km-tilestore-sse6.mp4`

Le même plan à **SSE 6** au lieu de 3, cuit à travers le store (projections,
pool de finition).

- pack `packs/24cd8199f4c13cfa/1-2880.tuilepack` (bucket `tracks-bucket`),
  scène `ef5dd4813ae33cbb`, **888 Mo** (1,73 Go à SSE 3)
- cuisson `tuile-bake-9s6b9` : **2 min 44 s** (3 min 02 s à SSE 3)
- rendu `tuile-render-nqpb5` : 3 × L4, **9 min 52 s** (10 min 32 s à SSE 3) ;
  2880 frames, 1920×1440, 624 Mo

À l'œil : un peu plus doux, et plus de tuiles d'imagerie grossières restent à
l'écran — donc plus de pavés de couleur (un bloc vert franc à 60 s, absent à
SSE 3). Le pack est deux fois plus léger, le rendu gagne 40 s.

## `pyrenees-2min-nadir-50km-tilestore-projection-v4.mp4`

Cinquième passe : tuiles finies sur un pool d'un thread par cœur, et zone
`top` projetée entière. **La cuisson ne parle plus au réseau.**

- pack W5 : `tile-store-test/W5/1-2880.tuilepack`, scène `90baed1a239165d4`,
  4 266 tuiles ; cuisson `tuile-bake-m6bwr` ; rendu `tuile-render-r6hx6`
  (3 × L4, 10 min 32 s), 2880 frames, 663 Mo
- cuisson **3 min 02 s** au total (contre 7 min 52 s sans store) : cuisson
  111 s dont frame 1 35 s (106 s avant le pool), les 2 879 autres 74 s ;
  projection 5,8 s pour 377 Mo en 320 requêtes ; vidage 0,8 s
- lectures : 41 027 dans les projections, 40 replis (des absences), **0 vers
  R2**
- image `blender-globe:5.1-su` (commit `9a7404e`, pool `79d1922`)

Toujours transparent : 4 142 tuiles communes avec B0, identiques.

## `pyrenees-2min-nadir-50km-tilestore-projection-v3.mp4`

Quatrième passe : les zones de cellule atteintes projetées entières.

- pack W4 : `tile-store-test/W4/1-2880.tuilepack`, scène `90baed1a239165d4`,
  4 251 tuiles ; cuisson `tuile-bake-pgjkq`, rendu 3 × L4, 2880 frames, 663 Mo
- **5 min 30 s** au total ; cuisson 234 s (frame 1 105 s, les autres 128 s) ;
  projection 7,6 s, 377 Mo en 320 requêtes
- lectures : 18 756 dans les projections, 21 842 replis — inchangés, ce qui a
  désigné `top` : la cuisson épingle une pyramide grossière du globe entier
  (niveaux 0 à 5), que le filtre par distance coupait. Corrigé ensuite
  (`top` gardée entière, commit `9a7404e`)
- image `blender-globe:5.1-su` (commit `9a8ea20`)

Toujours transparent : 4 127 tuiles communes avec B0, identiques.

## `pyrenees-2min-nadir-50km-tilestore-projection-v2.mp4`

Troisième passe à travers le store : une requête par archive, imagerie
projetée huit fois plus loin que le terrain.

- pack W3 : `tile-store-test/W3/1-2880.tuilepack`, scène `90baed1a239165d4`,
  4 255 tuiles ; rendu `tuile-render-2drdk`, 3 × L4, 2880 frames, 663 Mo
- cuisson `tuile-bake-hbb2n` : **5 min 49 s** au total (cuisson 242 s dont
  frame 1 106 s et les 2 879 autres 133 s ; projection 8,3 s pour 377 Mo en
  318 requêtes ; vidage 0,9 s)
- lectures : 18 580 dans les projections, encore 21 849 replis vers R2 — le
  filtre par tuile écarte ce que la cuisson utilise ; puisque chaque archive
  arrive entière, la passe suivante garde toutes les tuiles des zones
  atteintes et ne filtre plus que `top`
- image `blender-globe:5.1-su` (commit `afc1a32`)

Toujours transparent : 4 136 tuiles communes avec B0, 4 100 avec W,
identiques.

## `pyrenees-2min-nadir-50km-tilestore-projection.mp4`

Le même plan, cuit **à travers les projections locales** du store : avant la
première frame, la cuisson extrait des zones distantes les tuiles que ses
caméras peuvent voir, dans des PMTiles locaux, puis lit ceux-ci avant R2.

- bande et réglages : ceux de `pyrenees-2min-nadir-50km-tilestore-W.mp4`
- pack W2 : `tile-store-test/W2/1-2880.tuilepack`, scène `90baed1a239165d4`,
  4 392 tuiles, 1,76 Go
- cuisson `tuile-bake-49hwc` : **7 min 30 s** au total (cuisson 5 min 54 s,
  projection 6,3 s, vidage 1,3 s) contre 7 min 53 s sans store et 10 min 57 s
  avec le store sans projection
- projection : 283 zones, 8 362 tuiles gardées sur 25 379, 114 Mo en 341
  requêtes, facteur 12
- lectures : 8 887 dans les projections, **31 660 replis** vers R2 — le filtre
  est trop serré pour l'imagerie (voir plus bas), 22 absentes
- rendu `tuile-render-29f9z` : 3 tâches × L4 ; 2880 frames, 120,000 s, 663 Mo
- image `blender-globe:5.1-su@sha256:db9d5b56…` (branche `feat/tile-server`,
  commit `c2ca99c`)

**Toujours transparent** : 4 179 tuiles communes avec B0, 4 177 avec W, toutes
identiques. **Les replis viennent de l'imagerie** : un drapé de 2048² compose
des tuiles d'imagerie plusieurs niveaux plus fines que la tuile de terrain
qu'il habille, donc à même distance de l'œil l'imagerie utile est de largeur
bien plus petite que ce que le facteur 12 garde.

## `pyrenees-2min-nadir-50km-tilestore-W.mp4`

Le premier film cuit **à travers le store de tuiles** (`tuile-tile-server`,
bucket R2 `tiles-bucket`), store déjà rempli par une cuisson précédente. Même
bande que `pyrenees-2min-nadir-50km-fixed.mp4`.

- bande `tile-store-test/ref-20260920/1-2880.tuilepack.mcap` (copie de
  `packs/cad37e576618a47f/1-2880.tuilepack.mcap`), rejouée telle quelle avec
  `--trajectory pyrenees:2:24:50000:0.40`
- pack W : `tile-store-test/W/1-2880.tuilepack` (bucket `tracks-bucket`),
  scène `90baed1a239165d4`, 4 372 tuiles, 1,75 Go
- imagerie Bing (défaut), viewport 3840×2880, sse 3, 16 échantillons, 1920×1440
- cuisson `tuile-bake-qh8wv` avec `TUILE_TILES_BUCKET=tiles-bucket` : 10 min 57 s
  (cuisson 9 min 01 s, vidage du store 20 s)
- rendu `tuile-render-xq94m` : 3 tâches × L4 ; 2880 frames, 120,000 s, 663 Mo
- image `blender-globe:5.1-su@sha256:497fcd15…` (branche `feat/tile-server`,
  commit `14725be`)

**Le store est transparent.** Quatre cuissons de la même bande — B0 et B0' sans
store, A store froid, W store chaud — comparées tuile à tuile : toutes les
tuiles communes (même id, même drapé) ont des octets identiques (positions,
normales, UV, index, texture, origine), 4 094 à 4 183 par paire.
**La cuisson, elle, n'est pas reproductible** : B0 et B0' ne sélectionnent pas
les mêmes tuiles (203 / 103 de différence), store ou pas. Les packs de
comparaison sont gardés sous `tracks-bucket/tile-store-test/{B0,B0prime,A,W}/`.

**Les damiers de couleur sont bien visibles** — aplats de mer, blocs plus
saturés, raccords entre niveaux : c'est le film témoin de
`docs/18-color-harmonization.md`.

## `pyrenees-2min-nadir-50km.mp4`

2880 frames, 1920×1440, 120,000000 s, 632 Mo. Le premier tour des Pyrénées.

- trajectoire `pyrenees:2:24:50000:0.40` — **polyligne**, visée strictement
  verticale, 50 km
- pack `packs/03eb228f774ab939/1-2880.tuilepack`, scène `091ed8398f93c4dd`
- imagerie Bing (défaut), viewport 3840×2880, sse 3, 16 échantillons

Ses défauts, qui ont motivé le suivant : se lit comme une carte satellite
(pas de relief, pas de sens du vol), et le cap saute aux douze sommets de la
polyligne. Le pack porte aussi les effondrements de sélection décrits plus bas.

## `pyrenees-2min-bezier-90km-tilt20.mp4`

2880 frames, 1920×1440, 120,000000 s, 687 Mo. Le même tour, corrigé.

- trajectoire `pyrenees:2:24:90000:0.40` — **courbe** de Catmull–Rom centripète
  (crate `splines`), caps arrondis, 90 km, visée penchée de 20°
- pack `packs/a2fcaeafc9acb3e5/1-2880.tuilepack`, scène `1f3a9b5bc665e0d4`
- imagerie Bing, viewport 3840×2880, sse 3, 16 échantillons
- 12 min de rendu sur 3 L4, soit 0,21 s/frame cumulées

Saut de cap ramené de plus de 30° à **1,89° par frame**. Le relief apparaît
enfin (neige, contraste piémont/montagne) mais 20° d'inclinaison à 90 km reste
proche d'une vue orthographique : la sensation de vol n'y est pas.

## `pyrenees-2min-sentinel-20km-tilt20.mp4`

Le même tour des Pyrénées que le précédent, descendu de 90 à **20 km** et
drapé de **Sentinel-2** au lieu de Bing. La trajectoire est identique en forme
— spline fermée, penchée de 20° sur la verticale — mais les flancs sont à
0,20° de la crête au lieu de 0,40 : à 20 km le cadre ne porte plus que sur
quelques kilomètres, et l'ancien écart aurait fait perdre la crête.

- `pyrenees:2:24:20000:0.20`, 2880 frames, 24 fps, 120,000 s
- pack `packs/8743dc56923d2b3b/1-2880.tuilepack`, scène `70d87e6c5a8c2646`
- imagerie Sentinel-2 cloudless (asset ion 3954, z0–13, jpg), viewport
  1920×1440, sse 3, 16 échantillons, boost d'imagerie 1
- cuisson `tuile-bake-2d74r` : 942 s, pack de 2,05 Go, budget résident 16 GiB
- rendu `tuile-render-whl7r` : 3 tâches × L4, 12 min

**La clef du pack et le digest de scène diffèrent, et c'est normal.** La clef
dit où sont les octets — elle est calculée par le lanceur, qui ne connaît que
ses propres arguments ; le digest dit de quel globe ils parlent — il couvre les
réglages de traversée résolus, que seul le job connaît. Un rendu a besoin des
deux.

**Trois pannes ont précédé ce film, et chacune masquait la suivante.** La
première tuait la cuisson en OOM avant la frame 1 : une tuile fraîchement cuite
gardait sa mosaïque en RGBA brut, seize mébioctets, multipliés par la
sélection. La deuxième la tuait au bout de 120 s : la patience des réessais
budgétait 182 s sur une tuile, donc la chaîne ne pouvait jamais aboutir dans
une frame. La troisième la faisait boucler sans converger : `--resident-gb`
n'atteignait pas le job de cuisson, qui tournait au défaut de 4 GiB dans un
conteneur de 32 — `loads_started=134720` pour 234 tuiles, quarante-cinq
chargements par tuile. Aucune des trois n'était lisible avant qu'une ligne
`MEMORY` ne sorte les compteurs toutes les quinze secondes.

## `pyrenees-2min-sentinel-10km-60fps.mp4`

Le même tour, descendu à **10 km** et porté à **60 images par seconde** — à 24
le défilement se lisait saccadé, et à cette altitude le sol passe vite.

- `pyrenees:2:60:10000:0.10`, 7200 frames, 60 fps, 120,025 s
- pack `packs/71c30bde9d3799bb/1-7200.tuilepack`, scène `cb23304e21b16c72`
- imagerie Sentinel-2 cloudless (asset 3954), viewport 1920×1440, sse 3,
  16 échantillons, boost d'imagerie 1
- cuisson `tuile-bake-z9svv` : 587 s, pack de **885 Mo** — plus petit que celui
  à 20 km malgré 2,5× plus de frames, parce que le cadre couvre quatre fois
  moins de sol (76 tuiles sélectionnées contre 234)
- rendu `tuile-render-pczpc` : 3 tâches × L4, 28 min

**Les flancs sont à 0,10° et non 0,20.** La convention du tracé est « décalage
≈ largeur du cadre » ; à 10 km le cadre ne fait plus que ~12 km de large contre
23 à 20 km, et l'ancien écart aurait sorti la crête de l'image.

**Sentinel est à sa limite ici.** La cible texel tombe à 5,75 m/pixel alors que
l'asset plafonne à z13, soit ~7 m/texel à cette latitude : chaque texel source
couvre environ 1,2 pixel écran. C'est encore lisible, mais plus bas l'imagerie
serait franchement molle — le terrain, lui, descend plus loin.

**Plus la cadence est haute, moins la cuisson coûte par image.** Mesuré :
`reused=75/75` et 0,0003 s par frame. À 60 i/s deux frames consécutives voient
exactement le même sol, donc tout est déjà dans le pack et il n'y a qu'une
liste d'index à écrire. Le coût est porté par le tracé, pas par le nombre
d'images.

**Ce film a été remuxé, et c'est un défaut de la chaîne, pas du rendu.**
`JOB_FPS` n'atteignait que le générateur de manifeste, jamais `render_usd.py` :
les 7200 poses étaient donc justes — le mouvement est exact — mais Blender
gardait `scene.render.fps = 24` et estampillait le conteneur à 24, soit cinq
minutes pour deux minutes de vol. Le flux h264 étant correct, il a suffi de le
ré-estampiller à 60 sans réencoder. `render_job.sh` passe désormais `--fps`, et
un test le tient.

## `pyrenees-2min-nadir-50km-fixed.mp4`

Le tout premier tour, **refait sur sa propre bande**, une fois l'effondrement
de sélection corrigé. Même vol à six millimètres près que
`pyrenees-2min-nadir-50km.mp4` : c'est la seule façon de comparer la mer.

- bande `packs/cad37e576618a47f/1-2880.tuilepack.mcap` — rejouée telle quelle,
  **pas** régénérée depuis `pyrenees:2:24:50000:0.40` (voir plus bas)
- pack `packs/cad37e576618a47f/1-2880.tuilepack`, scène `feb6abe111588f02`
- imagerie Bing (défaut), viewport 3840×2880, sse 3, 16 échantillons
- 2880 frames, 1920×1440, 120,000 s, 663 Mo, 24 i/s
- cuisson `tuile-bake-4srgq` : 739 s, pack de 1,76 Go, budget résident 16 GiB
- rendu `tuile-render-5lpz2` : 3 tâches × L4, 11 min 21 s

**Le yoyo au-dessus de l'Atlantique a disparu.** Les 52 frames qui tombaient à
26 tuiles — frames 2852-2880 puis 1-39 — tiennent maintenant entre 240 et 271,
et la recherche d'une frame sous cent tuiles sur les 2880 ne renvoie rien. La
cause était une descente gloutonne qui choisissait « l'enfant le plus proche »
alors que l'œil, à 50 km, est *à l'intérieur* du volume englobant de chaque
tuile proche de la racine : `distance_to_point` y rend zéro, la clé comparait
des zéros, et 317 m de vol suffisaient à changer de branche. `d_near` basculait
entre 41,0 et 133,9 km, et comme le disque uniforme tarife toutes les erreurs
écran de la passe à cette distance, la sélection était divisée par dix. La
descente suit désormais **toutes** les branches que le rayon de visée traverse
et prend le minimum.

**Le premier rendu de ce pack est mort en neuf secondes, et c'est la leçon de
l'entrée.** Lancé avec la même chaîne `pyrenees:2:24:50000:0.40` que la
cuisson, il a régénéré un vol penché de 20° — `pyrenees-tape` a changé depuis
le premier film — alors que le pack avait été cuit sur la bande nadir
archivée. Un pack répond à une caméra par la pose, à un mètre et un
milliradian près : l'écart lu était 0,349066 rad, pour une position juste à
six millimètres. Le rendu tire maintenant la bande déposée à côté du pack, et
un test tient les deux scripts d'accord.

## `orbite-8frames-1920.mp4`

8 frames, 1920×1440, 1,5 Mo. Pas un film : la **première image** où toute la
chaîne a tenu — manifeste entré par le pont Blender, procédural cuisant pour
`/freeCamera`, textures lues dans le tuilepack. Gardée comme témoin.

- pack `packs/62fed640f2d6a36a/1-24.tuilepack`, orbite à 8 km
- c'est le run qui a mesuré la cuisson incrémentale : `cook #1 built=444`,
  puis `kept=444 built=0`, contre 444 reconstruites à chaque frame avant.

## `testA-poll-960-render.mp4`, `testB-wait-960-render.mp4`

1 frame chacun, 960×720, 22 septembre 2026. La paire qui a clos l'interblocage
de la boucle de rendu Hydra — **pixel pour pixel identiques** (écart moyen
0,00/255), seule la façon d'attendre diffère.

- trajectoire `orbit:64:2.17:42.52:8000:5000`, frame 1
- pack `packs/ab87620af91e0549/1-64.tuilepack`, scène `0f766ae6c728c202`
  (cuit en 3840×2160, sse 64)
- imagerie Bing (défaut), `--width 960`, 16 échantillons, un processus par GPU
- image `tuile/blender-globe:5.1-su@sha256:3abc6f19…`, base
  `blender-shared-usd:5.2-2605@sha256:f1c512fb…`
- les deux avec `--env CYCLES_BACKGROUND=1`, puis :
  - **A** `--env TUILE_WAIT_MODE=poll` — 36 tours de 50 ms, `Render Time 2.57 s`
  - **B** `--env TUILE_WAIT_MODE=command` — **un** tour,
    `WAIT[1] returned from wait, converged 2.487s`, `Render Time 2.51 s`

Sans `CYCLES_BACKGROUND=1`, B bloque à jamais : hdCycles écrit
`params.background = false` en dur, `run_wait_for_work` gare alors le fil de
rendu dans `pause_cond_.wait()` en le laissant à l'état `SESSION_THREAD_RENDER`,
et `Session::wait()` attend une transition qui n'arrive pas — zéro CPU, zéro
GPU, mesuré sept minutes durant sur le même pack. `Done:` reste faux dans les
deux modes (`-2147483648%` puis `0%`) : c'est `renderer_percent_done()`, non
bloquant, à regarder à part.


## 577d-native-tone-off.mp4 / 577d-native-tone-on.mp4 — the same film, imagery as stored and with one gain a level (2026-10-06)

Rendered natively by `tuile-film-render` (branch `feat/render-reads-the-store`, commit `aa70aa0` plus the NVENC sink in progress), from the packs of run `1557732/20260924T221734Z-577d` (10 packs of references, frames 1–8094, scene `72212abb4518d1b0`) and the tile store (layers `asset-1` terrain, `asset-2` imagery). 1920×1080, supersample 2, 30 fps, every frame, H.264 by VideoToolbox at 12 Mb/s.

- `tone-off`: `--no-tone`.
- `tone-on`: `--tone-table tone.json`, the table the first render's own light meter solved (anchor level 10; levels 13–19 at +1.57 / +1.53 / +0.96 stops R G B, levels 1–12 at 0).

To make them again (worktree `tuile-store-render`, buckets in the environment):

```bash
tuile-film-render 1557732/20260924T221734Z-577d/packs --no-tone --out 577d-native-tone-off.mp4 --meter off
tuile-film-render 1557732/20260924T221734Z-577d/packs --tone-table off/tone.json --out 577d-native-tone-on.mp4 --meter on
```

Measured (stops): top third − bottom third, worst frame 0.72 → 0.30, frames beyond 0.3: 266 → 0; mean luminance range over the film 0.38 → 0.24. The meter's reports are in the worktree under `local-packs/probe/577d-film/{off,on}/`.

## 577d-decoded-grade-off.mp4 / 577d-decoded-grade-on.mp4 — imagery decoded from sRGB, without and with a grade per level (2026-10-06)

Same run and settings as the `577d-native-tone-*` pair above (packs of `1557732/20260924T221734Z-577d`, frames 1–8094, 1920×1080, supersample 2, 30 fps, H.264 by VideoToolbox, 12 Mb/s), rendered by `tuile-film-render` at commit `cfd8d66` of `feat/render-reads-the-store`. Two things differ from that pair: the default look now decodes imagery from sRGB (exposure 0.8 stops), and the correction is a whole grade a level — black point, gain, contrast, saturation — not a gain.

- `grade-off`: `--no-tone --anchor 10 --meter off` (the meter fits the grade from this render's own tiles).
- `grade-on`: `--tone-table off/tone.json`. Levels 1–12 untouched; levels 13–19: black +0.005 +0.005 +0.013, gain +1.70 +1.51 +1.92 stops, contrast 0.88 about 0.18, saturation 1.77.

The same table is in the tile store as `asset-2/tone.json` (version 2). Measured (stops): top third − bottom third, worst frame 1.55 → 0.61; mean luminance range over the film 0.95 → 0.52; largest step between consecutive frames 0.17 → 0.06. Reports under `local-packs/probe/577d-grade/{off,on}/` in the worktree.

## 8743-native.mp4 — a coast, re-rendered after the grade burnt it out (2026-10-07)

Rendered natively by `tuile-film-render` at commit `adcee1c` of `feat/render-reads-the-store`, from the pack `packs/8743dc56923d2b3b/1-2880.tuilepack` (references, frames 1–2880, scene of the pack's own digest) and the tile store (terrain `asset-1`, imagery `asset-2`). 1920×1440, supersample 2, 30 fps, every frame, H.264 by VideoToolbox at 12 Mb/s. Default look: imagery decoded from sRGB, exposure 1.0, contrast none, highlights rolled off above 0.5.

Grade: none of the film's 28 places has a table of its own, so each takes the layer's gains alone (levels 13–19 at +1.70 / +1.51 / +1.92 stops), no contrast or saturation.

```bash
tuile-film-render packs/8743dc56923d2b3b --out 8743-native.mp4 --meter film
```

Sampled frames measured L* 45, contrast 56, burnt 1.2 % (before: 44, 83, 6.5 %). What it does not fix: patches of different tone within imagery level 13, which are the source's.

## 8743-film-grade.mp4 — the film's own grade, no tile touched (2026-10-07)

`packs/8743dc56923d2b3b`, frames 1–2880, 1920×1440, supersample 2, 30 fps, H.264 by VideoToolbox, 12 Mb/s, rendered by `tuile-film-render` at commit `60e4037` of `feat/render-reads-the-store` with the grade then kept beside the pack: exposure +1.5 stops and saturation ×1.3 for the whole film, no gain on any tile. Sampled frames: L* 46, b* +1.3, C* 17, contrast 52, nothing burnt. The captures of imagery level 13 still show as plates.

## 8743-tile-gains-trial.mp4 — a trial of a gain a tile, not a result (2026-10-07)

Same film and settings, with a gain a tile of imagery from a prototype kept in the worktree (`local-packs/probe/8743/tiles/proto/`, `field.py` at μ = 3): every level-13 tile measured against the same ground in level 12, an edge-preserving fit over those offsets, the result applied as it is. Grade file: `local-packs/probe/8743/blocks/field-mu3.tone.json`, passed by `--tone-table`. Film part as above.

It brings the plates together, and it **does not hold the rule that tiles in accord stay in accord**: of 2706 edges where the tiles themselves show under a tenth of a stop, 156 are given a step of more than 0.05 stop and 75 of more than 0.15. Kept to judge the direction by, not to ship.

## 8743-sentinel-field.mp4 — tiles brought together against a Sentinel-2 reference (2026-10-07)

`packs/8743dc56923d2b3b`, frames 1–2880, 1920×1440, supersample 2, 30 fps, H.264 by VideoToolbox, 12 Mb/s, rendered natively by `tuile-film-render` at commit `b340564` of `feat/render-reads-the-store`, with a grade fitted against the store's layer `asset-3954` (Sentinel-2 cloudless, brought in for this film's footprint by `tuile-bake --reference`).

```bash
tuile-film-render packs/8743dc56923d2b3b --calibrate <dir> --every 24 --scale 0.5 \
    --reference-layer asset-3954 --measure linear
tuile-film-render packs/8743dc56923d2b3b \
    --tone-table <dir>/packs/8743dc56923d2b3b/1-2880.tuilepack.tone.json --out 8743-sentinel-field.mp4
```

The grade: every level-13 tile measured by the line that lays it on the reference (16×16 co-located places, orthogonal regression, one weight a place), a continuous field over the tiles' corners, jumps only at seams read against the reference past the two places a mosaic blends over (286 seam edges of 9170). Water was not yet left out of the measure in this render (it is from the next commit on).

What it does: the large grey-blue plates join their green neighbours. What it does not: a capture boundary that runs through the middle of a tile is smoothed, not removed (lower half of the boundary at columns 4073|4074); patches in the sea and steps along the shore; the film is brought to its own median tone against the reference, not to the reference — it stays lighter and less saturated than Sentinel-2. To compare with `8743-film-grade.mp4` (same film, no tile touched).

## 8743-sentinel-zones.mp4 — brought to the Sentinel-2 reference, sea and land each as itself (2026-10-07)

Same film and settings as `8743-sentinel-field.mp4`, rendered at commit `23ab0c1` with the grade of `--calibrate … --reference-layer asset-3954 --measure linear` (`--toward 1`, the default: the whole of the reference's look). A tile is measured on the zone it mostly is — water against water, ground against ground — each zone is given its own standing against the reference back, and a shore between a tile of water and a tile of ground is a seam.

Judged too green and too close to the reference's look, where the film is to keep the look of its own imagery. And the sea near a shore comes out green: a tile that is ground for the most part gives its strip of water the ground's correction — a zone is a tile's here, not a texel's.

## 4dbd-matrix-30.mp4 — a function a tile, fitted against a Sentinel-2 reference, three tenths of its look (2026-10-07)

Run `1557732/20260924T181803Z-4dbd`, its ten packs of references, frames 1–8094, 1920×1080, supersample 2, 30 fps, H.264 by VideoToolbox, 12 Mb/s, rendered natively by `tuile-film-render` at commit `0354035` of `main` in 334 s. Imagery levels 1 to 19.

```bash
# the reference under the film's packs, once (reads the reference's source, writes the store's layer)
tuile-bake --reference <pack>            # each of the ten
tuile-film-render 1557732/20260924T181803Z-4dbd/packs --calibrate <dir> --every 24 --scale 0.5 \
    --reference-layer asset-3954 --measure matrix
tuile-film-render 1557732/20260924T181803Z-4dbd/packs \
    --tone-table <dir>/1557732/20260924T181803Z-4dbd/packs/c0000.tuilepack.tone.json --out 4dbd-matrix-30.mp4
```

The grade: a colour matrix at each corner of a tile, blended across it and applied in the composition shader; fitted for the whole film at once on a lattice no finer than level 14 — a finer tile takes the function of the level-14 tile it lies in — against the reference given the film's own look, with three tenths of the reference's left in (`--toward`, `TUILE_REFERENCE_DOSE`). What a channel takes of the other two, and what is added, are held far more than its own gain.

Two earlier fits of this film were thrown away before this one, and say why it is as it is: fitted a tile at a time, the function smeared each field of a level-16 tile onto the reference's; fitted on the lattice with every term free, the film came out flat. The same grade is kept beside the film's packs, and the page of the demonstration draws it.

## pyrenees-coast-48f-through-tile-server.mp4 — a bake that asked no source itself (2026-10-08)

The first film whose bake held no access token: every tile the shared store lacked came through a host's tile server, handed to the globe as `Sources` (`GlobeConfig::sources`), and the server alone held the sessions.

- tape: `pyrenees-tape film.mcap 1 24 20000` (1 440 frames, one minute around the Pyrenees at 20 km, tilted 20°); frames 1–48, from the Basque coast inland
- bake: 640×480, `--sse 16`, embedded pack, scene `adddb0e5a320b454`, 15 distinct tiles over 537 selections, 17.7 MB; through the host's `Sources`, tile store shared with the server under a test prefix (`TUILE_TILES_PREFIX`). The two frames baked before it had asked the server for 13 969 imagery tiles and 2 744 terrain tiles; these 48 asked for 15 and 2 more — the rest was in the store.
- render: `tuile-film-render <prefix>/packs --out film.mp4 --no-tone --fps 24`, natively, 48 frames in 10.6 s; 15 tiles from the pack, none from the store; H.264 by VideoToolbox, 12 Mb/s

What it is and is not: two seconds, small, and soft — the screen-space error of 16 was chosen to bound what the trial asked of the imagery source, not for the picture. No grade. Looked at: ground everywhere, the coast at frame 1, a river valley at frame 48, no black.

To make it again one needs an implementation of `Sources` and its server, which this repository does not hold; with the built-in sources the same tape, frames and settings give the same selection (`tuile-bake --diff`), see #5.
