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
cuisson, il a régénéré un vol penché de 20° — `tuile-tape pyrenees` a changé depuis
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

- tape: `tuile-tape pyrenees film.mcap 1 24 20000` (1 440 frames, one minute around the Pyrenees at 20 km, tilted 20°); frames 1–48, from the Basque coast inland
- bake: 640×480, `--sse 16`, embedded pack, scene `adddb0e5a320b454`, 15 distinct tiles over 537 selections, 17.7 MB; through the host's `Sources`, tile store shared with the server under a test prefix (`TUILE_TILES_PREFIX`). The two frames baked before it had asked the server for 13 969 imagery tiles and 2 744 terrain tiles; these 48 asked for 15 and 2 more — the rest was in the store.
- render: `tuile-film-render <prefix>/packs --out film.mp4 --codec h264 --no-tone --fps 24` (built with `--features videotoolbox`), natively, 48 frames in 1.6 s; 15 tiles from the pack, none from the store; H.264 by VideoToolbox, 12 Mb/s. A first render without that feature came out as AV1 by rav1e (10.6 s), which the Mac's own player does not show: the file kept is the H.264 one.

What it is and is not: two seconds, small, and soft — the screen-space error of 16 was chosen to bound what the trial asked of the imagery source, not for the picture. No grade. Looked at: ground everywhere, the coast at frame 1, a river valley at frame 48, no black.

To make it again one needs an implementation of `Sources` and its server, which this repository does not hold; with the built-in sources the same tape, frames and settings give the same selection (`tuile-bake --diff`), see #5.

## washington-orbit-48f-720p-sse2-through-tile-server.mp4 — a precise bake through a host's sources (2026-10-08)

An orbit of Washington, baked cold with no access token: terrain and imagery both through a host's tile server (`GlobeConfig::sources`), the tile store shared with it under a test prefix.

- tape: `tuile-tape orbit orbit.mcap 48 -77.0353 38.8895 3000 2000` — 48 frames around the Washington Monument, 3 km out, 2 km up
- bake: 1280×720, `--sse 2`, embedded pack, scene `0cbb0a1859ace542`, 1.57 GB, 48 frames in 117 s; peak 1.5 GB of memory. It asked the server for 9 468 imagery and 4 739 terrain tiles; the ground had been baked once before at 640×480 and `--sse 16` (18 235 and 4 442 asked, 79 s), so the coarse levels were already in the store.
- render: `tuile-film-render <prefix>/packs-precise --out washington.mp4 --codec h264 --no-tone --fps 24` (built with `--features videotoolbox`), natively on a laptop, 48 frames in 83 s; 10 799 tiles from the pack, none from the store; H.264 by VideoToolbox, 12 Mb/s

Looked at, frames 1 and 24: the Mall, the Potomac and the street grid sharp to the horizon, ground everywhere, no black. No grade. Two seconds.

As for the entry above, making it again needs an implementation of `Sources` and its server, which this repository does not hold.

## 577d-f4500-projected-ribbon-8m-penumbra-x{0,1,2,4,8,20,40}.png, …-grid.png, …-grid-left-2x.png, …-before-after-x4.png — a projected shadow's penumbra (2026-10-18)

Frame 4500 of run `1557732/20260924T221734Z-577d` (packs `…/packs`, bucket `stl-renders`; tile store `stl-tiles`; keys from `.env.film`), 1920×1080, supersample 2, no grade, rendered natively on Metal (Apple M2) by `tuile-film-render` (release) on branch `feat/projected-penumbra` (worktree `tuile-penumbra`). The demonstration ribbon of the `577d-f4500-overlay-ribbon-*` pictures (same 109 points), **8 m wide**, its shadow **projected** (`--ribbon-shadow project`), sun at azimuth 250°, 20° up, shadows full:

