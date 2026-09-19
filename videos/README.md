<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Les rendus qu'on garde

Gitignoré : sept cents mégaoctets par film, et ils sont reproductibles — le
pack vit dans R2, la trajectoire dans `crates/tuile-tape/src/bin/`. Ce
répertoire existe pour qu'un film survive à un nettoyage de `/tmp`, pas pour
faire office d'archive.

Chaque entrée dit **de quoi refaire le film**, parce qu'un mp4 sans ses
paramètres ne se compare à rien.

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

## `orbite-8frames-1920.mp4`

8 frames, 1920×1440, 1,5 Mo. Pas un film : la **première image** où toute la
chaîne a tenu — manifeste entré par le pont Blender, procédural cuisant pour
`/freeCamera`, textures lues dans le tuilepack. Gardée comme témoin.

- pack `packs/62fed640f2d6a36a/1-24.tuilepack`, orbite à 8 km
- c'est le run qui a mesuré la cuisson incrémentale : `cook #1 built=444`,
  puis `kept=444 built=0`, contre 444 reconstruites à chaque frame avant.
