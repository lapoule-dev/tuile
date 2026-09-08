# Le look « terrain » par composition — ce qui marche, ce qui attend

Mesuré le 2026-09-08 sur la chaîne locale (usdrecord/Storm 26.08 + plugin
TuileGlobe, manifeste d'orbite Ariège), trois rendus comparés (headlamp nu /
aube stylisée / jour neutre). Le principe tient : **le procédural émet la
géométrie, tout le look se compose par-dessus** — `look.usda` sublaye le
manifeste et rien d'autre.

## Marche sur Storm aujourd'hui (prouvé image à l'appui)

- **DomeLight + HDRI équirect** (`inputs:texture:file`, un `.exr` — un dome
  sans texture est INERTE sur Storm, warning mesuré). Panorama CC0
  Poly Haven ; le « dawn » stylisé teinte tout en mauve (jugé moche), le
  « partly cloudy puresky » neutre remplit les ombres sans les salir.
- **DistantLight orienté** : l'élévation est LE levier de relief — 18° sculpte
  fort mais assombrit des vallées entières, 35° est le compromis jour.
  Rendre avec `--disableCameraLight`, sinon le headlamp écrase tout.
- **Over caméra à travers le sublayer** : `focalLength` doublé sans toucher
  aux timeSamples du transform — la compression de perspective fait « photo
  aérienne ». La sélection suit (view.cpp lit aperture/focal).
- **Orientation** : la scène est rebasée mais PAS réorientée — le haut local
  est le zénith ECEF (`renderOrigin` normalisé), pas +Z monde. Dome et soleil
  portent une matrice qui aligne leur +Z dessus ; un DistantLight par défaut
  éclaire un pôle (mesuré : image noire). Les matrices de `look.usda` valent
  pour le manifeste d'orbite ; autre manifeste ⇒ recalcul depuis son
  renderOrigin.

## Altérer la TEXTURE elle-même (bruit, grain, mordant)

La demande précise : casser le lisse satellite sur la texture, pas la
lumière. Deux étages :

1. **Aujourd'hui, en post 2D** (aucun code moteur) : sur la frame rendue,
   `unsharp` (7:7:1.1) rend leur mordant aux mosaïques bakées (sorties
   douces), `eq` (contraste 1.07, saturation 1.1) densifie, `noise`
   (alls=7, temporel) pose un grain fin qui vit d'une frame à l'autre.
   Prouvé sur image (`textured.1.png` vs `neutral.1.png`) ; sur la ferme,
   c'est un filtre ffmpeg au moment de l'encode, ou le compositor Blender
   (grade + grain + léger glare) sur le chemin HYDRA_STORM.
2. **La vraie couche matériau** (détail-noise/bump multiplié sur la diffuse,
   roughness variée) : impossible par composition pure aujourd'hui — chaque
   tuile générée émet SON binding vers SON matériau (plus spécifique, il
   gagne sur tout binding composé), et les primvars arbitraires ne sont pas
   dans le jeu du flattening (qui n'hérite que xform/visibility/purpose/
   materialBindings). Le bon crochet : des primvars de config sur le prim
   `Globe` (`tuile:roughness`, `tuile:detailNoise`) que le procédural
   transmet à ses réseaux de matériaux — petit ajout à `procedural.cpp`,
   recommandé comme prochaine marche.

## N'existe pas sur Storm — attendra hdCycles (ou Blender)

- **Brouillard / atmosphère** : pas de fog Storm, pas de PhysicalSky, UsdVol
  inexploitable ici. C'est l'aérien-perspective qui manque le plus au
  lointain ; hdCycles (volume scatter) ou le compositor Blender le donneront.
- **Depth of field** : Storm l'ignore en batch.
- **Tonemapping** : usdrecord n'offre que `--colorCorrectionMode sRGB` ; le
  grade fin relève du post ci-dessus.

## Côté Blender (chemin ferme), à brancher quand l'image globe est prête

- **World Nishita sky** (soleil physique, élévation/turbidité) remplace le
  DomeLight HDRI — même effet, paramétrable par-job.
- **Compositor en batch** sur le rendu HYDRA_STORM : grade + grain + glare ;
  le mist pass demande une profondeur que Storm ne sort pas — fog en grade
  2D d'abord.