```bash
TUILE_STORE_BUCKET=stl-renders TUILE_TILES_BUCKET=stl-tiles \
tuile-film-render 1557732/20260924T221734Z-577d/packs --frames 4500:4500 \
  --sun 250,20 --shadow 1 --ribbon-width 8 --ribbon-shadow project \
  --penumbra-scale <k> --ribbon "<the 109 points above>" --pictures <dir>
```

`x0` is the hard shadow as before the penumbra; `x1` the sun's own disc (the default), `x2` … `x40` the disc cut that many times wider. The grids are crops of the same frames: the forest east of the plateau (880,120)–(1680,620), and the ribbon's left turn (180,40)–(580,340) at twice the size.

Looked at: at x1 the edge is already softer than x0, the band still dark; at x2 it reads as a soft, diffuse band and holds its darkness; at x4 it is faint, at x8 and past it barely there — an 8 m ribbon a few hundred metres up under a disc 20 m and more across hides a fifth of the sun or less. The shadow's upper end, cast from higher, fades first. `…-before-after-x4.png` sets the hard shadow, x1 and x4 side by side (the left turn and the forest, twice the size): x4 is the value chosen for a track ribbon; on this 8 m ribbon a few hundred metres up it is a light, diffuse veil, darker where the ribbon runs lower. No black, no gap, nothing flickers (one frame; the GPU tests pin the stillness).

## stars-twilight-pic-du-midi-2026-06-21/, …-up20/ — the stars come out through twilight (2026-10-25)

