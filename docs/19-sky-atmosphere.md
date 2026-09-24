# 19 — Ciel, soleil et atmosphère : un seul modèle pour wgpu, Cycles et la référence couleur

Statut : proposition de conception. Aucun code modifié. S'aligne sur le contrat
de `docs/18-color-harmonization.md` §6 (`Sun`, `AtmosphereModel`, `SkyState`,
`ReferenceIllumination`) et propose des amendements précis (§5.2).

## 1. Objectif et exigences

« Réaliste » veut dire, pour nos plans (vols de parapente rejoués, orbites
pyrénéennes, de 500 m à 50 km d'altitude, caméra vers l'horizon ou vers le sol,
de l'aube au crépuscule) :

1. **Un ciel physique** : bleu saturé au zénith, blanchi à l'horizon, halo de
   Mie autour du soleil, rougissement au coucher, zénith encore bleu au
   crépuscule (ozone), jamais de dôme uni.
2. **La perspective aérienne** : les crêtes lointaines bleuissent et perdent
   du contraste avec la distance *et* avec la densité de l'air traversé ; au
   coucher la brume devient orangée côté soleil. C'est le principal indice de
   profondeur d'un globe.
3. **L'horizon courbe** : à 50 km, l'horizon est abaissé d'environ 7° et à
   ~800 km ; le limbe est une bande fine et lumineuse. Le sol lointain et le
   ciel juste au-dessus doivent se raccorder sans couture.
4. **La lumière directe et l'ambiant viennent du même air** : couleur et
   intensité du soleil transmis, éclairement du ciel. Ce sont exactement les
   grandeurs de la `ReferenceIllumination` du doc 18.
5. **Cohérence wgpu ↔ Cycles** : même soleil, même ciel, même brume, même
   exposition et même courbe de tonalité, à une tolérance mesurée près (ΔE).
6. **Déterminisme** : la lumière est une fonction pure de
   `(instant, lieu, AtmosphereParams, caméra)`, jamais de l'horloge murale ni des
   pixels rendus (pas d'auto-exposition par histogramme).
7. **Budgets** : 60 fps en temps réel (desktop, et le web via la façade) ;
   ~0,47 s/frame pour le rendu OpenUSD/Cycles (64 frames ≤ 30 s). Tout ce qui
   est volumétrique en path tracing est donc exclu par défaut.
8. **Jamais de sol noir** : la nuit a un plancher, l'atmosphère ne peut
   qu'atténuer le sol sans l'éteindre, et toute défaillance retombe sur
   « sans atmosphère », jamais sur « noir ».

## 2. Existant dans tuile, et lacunes

**`crates/tuile-atmosphere`** (dépend de `tuile-core`, `glam`, `bytemuck` ;
propre en wasm) :
- `sun.rs:57` `Sun::at_unix_seconds` : éphéméride basse précision (longitude
  moyenne + deux termes de l'équation du centre, GMST), f64, ~0,01°.
  **Réfraction non modélisée** (`sun.rs:20-23`). `elevation_at` (`sun.rs:121`)
  prend un `up` fourni par l'appelant, et rien n'impose la normale
  *géodésique* : la verticale géocentrique s'en écarte de 0,19° à 43°N, soit
  vingt fois l'erreur de l'éphéméride.
- `aerial.rs` + `aerial.wgsl` : diffusion simple Rayleigh + Mie en **forme
  fermée** (`air_column`, `aerial.rs:216`, stable en f32, bien testée),
  coefficients Rayleigh standard (`:41`), Mie 21e-6 m⁻¹ / H = 1,2 km / g = 0,76
  (`:49-57`, une brume assez forte). Lacunes :
  - **Bug de hauteur, mesuré** : `eye_height = |ecef| − WGS84_A`
    (`aerial.rs:130`, et `ground_height` dans `aerial.wgsl:110`) mesure la
    hauteur au-dessus d'une sphère de rayon équatorial. À 43°N, le sol au
    niveau de la mer est à −9 902 m de cette sphère, un œil à 5 000 m à
    −4 902 m (calcul WGS84). Tout est ramené à 0 par `max(0)` : **dans les
    Pyrénées, toute la brume est calculée avec la densité du niveau de la
    mer**, quelle que soit l'altitude. L'indice « l'air est plus clair en
    montagne » disparaît. Les tests ne le voient pas parce qu'ils sont tous à
    l'équateur (`eye_at(0.0, 0.0, h)`).
  - L'inscattering ne tient pas compte de la transmittance du soleil
    (`aerial.wgsl:146-148` : `sun_up` seul). La brume reste bleu-blanc au
    coucher.
  - Pas d'ozone, pas de diffusion multiple, pas de phase Cornette-Shanks.
- `sky.rs` `SkyShell` : couleur de limbe par masse d'air « plaque plane »
  plafonnée (`MAX_AIR_MASS = 40`, `:55`), Rayleigh seul (`:149`), plancher de
  nuit ad hoc (`:72`). **Il n'est consommé par aucun backend** (aucune
  occurrence hors de la crate).

**`tuile-wgpu`** : `shader.wgsl:137-140` fait `albedo × (ambient + (1−ambient)·lambert)`
avec un soleil blanc et un ambiant scalaire, puis `aerial_perspective`. La
sortie va directement dans une surface sRGB 8 bits (`surface.rs:152-163`) :
**pas de cible HDR, pas d'exposition, pas de tone mapping**. Le fond est effacé
en noir (`examples/wgpu-viewer/src/app/frame.rs:265-271`), il n'y a pas de passe
de ciel. Le viewer place le soleil à l'heure (`app/mod.rs:169`) ; `globe-bulk`
le place à la verticale de l'œil.

**USD / Blender** :
- `crates/tuile-usd/src/stage.rs:274-285` écrit un `DomeLight` uni (0,8) et un
  `DistantLight` (2,5) **sans orientation**. Il éclaire donc selon le −Z du
  repère rebasé, pas selon l'heure. À vérifier : ces lumières atteignent-elles
  le rendu via la fusion de scène de `integrations/hydra/src/manifest.cpp` ? Si
  oui, elles s'ajoutent aux lumières Blender (double éclairage).
- `integrations/blender/render_usd.py:232-243` : monde Blender uni
  (0,45, 0,58, 0,78) × 0,6 et soleil de 4 W/m² à `rotation_euler = (0.7, 0.2, 0.3)`,
  sans lien avec l'heure. View transform `Standard` et exposition −1,5 EV
  épinglés et mesurés (`:60-84`).
- `integrations/hydra/tests/make_look.py:27` : troisième soleil (NOAA
  approché), mais la bonne forme : un `DomeLight` HDRI (`sky-day.exr` statique)
  plus un `DistantLight` orienté (`:167-178`).
- STL (`sportstracklive-rails/services/stl-3d-rendering`) : `host.rs:245-257`
  a un soleil `Zenith` ou « Recorder » en Euler fixes (50°, −30°) dans l'ENU,
  un ambiant 0,23 façon Unity. `stage.rs:194` `FILM_EXPOSURE_STOPS = −1.5`,
  recopié du défaut Blender et authored en `primvars:stl:filmExposure`
  (`:249`) pour compenser la couche de vol.

**Bilan** : trois ou quatre soleils, aucun ciel rendu, une brume physique mais
à hauteur faussée hors équateur, un éclairage sans couleur, pas d'HDR côté wgpu,
et des intensités Cycles fixées « à l'œil ». Le client web de référence
(`references/cesium/.../computeScattering.glsl`) fait une marche 16 × 4 pas par
fragment. Il prend un `atmosphereInnerRadius` dynamique précisément pour ne pas
confondre ellipsoïde et sphère : c'est notre bug, qu'il a résolu. Il expose
aussi plusieurs tone mappers (`Scene/Tonemapper.js`). À prendre comme
inspiration, sans rien en porter.

## 3. État de l'art

| Modèle | Principe | Altitude / espace | Crépuscule | Persp. aérienne | Coût | Usage pour nous |
|---|---|---|---|---|---|---|
| Nishita 1993 | marche Rayleigh+Mie, diffusion simple | oui | non (simple) | oui (marche) | élevé par pixel | base conceptuelle |
| Preetham 1999 | ajustement analytique (turbidité) | sol seul | faux sous ~10° | fog empirique | nul | non |
| Hosek-Wilkie 2012 | ajustement sur simulation spectrale, albédo sol | sol seul, soleil ≥ 0° | correct au-dessus de l'horizon | non | nul | **oracle** de ciel au sol |
| Bruneton-Neyret 2008 / 2017 | LUT 4D précalculées, diffusion multiple d'ordre n, ozone, profils custom, spectral | sol → espace | oui | oui (LUT) | précalcul lourd, rendu bon marché, artefacts d'horizon 4D | **oracle** / référence hors ligne (BSD) |
| Hillaire 2020 | LUT légères : transmittance 2D, diffusion multiple 2D, sky-view 2D par frame, perspective aérienne en froxels 3D | sol → espace | oui (+ozone) | oui (froxels) | < 1 ms GPU, recalcul dynamique | **standard temps réel** (Unreal, Bevy 0.16, WebGPU) |
| Ciel Blender 5.x « Multiple Scattering » (ex-Nishita « Single Scattering ») | texture de monde procédurale ; air, aérosols, ozone, altitude | caméra seulement | oui | **non** (pas de sol, pas de brume sur la géométrie) | faible | **oracle A/B** ; non utilisable via Hydra |

Points physiques retenus :
- **Ozone** : absorption dans la bande de Chappuis, profil en tente centré
  vers 25 km. Sans elle, le zénith crépusculaire vire au gris-jaune.
- **Phase de Mie** : Cornette-Shanks, meilleure que Henyey-Greenstein pour
  g ≈ 0,8 (Hillaire l'emploie).
- **Diffusion multiple** : l'approximation isotrope de second ordre de Hillaire
  (LUT 32×32) donne la luminosité des ombres de l'atmosphère et un horizon
  jamais noir. Elle remplace le plancher ad hoc de `SkyShell`.
- **RVB contre spectral** : trois longueurs d'onde suffisent au temps réel. La
  couleur du soleil rasant et du crépuscule est nettement plus juste en
  spectral (Bruneton 2017 convertit 15+ longueurs d'onde via CIE). Or c'est
  justement ce que la référence couleur consomme. Le CPU peut se l'offrir pour
  quelques scalaires par frame.
- **Soleil** : SPA (NREL, ±0,0003°) contre NOAA/Meeus (~0,01°). Pour éclairer,
  0,01° suffit. Ce qui compte davantage : la **réfraction** (+0,57° à
  l'horizon, soit ~3-4 min sur l'heure du coucher) et la **normale
  géodésique**. Disque solaire de 0,53°, assombrissement centre-bord (loi
  polynomiale) pour le disque vu seulement.
- **Exposition** : EV100 et facteur `1/(1,2·2^EV100)` (Lagarde & de Rousiers,
  « Moving Frostbite to PBR »), avec pré-exposition des sources.
- **Tone mapping** : **Khronos PBR Neutral**, conçu pour restituer fidèlement
  une couleur de base sRGB et n'écraser que les hautes lumières. Il est livré
  dans Blender depuis 4.2 et dans three.js, et c'est une formule analytique
  publiée, triviale en WGSL. C'est exactement le compromis que
  `render_usd.py:62-72` a mesuré (Standard fidèle mais qui écrête, AgX qui
  désature l'imagerie).

## 4. Recommandation

### 4.1 Le modèle : Hillaire 2020, calculé par une référence CPU partagée

`AtmosphereParams` (valeur canonique, sérialisable, entrant dans le digest de
rendu) :
- planète : rayon *local* (sphère osculatrice, cf. §4.2), épaisseur 100 km,
  albédo sol moyen (0,3 par défaut, ou moyenne basse fréquence de l'imagerie
  pour la région, fournie par le doc 18) ;
- Rayleigh : β_s (5,802 ; 13,558 ; 33,1)·10⁻⁶ m⁻¹, H = 8 km ;
- Mie : β_s 3,996·10⁻⁶, β_a 4,40·10⁻⁶ m⁻¹, H = 1,2 km, g = 0,8, avec un
  paramètre `turbidity` qui module β_Mie. Les 21·10⁻⁶ actuels deviennent le
  préréglage « brumeux » ;
- ozone : β_a (0,650 ; 1,881 ; 0,085)·10⁻⁶ m⁻¹, tente de 30 km centrée à 25 km ;
- éclairement solaire hors atmosphère : 1 360,8 W/m² (≈ 128 klx), RVB
  normalisé (convention : soleil hors atmosphère = blanc (1,1,1) × E₀) ;
- `night_floor` : luminance minimale du ciel et éclairement minimal du sol,
  garant de « jamais noir ».

Quatre produits dérivés, **tous calculables sur CPU (référence, déterministe)
et sur GPU (optimisation, validée contre le CPU)** :
1. `TransmittanceLut` 256×64 (hauteur, cos zénith solaire) : constante tant
   que les paramètres ne changent pas.
2. `MultiScatteringLut` 32×32 : idem.
3. `SkyViewLut` 192×108, par position d'œil et par soleil, paramétrée
   relativement à l'horizon apparent (donc correcte à 50 km). Recalculée quand
   l'œil bouge de plus d'une fraction d'échelle de hauteur ou quand le soleil
   bouge de plus de 0,05°, pas à chaque frame.
4. `AerialVolume` en froxels 32×32×N, par frame. **Amendement à Hillaire** :
   sa profondeur par défaut (32 km) ne couvre pas nos plans. Les tranches
   suivent une distribution quadratique jusqu'à la distance de l'horizon
   `√(h(2R+h))` (800 km à 50 km), avec N = 32 en dessous de 5 km et 64
   au-dessus. Au-delà du volume, et pour les rayons qui ratent le sol, on
   prend la sky-view.

La **forme fermée actuelle reste** comme implémentation « `ClosedForm` » de
`AtmosphereModel` : repli (web bas de gamme, tests) et garde-fou numérique.
Son `air_column` stable reste la brique de la transmittance le long des rayons
quasi horizontaux.

**Spectral côté CPU seulement** : `sun_transmittance`, `sky_irradiance` et
les LUT de référence du doc 18 peuvent être intégrés sur ~16 longueurs d'onde
puis projetés en Rec.709 linéaire ; les LUT GPU restent RVB. Ce sera l'étape 5,
et seulement si l'A/B montre un écart visible au coucher.

### 4.2 Géométrie : sphère osculatrice, hauteur géodésique

L'atmosphère est sphérique ; la Terre ne l'est pas. Par frame (ou par plan), on
prend le point de référence P (sol sous le centre de l'image, ou l'origine de
rendu), la normale géodésique n, et le rayon de Gauss `R = √(M·N)` à sa
latitude. Le centre de l'atmosphère vaut `P − n·R`, en f64 puis rebasé. Toute
hauteur envoyée au modèle est la **hauteur géodésique** (`ecef_to_geodetic`) ou
la distance à ce centre moins R, jamais `|ecef| − a`. L'erreur résiduelle sur
±400 km autour de P est de l'ordre de 100 m de hauteur, ce qui est invisible.
Les positions ne passent en f32 qu'après rebasage (règle 7).

### 4.3 Le soleil : un seul, en f64

`Sun::at_unix_seconds` est conservé comme source unique et complété par :
- `Sun::topocentric(&self, geodetic) -> SunLocal { azimuth, elevation_true, elevation_apparent, to_sun_enu, to_sun_ecef }`,
  avec la **normale géodésique** et une réfraction de Saemundsson/Bennett
  (pression et température standard, paramétrables). Au-delà de 1° sous
  l'horizon, pas de réfraction : continuité imposée par test ;
- `make_look.py` et `render_usd.py:243` cessent de calculer un soleil ; STL
  remplace `SceneSun::Recorder` par l'instant du vol ;
- l'instant vient des horodatages de la trajectoire (`tuile-tape`) ou d'un
  `--at` explicite, jamais de l'horloge murale.

L'éphéméride actuelle suffit (écart ≤ 0,01°). L'oracle de test est le cas
publié du SPA (NREL TP-560-34302) plus une grille d'instants et de lieux
calculée une fois avec l'outil en ligne NREL et figée comme fixture. Pas de
portage du SPA.

### 4.4 Temps réel (tuile-wgpu, puis façade web)

Passes, dans l'ordre :
1. **LUT** : transmittance et diffusion multiple à la création (compute, ou
   upload des LUT CPU) ; sky-view et volume aérien par frame, en compute.
   wgpu 29 le permet sur natif et WebGPU. Sur la façade three.js (WebGL2),
   les LUT CPU calculées dans le worker wasm sont transférées comme `DataTexture` :
   ~1 Mo par frame pour le volume, moins s'il est recalculé à mi-cadence.
2. **Sol** : cible **HDR `Rgba16Float`** au lieu de la surface sRGB. Le fragment
   calcule `albédo × (sun_rgb · T_sun(h, μ_s) · max(n·s, 0) + E_sky(n))`. Ici
   `T_sun` est lu **par fragment** dans la LUT de transmittance, ce qui donne
   un terminateur correct sur un globe entier et remplace la « LUT 1D
   élévation » du doc 18 §6.3. `E_sky` est une SH L1 fournie par le CPU. On
   applique ensuite `L·T_ap + S_ap`, lus dans l'`AerialVolume`. L'ensemble
   reste affine dans l'albédo : la composition multi-passe
   (`shader.wgsl:130-133`) reste exacte.
3. **Ciel** : triangle plein écran derrière la profondeur maximale. Il lit la
   sky-view, ajoute le disque solaire (assombrissement centre-bord) et remplace
   le clear noir. Vu de 50 km, sous l'horizon apparent sans géométrie, il
   dessine le sol « lointain » du modèle (albédo moyen éclairé et vu à travers
   l'air). **Même avant que les tuiles arrivent, le globe n'est pas noir.**
4. **Développement** : exposition (EV100 → facteur), adaptation chromatique
   partielle (doc 18), Khronos PBR Neutral, OETF sRGB, dithering. Une fonction
   WGSL a un jumeau Rust bit à bit testé. La façade three.js utilise son
   `NeutralToneMapping` natif, avec la même exposition.

Budget : Hillaire rapporte des coûts sous la milliseconde sur GPU grand public
à ces résolutions de LUT. La marche aérienne par pixel n'existe qu'en mode
qualité (orbite > 100 km), et le mode normal ne la déclenche jamais.

### 4.5 Hors ligne (USD / Hydra / Cycles) : comparaison et choix

**Ciel et éclairage.** Blender via Hydra ne transporte que des mondes
« couleur ou texture d'environnement » vers un `DomeLight`. Le nœud Sky Texture
n'y passe pas (les TODO du moteur Hydra de Blender le reconnaissent), et il
ignore la géométrie et la brume. Choix : **Rust génère le layer de look**
(sous-commande de `tuile-usd`, remplace `make_look.py` et les lignes
`stage.rs:274-285`) :
- `DomeLight` avec `texture:file` = une lat-long EXR demi-flottant 2048×1024,
  produite par `sky_radiance` à la position de la caméra, orientée par un xform
  ENU → repère rebasé. **Sans disque solaire** : le soleil est porté par le
  `DistantLight` (pas de double comptage, pas de lucioles). L'hémisphère bas
  contient l'inscattering au-dessus d'un sol d'albédo moyen, jamais du noir ;
- `DistantLight` : direction `Sun`, `angle = 0.53`, `color` et `intensity`
  = `sun_transmittance` au point de référence × E₀ × pré-exposition ;
- pré-exposition commune `k = 1/(1,2·2^EV100)` appliquée **dans les valeurs
  authored**, pour que Cycles travaille avec des grandeurs O(1). L'exposition
  Blender passe à 0 et l'`--exposure` restant devient une *compensation* en EV ;
- régénération de l'EXR quand l'œil change de plus de ~500 m d'altitude ou le
  soleil de plus de 0,1°. Pour un vol de 10 min, cela fait quelques dizaines
  d'EXR clés par frame (une texture qui change force un rechargement Hydra, à
  mesurer).

**Perspective aérienne**, quatre options :

| Option | Fidélité | Coût / 0,47 s | Parité wgpu | Déterminisme | Verdict |
|---|---|---|---|---|---|
| A. Volume Cycles (coquille à densité exponentielle, Principled Volume) | la meilleure (ombres des montagnes dans la brume) | ×5-×20 d'échantillons, bruit à 100 km d'échelle | faible | bruit dépendant du seed | exclu (option « prestige ») |
| B. **Post-traitement Rust sur RGB linéaire + profondeur** | identique au temps réel (même `AerialVolume`) ; pas d'ombres volumétriques | ~20-40 ms CPU (froxels 2M évaluations + trilinéaire 2,7 Mpx, rayon) | **exacte, par construction** | total | **retenu** |
| C. Dans le matériau Cycles (émission + atténuation en nœuds) | forme fermée seulement, pas de LUT 3D dans les nœuds | faible | approchée | bon | non : double la physique dans un graphe que hdCycles accepte mal |
| D. Compositeur Blender (Z pass) | idem B sans nos LUT | faible | approchée | bon | non |

B consiste à rendre en **linéaire brut** (view transform `Raw`/Non-Color,
exposition 0, demi-flottant) avec une passe de profondeur. Une étape de
**développement** Rust (exposée par l'ABI C de `tuile-hydra`, appelée par
`render_usd.py` avant ffmpeg) applique `L·T_ap + S_ap`, puis exposition,
adaptation, PBR Neutral, sRGB et dithering : **la même fonction que l'étape 4
de wgpu**. Les pixels de ciel (profondeur infinie) ne reçoivent pas de
perspective aérienne, puisque le dôme est déjà le ciel. Le Z de Cycles n'est
pas anti-aliasé, donc un bord crête/ciel prend la valeur d'un des deux. L'erreur
est faible car l'inscattering lointain tend vers la couleur du ciel
d'horizon ; à mesurer (§6).

Conséquence : le « look » quitte OCIO/Blender et devient notre code, identique
sur les deux chemins. `Standard`/`AgX` ne sont plus des choix de rendu. On garde
en fallback le view transform Blender « Khronos PBR Neutral » si l'étape de
développement n'est pas branchée : même courbe, moins de parité.

### 4.6 Exposition et chaîne de couleur

- **Unités** : luminance relative dont l'échelle absolue est connue (E₀), RVB
  Rec.709 linéaire, blanc = soleil hors atmosphère.
- **EV100 physique**, fonction de l'éclairement horizontal au point de
  référence : `EV100 = log2(E_h · 100 / C)`, avec C ≈ 250 (étalonnage de
  posemètre incident, ISO 2720). On lisse le long du plan comme une fonction de
  l'**index de frame** (pas du temps mural ni des pixels). Plus une
  compensation par plan (`exposure_ev`). En plein jour on retombe sur la règle
  du « sunny 16 » (EV100 ≈ 15). Au crépuscule, l'exposition monte mais reste
  plafonnée à +4 EV au-dessus de la valeur de plein jour, pour que la scène
  s'assombrisse visiblement au lieu d'être compensée à fond.
- STL : `FILM_EXPOSURE_STOPS` devient l'exposition totale lue dans le stage
  (primvar écrite par tuile). La couche de vol, émise à `2^-exposure`, suit
  l'heure au lieu d'une constante.

## 5. Architecture

### 5.1 Vue d'ensemble

```
                   tuile-core (wasm32) : geo (WGS84, geodetic, enu_frame)
                              ▲
 ┌────────────────────────────┴──────────────────────────────────────────┐
 │ tuile-atmosphere (wasm32, sans backend ; glam, bytemuck, libm, half)  │
 │  sun::Sun ─ at_unix_seconds ─ topocentric(geodetic) → SunLocal         │
 │  params::AtmosphereParams (canonique, digest)                          │
 │  trait AtmosphereModel ◄── ClosedForm (actuel aerial+sky, repli)       │
 │                        ◄── LutAtmosphere (Hillaire, CPU de référence)  │
 │  lut::{Transmittance, MultiScattering, SkyView, AerialVolume}          │
 │      .build_cpu(params, …) → Vec<f16>   + WGSL jumeaux (compute)       │
 │  sky_state::SkyState → ReferenceIllumination (doc 18)                  │
 │  develop::{exposure, pbr_neutral, srgb_oetf} (Rust + WGSL jumeaux)     │
 │  bake::equirect_sky(state, eye) → Image<f16>  (pour le DomeLight)      │
 └───────▲───────────────────────▲────────────────────────▲──────────────┘
         │                       │                        │
  tuile-wgpu                tuile-usd (layer de look)   tuile-core::radiometry
  passes LUT/sol/ciel/dev   DistantLight + DomeLight     (doc 18 : ne voit que
  (natif + WebGPU)          EXR + primvars exposition     des nombres, via
         │                       │                        ReferenceIllumination)
  tuile-web worker ──LUT CPU──► façade three.js (DataTexture, NeutralToneMapping)
                                 │
                     tuile-hydra ABI C : tuile_develop_frame(rgb, z, cam, state)
                                 │
                     render_usd.py (Raw → développement Rust → ffmpeg)
```

Règles : `tuile-core` n'apprend rien (règle 1). `tuile-atmosphere` reste sans
wgpu, tokio ni USD. Le WGSL y est du texte (précédent : `aerial.wgsl`), et le
CI ajoute `cargo check --target wasm32-unknown-unknown -p tuile-atmosphere`.
`tuile-server` ignore tout de ceci (règle 6). Pas de nom de marque dans les
types. `libm` (crate pure Rust, déjà dans le lock en 0.2.16) fournit
`exp/pow/atan2` **identiques bit à bit** sur macOS, Linux et wasm. C'est la
réponse à l'exigence « pas de libm plateforme » du doc 18 §5.

### 5.2 Le contrat du doc 18 §6 : adopté, avec six amendements

On adopte `Sun`, `AtmosphereModel`, `SkyState` et `ReferenceIllumination`, et
on amende :

```rust
pub trait AtmosphereModel: Send + Sync {
    fn params(&self) -> &AtmosphereParams;                       // (1) référence, pas copie
    fn sun_transmittance(&self, p: DVec3, to_sun: DVec3) -> [f64; 3];
    fn sky_irradiance(&self, p: DVec3, n: DVec3, to_sun: DVec3) -> [f64; 3];
    fn sky_irradiance_sh(&self, p: DVec3, to_sun: DVec3) -> [[f64; 3]; 4]; // (2)
    fn sky_radiance(&self, eye: DVec3, dir: DVec3, to_sun: DVec3) -> [f64; 3]; // (3)
    fn aerial(&self, eye: DVec3, p: DVec3, to_sun: DVec3) -> ([f64; 3], [f64; 3]);
    fn aerial_volume(&self, cam: &FrustumF64, to_sun: DVec3, slices: SliceLayout)
        -> AerialVolume;                                         // (4) fourni par défaut
}
pub struct SkyState { pub utc_seconds: f64, pub sun: Sun, pub frame: AtmosphereFrame, // (5)
                      pub params: AtmosphereParams }
pub struct ReferenceIllumination {
    pub to_sun_ecef: DVec3, pub sun_rgb: [f32; 3], pub sky_sh: [[f32; 3]; 4],   // (2)
    pub white: [f32; 3], pub adaptation: f32,
    pub ev100: f32, pub compensation_ev: f32,                    // (6)
    pub night_floor: f32,
}
```

1. `params()` renvoie une référence, pour que le digest et le bake USD lisent
   exactement l'état du modèle.
2. L'ambiant est une **SH L1** (4 × RVB), pas un `sky_rgb` scalaire. Les
   versants au soleil et à l'ombre reçoivent des ciels différents, et Cycles
   intègre le dôme directionnellement : avec un scalaire en wgpu, l'A/B diverge
   sur les pentes. `sky_irradiance(n)` reste l'évaluation exacte, pour les
   tests et la référence couleur.
3. **`sky_radiance`** manque au contrat. Il faut pour cuire le dôme, pour le
   ciel CPU de référence et pour la mesure §6.5-4 du doc 18 (raccord
   sol/ciel).
4. **`aerial_volume`** : `aerial()` point par point est trop lent pour 2,7 Mpx
   en ferme. Le volume de froxels est le produit partagé CPU/GPU ; son
   implémentation par défaut appelle `aerial()`.
5. **`AtmosphereFrame`** (centre et rayon de la sphère osculatrice, en f64)
   dans `SkyState`, et règle d'unités écrite dans le trait : les positions
   sont en ECEF f64, et toute hauteur est géodésique. C'est ce qui corrige le
   bug `|ecef| − a`. `SkyState` possède ses `AtmosphereParams` (valeur
   sérialisable) au lieu d'un `&dyn`, ce qui le rend sérialisable et
   digestible ; le modèle concret se reconstruit à partir de lui.
6. `exposure_ev` est séparé en `ev100` (physique, calculé) et
   `compensation_ev` (choix de plan). Le doc 18 met l'exposition « dans le
   view transform Blender » ; ici elle passe dans le développement commun
   (§4.5). Le `night_floor` est explicite.

Les `AerialPerspective` et `SkyShell` actuels deviennent `ClosedForm`, première
implémentation, **après** la correction de la hauteur.

## 6. Plan de mise en œuvre et critères d'acceptation

**Étape 0 — corriger la hauteur (petit, immédiat).** On écrit d'abord un test
de hauteur géodésique à 43°N et on vérifie qu'il échoue sur le code actuel
(règle « test qui échoue d'abord »), puis on applique la sphère osculatrice à
`AerialPerspective`, `SkyShell` et `aerial.wgsl`. Acceptation : transmittance à
5 km d'altitude sur 30 km à 43°N strictement supérieure à celle du niveau de
la mer. Film A/B de l'orbite pyrénéenne avant/après, ouvert, rangé dans
`videos/` avec son entrée README.

**Étape 1 — un seul soleil.** `Sun::topocentric` (géodésique + réfraction) ;
suppression des soleils de `make_look.py`, `render_usd.py:243` et STL.
Acceptation : écart au SPA ≤ 0,01° sur la grille figée ; direction identique à
1e-6 entre l'uniform wgpu, le `DistantLight` relu dans le stage et le soleil de
Blender. Réfraction continue en 0° (test de monotonie).

**Étape 2 — `LutAtmosphere` CPU + trait amendé.** LUT de Hillaire en Rust
(f64 → f16), `libm`, pas d'aléa. Contrôles numériques (tests) :
- épaisseur optique Rayleigh au zénith à 550 nm ≈ 0,097 (±5 %) ;
- éclairement direct normal au niveau de la mer, soleil au zénith, ciel clair :
  entre 850 et 1 050 W/m² ;
- masse d'air relative à l'horizon entre 36 et 40 (Kasten-Young ≈ 38) ;
- rapport diffus/global horizontal à 60° d'élévation entre 0,10 et 0,20 ;
  croissant de façon monotone quand le soleil descend ;
- rougissement : R/B du soleil transmis monotone croissant de 90° à 0°. À 2°
  d'élévation, chromaticité CIE xy du soleil au-delà de la locus 3 000 K ;
- crépuscule : à −4°, luminance du zénith B > R **avec** ozone, et le test
  échoue sans ozone ;
- ciel au sol comparé à **Hosek-Wilkie** (turbidité ajustée) : ΔE2000 médian
  < 5 sur l'hémisphère, soleil de 5° à 60° ;
- LUT GPU contre LUT CPU : écart relatif max < 1e-3 ;
- hash des LUT CPU identique sur macOS, Linux et wasm.

**Étape 3 — wgpu : HDR, sol, ciel, développement.** Cibles `Rgba16Float`,
passes du §4.4, SH L1, PBR Neutral. Acceptation : 60 fps sur le viewer
(orbite pyrénéenne, 1440p, M2) avec passes atmosphère ≤ 1,5 ms GPU mesurées ;
**aucune frame à fond noir** en partant de 50 km puis en descendant à 500 m
avant l'arrivée des tuiles (capture de chaque frame, pixel noir = échec) ;
carte grise 18 % (doc 18 §6.5-3) conforme au modèle à ΔE < 2 ; raccord
horizon sol/ciel ΔE < 3. Films 08 h / 13 h / 19 h 30 ouverts.

**Étape 4 — USD/Cycles.** Layer de look Rust (dôme EXR + `DistantLight`
pré-exposés), rendu Raw + Z, développement Rust via ABI C. Acceptation :
- plan blanc lambertien et sphère grise : rapport Cycles/modèle à 3 % près
  (étalonne une fois les unités `intensity` de hdCycles) ;
- **A/B wgpu ↔ Cycles** sur 5 frames fixes (500 m horizon, 3 km vers le sol,
  15 km, 50 km, coucher) : ΔE2000 médian < 3 sur le sol, < 2 sur le ciel ;
- dôme cuit comparé au ciel Blender 5 « Multiple Scattering » (mêmes
  élévation, altitude, air, aérosols, ozone) : ΔE médian < 5, écarts
  documentés ;
- budget : développement + régénération amortie des EXR ≤ 50 ms/frame, total
  ≤ 0,47 s/frame maintenu (64 frames ≤ 30 s, mesuré sur la ferme GCP) ;
- chaque film ouvert, rangé dans `videos/` avec trajectoire, pack, digest de
  scène (incluant `SkyState` quantifié), imagerie, viewport, échantillons,
  **instant et paramètres d'atmosphère**.

**Étape 5 (optionnelle) — spectral CPU** pour `sun_transmittance` et
`sky_irradiance` : seulement si l'A/B contre photos de référence (voir
ci-dessous) montre un écart de teinte au coucher.

**Référence photographique.** Une série de photos de référence datées et
géolocalisées (Pyrénées, belvédères connus, plusieurs heures), réduites en
basse fréquence : on compare chromaticité du ciel à l'horizon, rapport de
luminance sol lointain/ciel et contraste des crêtes par tranche de distance. On
ne vise pas le pixel : on vise la tendance avec la distance et l'heure.

## 7. Risques et questions ouvertes

- **Extraction des pixels linéaires de Blender** : lire le Render Result en
  flottant et le passer au développement Rust sans passer par le disque
  (Viewer node + `foreach_get`, ou pipe EXR). À mesurer contre les 0,47 s. En
  repli : view transform Blender « Khronos PBR Neutral » et perspective
  aérienne dans le compositeur (option D), avec parité dégradée et documentée.
- **Passe Z via Hydra/hdCycles dans Blender** : disponibilité des AOV
  `depth` avec le moteur Hydra de Blender, à vérifier en premier. Sinon, AOV
  `Peye`/position monde depuis le délégué.
- **Unités des lumières hdCycles** (`intensity`, `normalize`, `exposure` de
  UsdLux contre le « W/m² » de Cycles) : à étalonner une fois par l'étape 4,
  puis à épingler par test.
- **Double éclairage** : les lumières du manifeste passent-elles par la fusion
  de `manifest.cpp` en plus du monde Blender ? À trancher avant l'étape 4.
- **Rechargement de texture de dôme** dans Hydra quand l'EXR change : coût et
  risque de frame sans ciel. Règle de CLAUDE.md : le nouveau dôme est chargé
  avant de retirer l'ancien, jamais de frame entre deux.
- **Couleur unique du `DistantLight`** : sur un plan continental au coucher,
  Cycles éclaire tout le sol avec le soleil du point de référence, alors que
  wgpu lit la transmittance par fragment. Pour la parité, un drapeau
  `sun_per_fragment` à `false` sur le recorder de ferme ; la perspective
  aérienne, elle, reste par pixel sur les deux chemins.
- **Ombres de la brume** (rayons crépusculaires derrière les crêtes) : absentes
  sur les deux chemins, cohérent mais moins réaliste. Une shadow map dans les
  froxels plus tard côté wgpu, un volume Cycles pour des plans de prestige.
- **Nuages** : hors périmètre v1. Couche 2D (épaisseur optique) éclairée par
  le même soleil en v2 ; le volumétrique est incompatible avec le budget
  Cycles. **Étoiles et nuit** : plancher `night_floor`, pas de ciel étoilé
  en v1.
- **Imagerie déjà éclairée** : sans le dé-éclairage du doc 18, le lambert
  ajoute un second ombrage de relief. La normalisation « facteur 1 sous
  l'illumination de référence » dépend de l'aboutissement du doc 18.
- **Réalisme contre fidélité** : une vraie brume désature l'imagerie lointaine,
  ce que le choix `Standard` voulait éviter. La compensation passe par
  `turbidity` par plan (réglage artistique borné, inscrit dans le digest), pas
  par la courbe.
- **Licence des oracles** : Bruneton (BSD) et Hosek-Wilkie servent de
  références de test seulement, sans code porté. Le code de référence de
  PBR Neutral (Khronos) est à vérifier avant d'écrire le jumeau WGSL ; la
  formule publiée seule suffit.

## Sources

- Hillaire, *A Scalable and Production Ready Sky and Atmosphere Rendering Technique*, EGSR 2020 — https://onlinelibrary.wiley.com/doi/abs/10.1111/cgf.14050
- Bruneton, *Precomputed Atmospheric Scattering: a New Implementation* (2017, BSD, ozone, profils) — https://ebruneton.github.io/precomputed_atmospheric_scattering/ ; dépôt https://github.com/ebruneton/precomputed_atmospheric_scattering
- Bevy 0.16, atmosphère Hillaire en WebGPU (transmittance, diffusion multiple, froxels) — https://bevy.org/news/bevy-0-16/
- Blender 5.0, ciel « Multiple Scattering », Nishita renommé « Single Scattering » — https://www.blender.org/download/releases/5-0/ ; https://projects.blender.org/blender/blender/pulls/140480
- Moteur Hydra de Blender et export monde → DomeLight — https://projects.blender.org/blender/blender/issues/110765 ; https://projects.blender.org/blender/blender/pulls/123933
- Blender 4.2, view transform Khronos PBR Neutral — https://developer.blender.org/docs/release_notes/4.2/rendering/ ; Khronos — https://www.khronos.org/news/press/khronos-pbr-neutral-tone-mapper-released-for-true-to-life-color-rendering-of-3d-products
- Reda & Andreas, *Solar Position Algorithm* (NREL TP-560-34302, ±0,0003°) — https://docs.nlr.gov/docs/fy08osti/34302.pdf ; NOAA — https://gml.noaa.gov/grad/solcalc/calcdetails.html
- Nishita et al. 1993 ; Preetham et al. 1999 ; Hosek & Wilkie 2012 ; Cornette & Shanks 1992 ; Lagarde & de Rousiers, *Moving Frostbite to PBR* (2014) : références classiques, citées pour les modèles et l'EV100.
- Références locales (inspiration seulement) : `references/cesium/packages/engine/Source/Shaders/Builtin/Functions/computeScattering.glsl` (16×4 pas, `atmosphereInnerRadius`), `Scene/Tonemapper.js`.