Two viewer series from the Pic du Midi, the stars drawn from the Bright Star Catalogue, always in the sky and dimmed only by the air (branch `feat/stars-attenuated`, PR stacked on #137):
- Each holds the frames `000.png`–`012.png` and `noon/000.png`.
- `sheet.png` is the twilight frames at their own size, so that a star stays a pixel.
- `sheet-noon-and-twilight.png` is the noon frame, then the twilight frames, labelled.

The settings:
- Place: lon 0.1418, lat 42.9365, **altitude 3100** (ellipsoid height: 2900 is under the summit), heading 110 (east, towards the darkening sky).
- Date: 2026-06-21. The sunset is 19:41:54 UTC (the ephemeris, sea-level horizon). Frames run every 10 minutes from sunset to +2 h; the noon frame is at 12:00 UTC.
- Viewport: 960×540, the viewer's own frame (its metering, exposure, white balance and roll-off). Real terrain and imagery come through the ion catalogue proxy (`TUILE_ION_TOKEN` from `.env`).
- `…-2026-06-21/` is pitch 10 as asked. In this viewer that is 10° **below** the horizon (`View::pitch`, "degrees below the horizon"), so the sky is the top fifth of the frame.
- `…-up20/` is pitch −20 (20° above), all sky, and shows the stars coming out.

```bash
TUILE_ION_TOKEN=… cargo run --release -p tuile-wgpu-viewer --example sunset -- \
  --lon 0.1418 --lat 42.9365 --altitude 3100 --heading 110 --pitch 10 \
  --date 2026-06-21 --from 0m --to 2h --every 10m --sheet-scale 1 \
  --out videos/stars-twilight-pic-du-midi-2026-06-21
# the noon frame: --from -7h41m54s --to -7h41m54s --out …/noon
# the upward series: the same with --pitch -20 --out videos/stars-twilight-pic-du-midi-2026-06-21-up20
```

Looked at:
- **Noon.** A blue sky with no star in it: the stars are in its radiance at a few millionths of it, and no 8-bit picture shows them.
- **The upward series.** The sky goes from orange to deep blue. Point sources (local maxima 3 code values over their ring, the controls left out) number 0, 0, 0, 0, 0, 1, 34, 62, 157, 426, 427, 420 and 384 from sunset to +2 h. The first star shows at +50 min, a few dozen at +60, the naked-eye sky by +90.
- Near the horizon the stars are dimmer and redder.
- The pitch-10 series shows the terrain under a strip of sky, with the stars over the ridge line by +90 min.
- No black.

The viewer meters its own frame, sky included, so it shows a given star later than the test probes do. The probes are metered on level ground (`crates/tuile-atmosphere/tests/stars_seen.rs`) and show Sirius from +20 min. The noon frame's nearest ground is still the coarse fill: the run settles in a fraction of a second, and its terrain was not all there.

## planets-twilight-pic-du-midi-2026-06-21-west/ — Venus first, then Jupiter (2026-10-25)

The Pic du Midi's evening, looking west, with the planets drawn by the stars' rule (branch `feat/planets-attenuated`, stacked on #147). The place and clock are those of `stars-twilight-pic-du-midi-2026-06-21/`: lon 0.1418, lat 42.9365, altitude 3100 (ellipsoid), sunset 19:41:54 UTC, every 10 min to +2 h, plus noon. The view is heading 290 and pitch −5 (5° above the horizon), 960×540, through the viewer's own frame.

```bash
TUILE_ION_TOKEN=… cargo run --release -p tuile-wgpu-viewer --example sunset -- \
  --lon 0.1418 --lat 42.9365 --altitude 3100 --heading 290 --pitch -5 \
  --date 2026-06-21 --from 0m --to 2h --every 10m --sheet-scale 1 \
  --out videos/planets-twilight-pic-du-midi-2026-06-21-west
# noon: --from -7h41m54s --to -7h41m54s --out …/noon
```

Looked at (`sheet-noon-and-twilight.png`):
- **Noon.** No point of light.
- **The sunset frame.** The sun's disc on the horizon.
- **Venus** is the first point in the sky, at +40 min (sun −6.6°), at the top of the frame. It sets towards the west-north-west frame by frame, white with a warm cast, and saturates by +80.
- **Jupiter**, reddened low in the glow, shows from +70–80 min.
- **The first stars** come after Venus.
- No black.

## record-flicker/ — the first recorded interactive sessions, pictures included (2026-10-26)

Recorded by the windowed viewer with `--record` (branch `feat/record-interactive`, #156). This was before the flicker fix (#149), so the defect is in them. Each `.mcap` holds, frame by frame:
- `/camera`, the exact pose;
- `/viewer/frame`: wall instant, scene instant, rate, offset, viewport, scale and render origin;
- `/viewer/sky`;
- `/viewer/input`;
- `/frame`, the picture drawn (JPEG, half the frame's size).

The `.mp4`, `.parquet` and `-plots.png` are `tuile-record export` of them. `*-diagnose.txt` is `tuile-record diagnose`. Viewport 1600×1200 points at scale 2, 30 pictures a second, JPEG q90. The machine's load average was 110–200 throughout (other agents' builds), so the viewer ran at 37–54 fps, and some pictures were dropped. Dropped pictures are counted, and the frame indices stay exact.

```bash
export CESIUM_ION_TOKEN=…   # or ~/Library/Application Support/Tuile/token
# zoom-out: Pic du Midi, clock held, 3 km → 100 000 km, then a pan (zoom-out.steer)
TUILE_STEER_SCRIPT=videos/record-flicker/zoom-out.steer tuile-wgpu-viewer \
  --lon 0.1418 --lat 42.9365 --altitude 3000 --heading 290 --pitch 10 \
  --time 2026-10-25T17:35:00Z --time-rate 0 --record zoom-out.mcap
# sunset: 3.1 km, the clock at 120× (sunset.steer). Laurent pressed R during it: 600×, then 3600× from frame 1001
TUILE_STEER_SCRIPT=videos/record-flicker/sunset.steer tuile-wgpu-viewer \
  --lon 0.1418 --lat 42.9365 --altitude 3100 --heading 250 --pitch 5 \
  --time 2026-10-25T16:40:00Z --time-rate 120 --record sunset.mcap
tuile-record export zoom-out.mcap --mp4 zoom-out.mp4 --parquet zoom-out.parquet --plots zoom-out-plots.png --burn-in
tuile-record diagnose zoom-out.parquet
# replay of frames 860–889 through the viewer's renderer, headless, twice
TUILE_ION_TOKEN=… cargo run --release -p tuile-wgpu-viewer --example replay -- \
  zoom-out.mcap --out replay-1.mcap --seek 860 --frames 30 --bound 20
```

- `zoom-out.mcap`: 2979 frames, 1243 pictures, 37 MB (40 MB/min). Recording cost 0.04 ms a frame on the render thread.
- `sunset.mcap`: 2378 frames, 923 pictures, 83 MB.
- `laurent-session.mcap`: 8613 frames, 2007 pictures, 246 MB. It is the same command line as zoom-out, but driven by hand: wheel and drags to about 990 000 km and back. On that build the script did not run on a desktop (fixed since). At 60 pictures a second it dropped 5807 pictures under the load.

Looked at (`zoom-out.mp4`, `sunset.mp4`, `replay-1.mp4`, and the pictures at frames 877 and 879 via `tuile-record picture`):
- **The flicker, caught on frame 879 of zoom-out** (793 km, clock held, camera climbing 0.6 % a frame).
  - The ground's mean luma jumps from 0.65 to 0.96, and the limb goes from pink to white.
  - In the same frame `light_updates` steps from 56 to 57, and the pre-exposed illuminance from 9.7 to 85.6 (×8.8).
  - That is one light recompute: the exposure stepping. It is the defect #149 removes.
- Star counts drop at 378 km (30 → 12) and 488 km (36 → 17).
- The headless replay of frames 860–889 differs from the live pictures by 0.7–2.3 / 255 (JPEG included), and shows the same step (frame 876 in the replay).
- **Two replays are identical from frame 876 on, and not before.** The sky on main carries a history (when the light was last worked out), which a replay that seeks into the session does not share. Its first light recompute brings both runs together. With #149, which makes each frame's light from that frame alone, the replay should be deterministic from the first frame.
- No black ground in the scripted sessions. In Laurent's session, 15–28 pixels of the ground's clear colour come back frame after frame between 8 000 and 10 700 km, looking straight down at 20° N 5° E.

## stars-scan-pic-du-midi-2026-06-21/ and stars-metering-west-vs-east-pic-du-midi-2026-06-21.png — why the viewer shows stars in some directions only (2026-10-26)

The viewer's own frame, headless, at 2880×1620 and scale 2 (Laurent's Retina window), from the Pic du Midi (lon 0.1418, lat 42.9365, altitude 3100, ground 2877), pitch 0. The scan covers every 30° of heading at seven instants on 2026-06-21: +15, +30, +45, +60, +90 and +120 min after sunset (19:41:54 UTC), and 23:30 UTC.

- `scan.mcap` holds, per view: the camera's pose (`/camera`), the scene's clock (`/clock`), the frame (`/frame`, `foxglove.CompressedImage`) and the count with the view asked for (`/stars`).
- `scan.parquet` holds one row a view: instant, place, heading, pitch, `metered_exposure_ev` (the exposure the viewer's sky meters, its own rays and calibration) and `points` (point sources 4 code values over their ring, above the horizon, controls left out).
- `scan.png` plots the two columns of the Parquet against heading.
- `stars-metering-west-vs-east-…png` shows four crops: +60 and +90 min, looking east (heading 90) and west (heading 270).

```bash
TUILE_ION_TOKEN=… cargo run --release -p tuile-wgpu-viewer --example stars_scan -- \
  --at 2026-06-21T19:56:54Z,2026-06-21T20:11:54Z,2026-06-21T20:26:54Z,2026-06-21T20:41:54Z,2026-06-21T21:11:54Z,2026-06-21T21:41:54Z,2026-06-21T23:30:00Z \
  --step 30 --pitches 0 --places 0.1418:42.9365:3100:2877 --rows 0.43 \
  --width 2880 --height 1620 --scale 2 --out videos/stars-scan-pic-du-midi-2026-06-21
```

Looked at:
- **Night (+120 min and 23:30).** Stars in every direction: 105 to 336 a view. None of the 84 views is empty at night. The same holds at the other places and heights scanned: the plain, Paris, 20 km, 300 km.
- **+60 min.** The meter exposes 16.0 stops looking east and 11.1 looking west-north-west (300–330°), where the afterglow is. The view shows 86 stars east and 5–11 west.
- **+90 min.** The gap is 19.5 against 15.5 stops, and the west still shows fewer stars.
- **+15 to +45 min.** At most 3 stars in any direction under this meter.

The stars' path is the same in every direction. What moves with the heading is the exposure the frame meter picks, and that is the camera's response.

## discs-s0-2026-10-25/ — the sun's and the Moon's discs as they are drawn today (S0, 2026-10-25)

The baseline of the discs' work (`docs/explorations/sun-and-moon.md`, PR S0,
branch `feat/sun-moon-s0`): the four acceptance scenes of
`tuile_atmosphere::scenes`, unchanged pixels, in the film and in the viewer.
`film/<scene>/frame-NNNNN.png` (960×540, supersample 2, the film's stages:
tape → `tuile-film-produce` → `tuile-film-render --atmosphere --discs`, with
`discs.parquet` beside), `viewer/<scene>/NNN.png` (960×540, the viewer headless;
a 10° lens drawn at 35° and 3460×1946 and its middle cut out),
`sheets/<scene>.png` side by side (before = after here: S0 moves no pixel).

- `pic-du-midi-sunset`: 0.1418°E 42.9365°N, 2 900 m, heading 290°, pitch −3°,
  35°, 2026-06-21T19:41:54Z (the ephemeris's sunset). Every scene stood at
  2 900 m over the ellipsoid for these renders, some 25 m under the summit
  (2 877 m over the sea, ≈ 2 926 m over the ellipsoid): the film's frames
  show the summit's nearest faces as walls of stretched imagery at the
  lower right (the hole probe counts 0 pixels; at 3 100 m they are gone).
  The scenes now stand at 3 100 m; renders from then on are made there.
- `full-moon-rise`: same place, heading 63.89° (the Moon's azimuth), pitch −2°,
  10°, 2026-10-26T16:53:46Z (its centre on the eye's horizontal).
- `crescent-at-dusk`: heading 225.92°, pitch −0.37°, 10°, 2026-11-11T17:10:12Z
  (civil dusk, the Moon 3.1° up, 5.5 % lit; on the 10th, 1.5 days after the
  new Moon of 2026-11-09T06:52:38Z, it is under the horizon at civil dusk).
- `sun-behind-ridge`: heading 235.63°, pitch −1°, 10°, 2026-12-15 from
  16:10:22Z, 16 frames a minute apart (the sun's centre from 1.5° up to under
  the Vignemale's ridge).

Looked at: imaged ground in every frame, no black. The disc instrument reads
today's defects: the sunset's sun at 0.693 of its light, the setting sun
behind the ridge at 0.80 (the unnormalised limb law) falling to 0 over four
frames, the full Moon at 0.964, the crescent at 0.514. The film's glare
switches off in one frame as the sun's centre crosses the ridge (frames 12 →
13). In the viewer the sun is a faint clipped dot with no glare.

```bash
cargo build --release -p tuile-film-native --features videotoolbox \
  --bin tuile-film-render --bin tuile-film-produce --bin tuile-film-scene
cargo build --release -p tuile-wgpu-viewer --example discs
TUILE_ENV_FILE=~/Projects/tuile/.env scripts/discs-scenes.sh videos/discs-s0-2026-10-25
scripts/discs-sheet.py videos/discs-s0-2026-10-25 videos/discs-s0-2026-10-25 videos/discs-s0-2026-10-25/sheets
```

## discs-s1-2026-10-25/ — the discs carry their light (S1, 2026-10-25)

PR S1 (branch `feat/sun-moon-s1`) against the S0 baseline above, the same
four scenes, the same commands (`scripts/discs-scenes.sh`, then
`scripts/discs-sheet.py ../discs-s0-2026-10-25 . sheets`). `sheets/<scene>.png`:
viewer before | viewer after | film before | film after, the disc
instrument's ratio under the film's; `sheets/zoom-sun-viewer.png`: the
viewer's sunset disc, 8× (before | after).

- Disc instrument, film (light measured over expected): sunset sun 0.693 →
  0.998; full-Moon rise 0.964 → 1.017; crescent 0.514 → 1.011; the sun before
  the ridge 0.80 → 1.000, then over the ridge 0.899, 0.636, 0.274, 0.000
  (before: 0.681, 0.423, 0.151, 0.000).
- Looked at: the viewer's setting sun was a faint dot the colour of the sky;
  it is now a disc, yellow above and orange at its lower limb, cut by the
  horizon with no second dimming; the film's sunset brighter in its glare;
  the Moon's edge no longer soft. No black anywhere. The viewer's first
  crescent render came out on placeholder ground (the tiles had not
  arrived; the run's settle said settled): rendered again, the one in the
  sheet.
- Known, not changed here: the sun's core clips yellow-white in both
  renderers (the tone of highlights, Laurent's decision); the viewer has no
  glare (S4); the film's glare still switches off when the sun's centre
  crosses the ridge (S4).

```bash
TUILE_ENV_FILE=~/Projects/tuile/.env scripts/discs-scenes.sh videos/discs-s1-2026-10-25
scripts/discs-sheet.py videos/discs-s0-2026-10-25 videos/discs-s1-2026-10-25 videos/discs-s1-2026-10-25/sheets
```

## ion-photoreal-2026-10-31/ — 3D Tiles over the globe, through ion (2026-10-31)

Branch `feat/viewer-ion-photorealistic`. The viewer's own frame, headless on
Metal (M2), with ion asset 2275207 — a 3D Tiles tileset ion points at on
another service, opened through its endpoint — drawn over the globe as a
surface that replaces the ground. Each directory holds the same view twice:
`globe.png` without the tileset, `tileset.png` with it; each `.log` the scan
of both frames (0 black pixels in all six).

- `eiffel/`: eye 2.3040 E 48.8535 N, 350 m, heading 310, pitch 12.
- `pic-du-midi/`: eye 0.1520 E 42.9300 N, 3150 m, heading 315, pitch 6.
- `paris-overview/`: eye 2.3065 E 48.8505 N, 5000 m, heading 315, pitch 20.
- Lit for 2026-10-31T12:00:00Z, 1280×800 at scale 1, the tileset given 60 s
  after the globe settled (these frames still show the providers' logos
  bottom left; they were removed from the viewer afterwards).
  `window-title.txt` is the native window's title during a run, with the
  credits the drawn tiles name.
- Looked at: the tower, La Défense and the Trocadéro stand in 3D where the
  globe alone has them flat; no seam or black between the mesh and the
  globe; the mesh is lit by the scene's sun over textures that already carry
  their own light, so it reads brighter and flatter than the imagery beside
  it (since fixed: the tiles declare `KHR_materials_unlit`, now honoured —
  see `ion-seams-2026-10-31/`).

```bash
# CESIUM_ION_TOKEN in the environment (never printed)
for shot in "eiffel 2.3040 48.8535 350 310 12" "pic-du-midi 0.1520 42.9300 3150 315 6" \
            "paris-overview 2.3065 48.8505 5000 315 20"; do
  set -- $shot
  cargo run --release -p tuile-wgpu-viewer --example tileset_shot -- --tileset 2275207 \
    --lon $2 --lat $3 --altitude $4 --heading $5 --pitch $6 \
    --at 2026-10-31T12:00:00Z --seconds 60 --out videos/ion-photoreal-2026-10-31/$1
done
```

## ion-photoreal-credits-2026-10-31/ — credits bar, the switch, the web (2026-10-31)

Branch `feat/viewer-ion-photorealistic`, after the providers' logos were
removed, the credits bar (tuile-ui) added, the 3D Tiles made a switch (off by
default), `KHR_materials_unlit` honoured and the camera floor fixed. Same
three views as `ion-photoreal-2026-10-31/`, headless on Metal (M2), 1280×800
at scale 1, lit for 2026-10-31T12:00:00Z, the tileset given 60 s:

- `<view>/globe.png` (no tileset: no bar), `tileset.png` (the bar along the
  bottom, outside the view, the credits centred), then in the same session
  `off-at-once.png` (switched off as the menu does, 0.1 s later),
  `off.png` (settled) and `on.png` (switched on again, 60 s later). Each
  `shot.log` says what the scan found: **0 black pixels in all fifteen**.
- `web-webgpu-eiffel/`, `web-webgl2-eiffel/`: the page in headless Chrome
  through the catalog proxy, the switch clicked in the page's own menu:
  `1-globe` (off by default, no bar), `2-tileset-on`, `3-off-at-once`,
  `4-off`, `5-on-again`; the choice kept in local storage. The eye stays at
  350 m with no second placement (before the camera fix it was lifted to
  ~4.8 km). 0 black pixels in all ten; the page's caption stands above the
  bar while it is shown.
- Looked at: no logo; the bar bottom centre, outside the picture; the
  tileset gone the next frame after the switch, the globe there, no black.

```bash
# The token is the one the viewer reads (CESIUM_ION_TOKEN, or the `token`
# file in its support directory); never printed.
for shot in "eiffel 2.3040 48.8535 350 310 12" "paris-overview 2.3065 48.8505 5000 315 20" \
            "pic-du-midi 0.1520 42.9300 3150 315 6"; do
  set -- $shot
  cargo run --release -p tuile-wgpu-viewer --example tileset_shot -- --tileset 2275207 \
    --lon $2 --lat $3 --altitude $4 --heading $5 --pitch $6 --at 2026-10-31T12:00:00Z \
    --seconds 60 --switch on --out videos/ion-photoreal-credits-2026-10-31/$1
done
# The web: packages/tuile-viewer/build.sh, then the catalog proxy with the
# asset (as scripts/try-viewer.sh web), then (pip: websockets):
python3 videos/ion-photoreal-credits-2026-10-31/web-shoot.py \
  "http://127.0.0.1:8091/examples/web-viewer/?lon=2.3040&lat=48.8535&altitude=350&heading=310&pitch=12&time=2026-10-31T12:00:00Z&timeRate=0" \
  videos/ion-photoreal-credits-2026-10-31/web-webgpu-eiffel webgpu 60   # and ?backend=webgl2&…
```

## ion-seams-2026-10-31/ — seams in the photorealistic 3D Tiles, far over the countryside (2026-10-31)

The diagnosis of the lines seen along tile borders from far away, one
hypothesis at a time. Headless on Metal (M2), 1280×800 at scale 1, lit for
2026-10-31T12:00:00Z, the tileset given 60 s. Views (lon lat altitude
heading pitch): `beauce-5km` 1.49 48.45 5000 0 30, `beauce-15km` … 15000 0
40, `beauce-40km` … 40000 0 60, `bourgogne-15km` 4.85 47.05 15000 0 30,
`pyrenees-15km` 0.15 43.10 15000 180 25, `paris-15km` 2.35 48.85 15000 0 40.

- `before/<view>/{globe,tileset}.png`: as the viewer drew them before the
  fix (lit by per-tile computed normals).
- `diagnostic/<view>/{unlit,coverage,geometry}/tileset.png`: the same code
  in the diagnostic views (albedo only; magenta where no imagery layer —
  the tileset — and green/blue where the globe shows through; flat lambert
  only); `diagnostic/<view>/no-mips/`: lit as before, mipmaps off
  (`TUILE_DIAG_NO_MIPS`, a temporary build, not committed).
- `after/<view>/tileset.png`: with `KHR_materials_unlit` honoured.
- `seamline.py`: the measure — the strongest luminance step between two
  rows in a band, against the band's median. `glbscan.py` /
  `glb-beauce.jsonl`: what the tiles declare, walked through the catalog
  proxy towards the Beauce, levels 2 to 11.

Measured (ratio of the border row's step to the band's median):

| | before | unlit view | geometry view | no mipmaps | after |
|---|---|---|---|---|---|
| bourgogne-15km, rows 725–750 | 1.70 | 1.09 | 4.04 | 1.64 | 1.11 |
| beauce-5km, rows 255–285 | 2.28 | 1.88 | 2.12 (steps of 0.3) | 2.21 | 2.50 |

Over relief the lines were lighting (gone in the albedo, strongest in the
geometry view, not the mipmaps) and went with the fix. Over the flat Beauce
the remaining step is in the albedo itself: neighbouring tiles of different
levels whose photographs differ in colour and sharpness — the data, which
the fix does not change. The coverage view finds 124, 18 and 4 pixels of
885 500 scanned (Bourgogne, Beauce, Pyrenees; the controls and the bar left out) where the globe shows through the
tileset: sub-pixel cracks between tiles of different levels, drawn in the
ground's colour, not visible as lines. No tile failed to decode (warnings
logged: none from the tileset).

```bash
# Before / after: tileset_shot over the six views (globe and tileset)
cargo run --release -p tuile-wgpu-viewer --example tileset_shot -- --tileset 2275207 \
  --lon 4.85 --lat 47.05 --altitude 15000 --heading 0 --pitch 30 \
  --at 2026-10-31T12:00:00Z --seconds 60 --out videos/ion-seams-2026-10-31/after/bourgogne-15km
# A diagnostic view, the tileset alone
… --globe off --diagnostic unlit|coverage|geometry --out …/diagnostic/<view>/<view-name>
python3 videos/ion-seams-2026-10-31/seamline.py 170 690 725 750 <png>…
```

## 3d-tiles-ledger-2026-11-02/ — the globe and a tileset as one scene, switched on, off at once, on again (2026-11-02)

Pictures, not a film: the photorealistic tileset (asset 2275207) over Paris,
700 m up, heading 315°, pitch 20°, with the globe, through the one server,
budget, stream and pump of the "3D Tiles in the ledger" stack (top:
`feat/3d-tiles-viewer`). Five pictures a run: `globe`, the tileset `on`,
`off-at-once` (the frame after the switch: the ground drawn, never black),
`off` (the ground refined again) and `on` again.

- `tip/`: desktop (Metal), the stack's top. GPU bytes printed at each
  picture: globe 368 MiB; tileset 954 MiB (555 MiB of 3D Tiles, 646 drawn)
  of a 4 GiB budget; off 402 MiB (no 3D Tiles held); on again 1019 MiB.
- `web-{base,tip}-{webgpu,webgl2}/`: the page, base = #173's tip (a second
  server per tileset in the worker), tip = the stack; the same five pictures,
  menu switch clicked through the DevTools protocol. Same picture both sides.

```bash
# Desktop
cargo run --release -p tuile-wgpu-viewer --example tileset_shot -- --tileset 2275207 \
  --lon 2.2945 --lat 48.8584 --altitude 700 --heading 315 --pitch 20 \
  --switch on --seconds 40 --out videos/3d-tiles-ledger-2026-11-02/tip
# Page: the web viewer behind the catalog proxy (assets 1,2,3,3954,2275207),
# opened at ?lon=2.2945&lat=48.8584&altitude=700&heading=315&pitch=20[&backend=webgl2],
# the "Photorealistic 3D Tiles" menu entry clicked on, off, on, 40 s between pictures.
```
