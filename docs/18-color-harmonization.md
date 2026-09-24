# 18 — Harmonisation colorimétrique de l'imagerie drapée

Document d'architecture et de R&D, rédigé le 2026-09-24 sur `feat/tile-server`.
Rien n'est implémenté. Il fixe le problème, l'existant, l'état de l'art, une
recommandation par étapes, l'architecture (cœur + deux consommateurs), le
niveau de référence colorimétrique lié à l'heure et au soleil (§6), le banc
de tests, les crates et techniques utilisables, et les critères de mesure.

## 1. Le problème

Le globe est un terrain drapé d'imagerie aérienne (aujourd'hui Bing Aerial via
Cesium ion, asset 2 ; Sentinel-2 via l'asset ion 3954 a été essayé). Les tuiles
reçues ne s'accordent pas en couleur :

- **Damier entre voisines** : deux tuiles d'un même niveau, côte à côte,
  diffèrent en luminosité, balance des blancs, saturation, voile
  atmosphérique, saison. Le quadrillage de la pyramide devient visible.
- **Sauts entre niveaux** : un même écran mélange plusieurs niveaux
  d'imagerie (fin près de la caméra, grossier au loin, et un *floor* grossier
  sous chaque drapé). Les frontières de niveau deviennent des frontières de
  couleur.
- **Pops de re-drapage** : quand la caméra approche, une tuile est redrapée
  plus finement ; si le niveau fin vient d'une autre prise de vue, la couleur
  saute d'une frame à l'autre — dans le viewer comme dans un film de ferme.

**Pourquoi Bing diffère.** Bing n'a qu'une seule « couverture », mais c'est une
mosaïque de sources, de dates et de résolutions différentes (de 15 m à 10 cm,
Landsat aux niveaux bas, satellites commerciaux au milieu, aérien aux niveaux
hauts), chaque tuile portant une *plage* de dates de prise de vue
([Plex-Earth](https://support.plexearth.com/hc/en-us/articles/216073283-Viewing-Bing-Maps-imagery-dates),
[P. Gärtner](https://philippgaertner.github.io/2018/11/microsoft-bing-maps/)).
Les lots de prise de vue changent **par niveau** : le niveau z et le niveau z+1
d'un même sol ne viennent pas forcément du même vol ni de la même saison. Le
code l'a déjà mesuré : « adjacent terrain levels carry imagery from different
capture batches, every LOD boundary becomes an exposure seam »
(`crates/tuile-bake/src/session.rs:505-512`).

**Pourquoi Sentinel-2 seul ne suffit pas.** Une mosaïque annuelle sans nuages
(médiane sur une pile temporelle) est radiométriquement homogène par
construction ([EOX](https://eox.at/2017/03/sentinel-2-cloudless/)), mais sa
résolution est de 10 m/px : au-delà de ~z14 elle n'apporte plus aucun détail,
et le sol devient flou exactement là où la caméra regarde. Elle est en revanche
une **référence basse fréquence** idéale.

Contrainte transversale (CLAUDE.md) : **jamais de noir, jamais de trou**, et
**déterminisme exact** pour les rendus de ferme (même pack → mêmes octets).

## 2. Ce qui existe dans tuile aujourd'hui

**Chaîne d'imagerie, cœur (`tuile-core`, wasm-clean).**
- `ImageryProvider` (`crates/tuile-core/src/raster.rs:1284`) sert les octets
  tels que reçus ; `CachedImagery` (`raster.rs:1359`) les met en cache par
  `img/{namespace}/{z}/{x}/{y}`.
- `decode_and_reproject` (`raster.rs:1461`) → `reproject_to_geographic`
  (`raster.rs:645`) : **chaque texel est parcouru avec sa longitude/latitude
  f64** — c'est le point d'accroche le plus naturel pour appliquer un champ de
  correction géographique, sans passe supplémentaire.
- `ImageryLayer` (`raster.rs:866`, `placed`/`substituted` `:898`/`:940`) :
  couverture + placement affine d'une tuile d'imagerie dans l'uv d'une tuile
  géométrique. Les masques sont **durs** (`step`), sans fondu.
- `bake_layers` (`raster.rs:1199`) et `bake_imagery` (`raster.rs:1252`) :
  composition CPU, texel par texel, sémantique identique au shader, **déjà
  déterministe** (« same layers, same size, same bytes »).
- `drape_identity` (`raster.rs:73`) : identité d'un drapé = coords des couches
  + `composed_at`. **Toute correction devra entrer dans cette identité.**
  Attention : elle hache avec `std::collections::hash_map::DefaultHasher`, dont
  l'algorithme n'est pas garanti stable entre versions de Rust — risque
  distinct mais aggravé dès qu'une clé de pack en dépend davantage.
- Côté GPU, le contrat de mélange est partagé : `blend_layer`
  (`crates/tuile-core/src/raster.wgsl:26`).

**Drapage planétaire (`tuile-planetary`).**
- `PlanetaryLoader::drape` (`crates/tuile-planetary/src/lib.rs:900`) : niveau
  de base depuis l'erreur géométrique, raffiné par l'altitude caméra
  (`ImageryDetail`), borné par `imagery_boost_cap` (`lib.rs:496`, « quantising
  into an exposure checkerboard ») ; un **floor** grossier posé sous la
  mosaïque (`lib.rs:1007-1040`), puis les tuiles fines (`lib.rs:1056`).
- `fetch_imagery` (`lib.rs:661`) : décodage + reprojection + détection de
  tuile noire dans **un seul job offloadé** — le lieu où greffer la correction
  (même job, même localité mémoire), avant l'insertion dans `ImageryCache`.
- `deterministic_floor` (`lib.rs:518`) et `fixed_floor` (`lib.rs:120`) :
  leçon capitale — `coarse_floor` (`lib.rs:131`) dépend de « ce qui est déjà
  décodé » et a rendu 16 tuiles sur 80 différentes entre deux cuissons. **Une
  correction ne doit jamais dépendre de l'état d'un cache.**
- `LayerBudget::set_unbounded` (`lib.rs:360`) : la cuisson CPU porte toutes
  les couches ; le GPU est plafonné par ses bindings.

**Temps réel (`tuile-wgpu`).**
- Textures d'imagerie **partagées par coord** : `gpu.shared_imagery(layer.coord, …)`
  (`crates/tuile-wgpu/src/prepare.rs:198`). Une correction appliquée au texel
  CPU, keyée par coord, se propage donc gratuitement.
- `fs_main` (`crates/tuile-wgpu/src/shader.wgsl:113-168`) : couches → lambert
  → `aerial_perspective` (`tuile-atmosphere`). Le commentaire `:130-133` note
  que la composition multi-passe reste exacte parce que tout est linéaire en
  couleur : une correction **affine** par texel conserve cette propriété, une
  courbe non linéaire appliquée *après* le mélange ne la conserve pas.
- Modes diagnostics (`shader.wgsl:108-111`) : on pourra ajouter une vue
  « niveau d'imagerie ».

**Cuisson et pack (`tuile-bake`, `tuile-pack`).**
- `bake_imagery` appelé `session.rs:1227`, encodage PNG `encode_baked`
  (`session.rs:1273`), clé `TextureKey`/`drape_key` (`session.rs:221`,
  `:1311`) ; pack stocké par `(id, drape)` (`crates/tuile-pack/schema/tuile_pack.fbs`,
  `table Tile`).
- `bake_settings` (`crates/tuile-bake/src/main.rs:517`) alimente le digest de
  scène : la version de correction devra y figurer, sinon deux packs corrigé /
  non corrigé se répondront l'un pour l'autre (même piège que
  `imagery_boost` le 17 septembre).

**OpenUSD / Cycles.**
- `_MaterialNetwork` (`integrations/hydra/src/procedural.cpp:329`) : nœud
  natif `cycles_image_texture` (`:366`) → `UsdPreviewSurface.diffuseColor`
  (`:444`). Le hdCycles embarqué refuse déjà un nœud à la fois source et cible :
  **ajouter des nœuds de correction dans ce réseau est fragile**.
- `render_usd.py:58-80,274` : view transform `Standard` épinglée et exposition
  −1,5 diaph — le « look » global, distinct de l'harmonisation.
- `integrations/hydra/tests/look.md` (« Altérer la TEXTURE elle-même ») :
  grading en post ffmpeg aujourd'hui.

**Magasin de tuiles (`tuile-tile-server`).** `Layer` (`src/layer.rs`) avec
epochs d'expiration (conditions d'un fournisseur), `catalog.json`
(`src/catalog.rs`, `tile_type` accepte `other`). Une couche dérivée (champ de
correction) ou de référence (Sentinel-2) s'y range naturellement.

**Implémentations de référence.** Le client web expose par couche
`brightness/contrast/hue/saturation/gamma`, éventuellement comme fonction de
`(x, y, level)` (`references/cesium/packages/engine/Source/Scene/ImageryLayer.js:67-103`,
appliqué dans `GlobeFS.glsl:237,269`) : réglage manuel, aucune harmonisation
automatique. Le moteur C++ n'a rien sur la couleur des overlays.

## 3. État de l'art

| Approche | Principe | + | − | Coût |
|---|---|---|---|---|
| **Gain/offset par image, moindres carrés** | Un gain (et offset) par image, minimisant les écarts sur les recouvrements + régularisation vers 1 ([Brown & Lowe 2007](https://www.cs.ubc.ca/~lowe/papers/07brown.pdf)) | simple, global | nécessite recouvrements ; un scalaire par tuile laisse des marches internes | système creux, offline |
| **Wallis / dodging** | Aligner moyenne et écart-type locaux sur une cible ([Improved Wallis, Sensors 2017](https://doi.org/10.3390/s17030623)) | classique en orthophoto, robuste | cible à choisir ; halos près des contrastes forts | O(pixels) |
| **Champ de gains interpolé** | Correction calculée par bloc puis interpolée par spline → ajustement spatialement continu ([Uyttendaele, Eden, Szeliski 2001](https://szeliski.org/papers/Uyttendaele_EliminatingGhosting_CVPR01.pdf)) | **continu par construction**, léger | estimation aux bords | O(blocs) |
| **Optimisation globale de cohérence** | Modèles couleur par image optimisés conjointement, local + global ([Xia et al. ISPRS 2017, guided](https://www.sciencedirect.com/science/article/abs/pii/S0924271616305731), [joint global/local 2020](https://www.sciencedirect.com/science/article/abs/pii/S0924271620302781), [probabiliste 2022](https://www.sciencedirect.com/science/article/abs/pii/S0924271622001484)) | meilleure qualité sur grandes mosaïques | lourd, non incrémental | offline, cluster |
| **Transfert statistique** | Moyenne/écart-type par canal dans un espace décorrélé ([Reinhard 2001](https://www.semanticscholar.org/paper/Color-Transfer-between-Images-Reinhard-Ashikhmin/f3a11158e9d8bdfdf07dca756335c084fce0123e)) ; [OKLab](https://bottosson.github.io/posts/oklab/) comme espace moderne | trivial, GPU-friendly | global, ignore la structure | négligeable |
| **Appariement d'histogramme sur référence** | Mapbox calibre NAIP (1 m) sur Landsat (30 m) ; limite : les détails fins sont « lavés » par les couleurs des gros objets ([Mapbox NAIP](https://medium.com/mapbox/processing-raw-naip-scenes-into-seamless-imagery-ab23c7cb4def)) | cohérence absolue | écrase le détail si appliqué en pleine bande | O(pixels) |
| **Référence basse fréquence + détail haute fréquence** | `out = raw ⊙ LP(ref)/LP(raw)` (ou additif en espace log/OKLab) : couleur basse fréquence de la référence, détail de l'aérien ; c'est la logique du pan-sharpening et de la normalisation relative sur Sentinel-2 ([PlanetScope→S2, Remote Sensing 2024](https://www.mdpi.com/2072-4292/16/21/4047), [RRN multi-références, ISPRS 2020](https://isprs-annals.copernicus.org/articles/V-2-2020/845/2020/)) | garde le détail de Bing, résout la cohérence **entre niveaux** | halos aux marches nettes ; saisons/neige ; dépend de la licence de la référence | O(pixels) + fetch référence |
| **Mélange multi-bandes** | Pyramide laplacienne, transitions proportionnelles à la longueur d'onde ([Burt & Adelson 1983](https://dl.acm.org/doi/10.1145/245.247)) | coutures invisibles | exige un recouvrement ; mosaïque à plat | O(pixels·log) |
| **Domaine du gradient / Poisson** | Résoudre un Poisson sur les gradients ([Pérez et al. 2003](https://dl.acm.org/doi/10.1145/882262.882269)) ; à l'échelle gigapixel par multigrille en flux ([Kazhdan & Hoppe 2008](https://hhoppe.com/smg.pdf)) ; pour la RRN ([arXiv 2106.07441](https://arxiv.org/pdf/2106.07441)) | l'état de l'art visuel | produit une image dérivée complète ; lourd | offline |
| **Fondu de niveaux (clipmaps)** | Zone de transition où texture et géométrie interpolent le niveau grossier ([Losasso & Hoppe 2004](https://hhoppe.com/geomclipmap.pdf), [GPU Gems 2 ch. 2](https://hhoppe.com/gpugcm.pdf)) | tue la *marche* spatiale de niveau | ne corrige pas une couleur fausse, la dilue | shader |

**Ce que font les globes.** Google a corrigé le « mottled » de ses bandes
satellite par correction couleur puis a reconstruit une mosaïque Landsat 8
sans nuages ([Google Earth Blog 2009](https://www.gearthblog.com/blog/archives/2009/06/improving_google_earth_imagery.html),
[Google 2016](https://blog.google/products-and-platforms/products/earth/keeping-earth-up-to-date-and-looking/)).
Mapbox calibre sur une couche de référence et n'ajuste qu'avec les voisines
immédiates pour éviter la dérive ([Mapbox Satellite](https://docs.mapbox.com/data/tilesets/reference/mapbox-satellite/)).
Esri propose un fond « Clarity » d'archives plus nettes et Vivid, mosaïques
« color-balanced » ([Esri](https://www.esri.com/arcgis-blog/products/imagery/imagery/world-imagery-clarity-a-clearer-view-of-the-world)).
NASA Blue Marble NG : composites mensuels 500 m, domaine public
([NASA SVS](https://svs.gsfc.nasa.gov/3539)). Tous harmonisent **en amont, en
production**, jamais dans le client — sauf les réglages manuels par couche du
client web de référence.

**Contraintes externes.**
- Conditions Bing : pas de dérivé, notamment « creating server-side
  modification of map tiles » ou « stitching … to create other imagery
  products » ([Bing Maps TOU](https://www.bingmapsportal.com/terms)). À faire
  valider juridiquement ; conséquence d'architecture ci-dessous : **le magasin
  garde les octets Bing tels que reçus** (c'est déjà le contrat de `Layer`), la
  correction est appliquée **au rendu, côté client**, et ce qu'on persiste
  est au plus un champ de statistiques. Les tuilepacks, qui contiennent déjà
  des mosaïques Bing composées, posent la même question indépendamment de ce
  document.
- Sentinel-2 cloudless : 2016 en **CC BY 4.0**, 2018–2024 en **CC BY-NC-SA
  4.0** (licence commerciale EOX séparée)
  ([EOX 2018](https://eox.at/2019/02/sentinel-2-cloudless-2018/),
  [EOX 2024](https://eox.at/2025/03/sentinel-2-cloudless-2024/)). L'asset ion
  3954 suit les conditions ion. Alternative sans ambiguïté : Blue Marble NG
  (grossier) ou un composite maison depuis Copernicus L2A (lourd).

## 4. Recommandation

Principe directeur : **corriger la basse fréquence, garder la haute
fréquence, et rendre la correction fonction pure de `(coord, données
source, paramètres)`**. Une correction qui ne dépend que de ça est
déterministe, cacheable par coord, partageable entre wgpu et Cycles, et ne peut
pas créer de trou (au pire : identité).

**Étape 1 — cohérence de pyramide, sans référence externe.** Pour chaque tuile
d'imagerie au niveau L, on calcule un champ de gains/offsets basse fréquence
(grille 9×9 de sommets sur la tuile, ~32 px par cellule pour 256²) tel que
`LP(corrigée_L) ≈ LP(corrigée_{L-1} restreinte au quadrant)`, avec
`LP` = moyennes de blocs en lumière linéaire. Récursivement, ancré à un niveau
`A` (défaut z10–z12) laissé tel quel. Effets :
- les **sauts de niveau** et les **pops de re-drapage** disparaissent par
  construction (la basse fréquence de l'enfant *est* celle du parent) ;
- le **damier** entre voisines d'un même niveau est corrigé dans la mesure où
  leur parent commun est homogène — les lots de prise de vue de Bing étant
  souvent alignés sur des tuiles d'un niveau d'ingestion, l'estimation
  unilatérale par tuile est justement la bonne (hypothèse à mesurer, §9).
- modèle par cellule : gain diagonal + offset en RGB linéaire (6 paramètres :
  exposition, balance des blancs, voile additif), option chroma OKLab
  (1 paramètre). Gains bornés [0,5 ; 2], offset borné, force atténuée quand la
  structure diffère trop (neige, chantier, nuage : confiance = corrélation des
  hautes fréquences parent/enfant).

**Étape 2 — ancrage sur une référence radiométriquement homogène.** Le niveau
d'ancrage A est lui-même corrigé vers Sentinel-2 cloudless (licence à trancher :
2016 CC BY en premier choix) ou Blue Marble NG pour les niveaux < z8. Le
damier *au niveau d'ancrage* disparaît alors, et la chaîne de l'étape 1
propage cette cohérence à tous les niveaux fins, Bing gardant tout son détail.
La référence est une couche durable du magasin (`s2cloudless-2016`), lue par
un `ImageryProvider` ordinaire.

**Étape 3 — optimisation globale offline (optionnelle).** Pour une région de
production (Pyrénées), un job de ferme résout un moindres carrés creux sur les
champs de correction (écarts aux bords des tuiles + fidélité à la référence +
régularité), à la [Brown & Lowe] / [Xia 2017], et publie le résultat comme
couche `…/color-field-vN` dans le magasin. Les deux consommateurs lisent cette
couche à la place du calcul à la volée ; même format, même point
d'application. Le Poisson plein (Kazhdan–Hoppe) reste hors périmètre : il
produit une image dérivée complète.

**Étape 1b (viewer seulement) — fondu temporel de re-drapage.** Le nouveau drapé
est activé, l'ancien reste 250 ms en fondu dessous (chevauchement autorisé,
trou interdit — règle d'échange de surfaces de CLAUDE.md). En ferme, pas de
fondu wall-clock ; s'il en faut un, il sera fonction de l'index de frame et
les deux drapés iront dans le pack.

**Où appliquer : à la composition CPU, pas dans le shader.**

| | Composition CPU (au décodage de la tuile d'imagerie) | Shader / nœuds matériau |
|---|---|---|
| Implémentations | **une seule** (cœur) | deux (WGSL + réseau Cycles) |
| wgpu | texture partagée par coord déjà corrigée, zéro binding | +1 texture de champ par couche : bindings déjà saturés (`imagery_slots`) |
| Cycles | PNG déjà corrigé, réseau inchangé | nœuds supplémentaires dans un hdCycles qui en refuse déjà |
| Déterminisme | total (entiers + LUT) | dépend des GPU / de la version de Cycles |
| Réglage en direct | recalcul des tuiles | instantané |
| Conditions fournisseur | pixels modifiés en mémoire client | idem |

Le shader garde un rôle : le **look** global (exposition, saturation,
courbe), uniforme, identique pour toutes les tuiles, et son jumeau Blender
(view transform + exposition, déjà épinglés). Harmonisation et look sont deux
couches distinctes ; seule la première touche aux données. Entre les deux
s'intercale l'éclairage de l'heure de rendu, dérivé du soleil et du ciel :
c'est le niveau de référence du §6.

## 5. Architecture

```
                         tuile-core (wasm32, sans backend)
  ┌──────────────────────────────────────────────────────────────────────┐
  │ radiometry::                                                         │
  │   ColorAffine {gain:[f32;3], offset:[f32;3], chroma:f32}             │
  │   ColorField  {coord: ImageryCoord, side: u8 (9), cells: [ColorAffine]}│
  │   TileStats   {coord, blocks: [[u32;4]; 16×16] sommes linéaires+n}   │
  │   CorrectionParams {version, anchor_level, ref_layer, clamp, …}      │
  │        .digest() ──────────────► drape_identity / scene digest       │
  │   trait FieldSource (async, comme ImageryProvider)                   │
  │        ├─ Identity                                                   │
  │        ├─ PyramidChain<P: ImageryProvider, R: Option<ImageryProvider>>│
  │        └─ Stored<S: ContentStore>   (couche du magasin, étape 3)     │
  │   fn apply(tex: &mut DecodedTexture, f: &ColorField)  — pur, déterm. │
  └──────────────▲───────────────────────────────▲───────────────────────┘
                 │ stats/champs keyés par coord  │
  tuile-planetary::fetch_imagery (job offloadé)  │
     decode → reproject → stats → field → apply ─┘
                 │ Arc<DecodedTexture> corrigée, clé = (coord, digest)
        ┌────────┴──────────────┐
        ▼                       ▼
  tuile-wgpu               tuile-bake (bake_layers → PNG → tuilepack)
  shared_imagery(coord)         │  pack: Tile(id, drape⊇digest)
  shader: look uniforme         ▼
                           tuile-hydra / procédural → cycles_image_texture
                           Blender: view transform + exposition (look)

  tuile-tile-server (magasin) : bing-aerial (octets bruts, epochs)
                               s2cloudless-2016 (référence, durable)
                               bing-aerial.color-field-vN (étape 3, tile_type other)
```

**Modèle de données.** Un `ColorField` est un treillis de sommets **alignés sur
une grille géographique globale** du niveau L (les sommets de bord sont
partagés entre voisines) ; chaque texel reçoit l'interpolation bilinéaire des
quatre sommets qui l'entourent, calculée dans la boucle existante de
`reproject_to_geographic`. Encodage : f16 (`half`, déjà dans le lock), 81
sommets × 7 = 1,1 Ko par tuile. Calcul, étape 1 : `TileStats` du parent et de
l'enfant (sommes **entières** de valeurs linéaires sur 16 bits → ordre
d'accumulation indifférent), rapport bloc à bloc, lissage, bornes. La
dépendance aux ancêtres est **géométrique** (`ImageryCoord` parent), jamais
« l'ancêtre qui se trouve en cache » — c'est la leçon de `coarse_floor`. Les
ancêtres jusqu'à A sont de toute façon demandés (floor), et leurs stats
pèsent quelques Ko : un cache `coord → (TileStats, ColorField)` suffit.

**Déterminisme.** sRGB→linéaire par LUT 256 entrées, linéaire→sRGB par LUT
4096 entrées + arrondi entier ; aucune `powf`/`exp` de libm (divergent entre
plateformes) ; f32 sans FMA implicite (Rust ne contracte pas). Même binaire,
même donnée → mêmes octets, testés par hash. Les données source changeant
dans le temps, le déterminisme de ferme repose sur le snapshot du magasin
(epochs) déjà utilisé.

**Sans noir, jamais.** Échec de calcul, stats absentes, tuile noire
(`is_opaque_black`, `lib.rs:166`) ou référence indisponible → `Identity`, la
tuile est drapée non corrigée. `apply` ne peut ni annuler un canal (gain ≥ 0,5)
ni rendre transparente une couche (alpha intouché). Le viewer ne publie un
drapé qu'une fois sa tuile corrigée : un drapé plus tardif (le précédent reste
affiché), jamais un trou.

**Clés et cache.** `CorrectionParams::digest()` entre dans `drape_identity`
(via un champ à côté de `composed_at`, ou en le composant), dans la clé du
`TextureMemo`, dans `bake_settings` et dans la clé GPU `shared_imagery`
(sinon un changement de paramètres réutiliserait une texture non corrigée).
Le pack peut porter le digest en clair pour la provenance.

**Règles de dépendance.** `radiometry` ne dépend que de `glam`, `half`,
`bytemuck` — rien de nouveau hors du core autorisé ; `palette` ou `oklab` sont
évitables (OKLab = deux matrices 3×3 et une racine cubique, qu'on remplace
par une LUT si le déterminisme l'exige). `tuile-tile-server` ne connaît ni la
couleur ni USD : il stocke des octets de type `other`. Aucun nom de marque dans
les types publics (`FieldSource`, pas `BingCorrection`).

## 6. Niveau de référence colorimétrique : l'heure et le soleil

L'harmonisation (§4–5) rend l'imagerie *cohérente*. Elle ne dit pas *sous
quelle lumière* elle doit apparaître. Or chaque tuile a été prise sous un soleil
à elle : ombres portées et ombrage de relief imprimés dans les pixels,
température de couleur, voile du jour de prise de vue. Et le rendu a son heure
à lui. Un vol rejoué à 19 h 30 en septembre au-dessus des Pyrénées doit avoir
un soleil rasant chaud venu de l'ouest. Il ne doit pas afficher des versants
ombrés au nord-ouest par un soleil de midi d'un autre jour.

### 6.1 Deux références, deux rôles

- **(a) Référence neutre, côté données, indépendante de l'heure.** L'imagerie
  est ramenée vers une *pseudo-albédo* : lumière d'acquisition retirée autant
  que raisonnable en basse fréquence. Cela couvre l'ombrage de relief, le voile
  additif et une balance des blancs ramenée au blanc de référence D65. Ce que
  le rendu éclairera ensuite, c'est cette albédo.
- **(b) Illumination cible, côté rendu, fonction de l'heure et du lieu.** Elle
  comprend la direction du soleil, la couleur et l'intensité du soleil
  transmis par l'atmosphère, l'éclairement du ciel (ambiant) et la perspective
  aérienne. Elle sort du **même modèle soleil et atmosphère que le rendu du
  ciel**. Viennent ensuite une adaptation chromatique de « caméra » (une
  fraction réglable de balance des blancs vers l'illuminant) et l'exposition.

**Composition : on plie (a) dans le champ d'harmonisation, on garde (b)
séparée.** Le dé-éclairage basse fréquence est, comme l'harmonisation, un champ
multiplicatif lent, défini par tuile, indépendant de l'heure. Il entre donc
dans le même `ColorField` (même point d'application, même clé, même cache,
même PNG de pack). L'heure, elle, ne doit **jamais** entrer dans une texture,
pour trois raisons :
- une texture par heure invaliderait le `TextureMemo`, les textures GPU
  partagées par tuile et la déduplication `(id, drape)` du pack à chaque frame
  d'un time-lapse ;
- le relief est éclairé par le rendu à partir des normales de la géométrie
  (lambert dans `shader.wgsl:137-140`, DistantLight dans Cycles), pas par la
  texture ;
- un pack cuit une fois doit servir pour toutes les heures. Le digest de scène
  du pack reste donc indépendant de l'heure, et l'heure est un paramètre du
  *rendu* (job, `videos/README.md`).

```
 octets bruts ─► ColorField(tuile) = harmonisation ⊙ dé-éclairage BF  [données, sans heure]
                         │  pseudo-albédo (sRGB 8 bits, PNG / texture partagée)
                         ▼
 SkyState(t, atmosphère) ─► ReferenceIllumination ─► éclairage de rendu  [rendu, avec heure]
   Sun::at_unix_seconds      soleil RGB, ciel RGB,     wgpu : uniforms sun/sky/air
   AtmosphereModel           direction, blanc,          USD  : DistantLight + ciel/dôme
   (partagé avec le ciel)    exposition                 Blender : même valeurs
                         ▼
             adaptation « caméra » + exposition ─► view transform (Standard)
```

### 6.2 Calcul

**Position du soleil.** `tuile-atmosphere::Sun::at_unix_seconds`
(`crates/tuile-atmosphere/src/sun.rs:58`) existe déjà : f64, basse précision,
écart annoncé inférieur à 0,01°, largement suffisant pour éclairer. Élévation et
azimut locaux = projection de `direction_ecef` dans le repère ENU du point
considéré, en f64. Les oracles de test sont le
[NREL SPA](https://docs.nrel.gov/docs/fy08osti/34302.pdf) (Reda & Andreas,
±0,0003°, cas de test publié : 17/10/2003 12:30:30, UTC−7, Golden CO, zénith
50,11162°, azimut 194,34024°) et les
[équations NOAA](https://gml.noaa.gov/grad/solcalc/calcdetails.html)
(d'après Meeus). La crate `spa` ou `solar-positioning` peut servir d'oracle en
`dev-dependencies` seulement.

**L'heure est une donnée, jamais l'horloge murale.** Elle vient des horodatages
de la trajectoire (`tuile-tape` porte des `sec/nsec` par frame) ou d'un `--at`
explicite. Le viewer utilise l'heure de la caméra (rejeu) ou une heure choisie.
Le soleil suit l'heure de chaque frame et bouge donc au cours d'un long vol.

**Illumination cible.** `SkyState { sun, atmosphere }` fournit, au point de
référence de la frame (origine de rendu ou cible de la caméra) :
- `sun_radiance` : transmittance le long du trajet solaire, soit le rôle que
  joue aujourd'hui `SkyShell::sunlight_reaching`
  (`crates/tuile-atmosphere/src/sky.rs:149`, orange au terminateur) ;
- `sky_irradiance` : l'ambiant du ciel sur une surface horizontale (idéalement
  une harmonique sphérique d'ordre 1 pour l'orienter), qui remplace la
  constante `params.x` du shader ;
- le blanc de l'illuminant, qui alimente l'adaptation chromatique partielle
  (facteur `adaptation` ∈ [0,1] : 0 = pellicule « lumière du jour », soleil
  couchant très orangé ; 1 = balance des blancs automatique, neutre).

La forme exacte de ces intégrales appartient au modèle d'atmosphère (Rayleigh +
Mie + ozone, diffusion multiple), qu'il soit précalculé à la
[Bruneton & Neyret 2008](https://ebruneton.github.io/precomputed_atmospheric_scattering/)
ou calculé par LUT légères à la
[Hillaire 2020](https://onlinelibrary.wiley.com/doi/abs/10.1111/cgf.14050).

**Dé-éclairage basse fréquence (référence a).** C'est la correction
topographique classique de la télédétection. La luminance d'un pixel est
régressée sur `cos i`, l'angle entre la normale du relief et le soleil
d'acquisition, et on divise par `(cos i + C)/(cos θz + C)` : C-correction de
Teillet 1982, ou ses variantes Minnaert et SCS+C
([Soenen 2005](https://www.semanticscholar.org/paper/SCS+C:-a-modified-Sun-canopy-sensor-topographic-in-Soenen-Peddle/c79888d882deb4119617a2ff4baab73956c3ea45),
[Minnaert amélioré](https://www.tandfonline.com/doi/full/10.1080/15481603.2015.1118976)).
Le `C` empirique représente la part diffuse et évite de surcorriger les
versants sombres. Spécificités tuile :
- le **soleil d'acquisition est inconnu** : Bing ne donne qu'une plage de dates.
  On l'estime par région et par lot de prise de vue, comme la direction `s`
  qui maximise la corrélation entre la luminance basse fréquence et `n·s`. Les
  normales viennent du terrain à un niveau **fixe** (déterminisme :
  géométrique, jamais « le maillage en cache ») ;
- on corrige uniquement à l'échelle du relief (la cellule du `ColorField`,
  quelques dizaines de mètres). Les ombres portées fines (arbres, bâtiments)
  restent : les retirer relève de l'image intrinsèque, hors périmètre ;
- confiance faible, c'est-à-dire corrélation faible ou relief plat, → pas de
  dé-éclairage, facteur 1.

**Voile et balance des blancs d'acquisition.** Ils sont déjà traités par les
offsets et les gains de l'harmonisation, surtout à l'étape 2 où la référence
Sentinel-2 est un composite médian. Il faut vérifier si la mosaïque de référence
est dérivée de L1C (non corrigée de l'atmosphère) ou de L2A : cela décide si la
référence est elle-même voilée.

### 6.3 Où cela vit

- **`tuile-atmosphere`** (dépend de `tuile-core`, `glam` et `bytemuck`, sans
  backend) porte `Sun`, `AtmosphereModel`, `SkyState` et
  `ReferenceIllumination`. Ce n'est pas `tuile-core`, parce que le core n'a pas
  à connaître le soleil et que la dépendance va déjà de l'atmosphère vers le
  core. Il faut ajouter `cargo check --target wasm32-unknown-unknown -p tuile-atmosphere`
  à la garde CI.
- **`tuile-core::radiometry`** ne voit que des nombres : `ColorField` inclut le
  terme de dé-éclairage, fourni par un `FieldSource` qui reçoit une direction
  d'acquisition estimée, pas un `Sun`.
- **Interface partagée avec le chantier ciel/atmosphère** (un autre agent la
  conçoit). Un seul soleil et un seul modèle d'atmosphère alimentent le rendu
  du ciel ET la référence couleur :

```rust
// tuile-atmosphere — contrat minimal, sans backend, f64
pub trait AtmosphereModel: Send + Sync {
    /// Transmittance du sommet de l'atmosphère jusqu'à `p`, dans la direction `to_sun`.
    fn sun_transmittance(&self, p_ecef: DVec3, to_sun: DVec3) -> [f64; 3];
    /// Éclairement du ciel reçu par une surface de normale `n` en `p` (SH L1 ou intégrale).
    fn sky_irradiance(&self, p_ecef: DVec3, n: DVec3, to_sun: DVec3) -> [f64; 3];
    /// (transmittance, luminance diffusée) entre l'œil et `p` : la perspective aérienne.
    fn aerial(&self, eye_ecef: DVec3, p_ecef: DVec3, to_sun: DVec3) -> ([f64; 3], [f64; 3]);
    /// Paramètres canoniques, pour le digest et l'export vers le ciel USD/Blender.
    fn params(&self) -> AtmosphereParams;
}
pub struct SkyState<'a> { pub utc_seconds: f64, pub sun: Sun, pub atmosphere: &'a dyn AtmosphereModel }
pub struct ReferenceIllumination {
    pub to_sun_ecef: DVec3, pub sun_rgb: [f32; 3], pub sky_rgb: [f32; 3],
    pub white: [f32; 3], pub adaptation: f32, pub exposure_ev: f32,
}
impl SkyState<'_> { pub fn reference_at(&self, p_ecef: DVec3) -> ReferenceIllumination { /* … */ } }
```

  Le rendu du ciel (dôme, limbe, horizon) et le sol consomment les **mêmes**
  `sun_transmittance`, `sky_irradiance` et `aerial`. C'est ce qui garantit que
  le sol à l'horizon et le ciel derrière lui ont la même couleur. Les
  `AerialPerspective` et `SkyShell` actuels deviennent une première
  implémentation de ce trait (forme fermée, diffusion simple).

- **wgpu.** `ViewUniform` porte déjà `sun_dir` et `air`. On ajoute `sun_rgb`,
  `sky_rgb` et `grade` (adaptation + exposition), et le fragment calcule
  `albédo × (sun_rgb·max(n·s,0) + sky_rgb)` puis `aerial_perspective`. C'est
  toujours affine dans l'albédo, donc la composition multi-passe reste exacte
  (`shader.wgsl:130-133`). Pour les plans globe entiers, où le terminateur est
  visible, `sun_rgb` doit varier par fragment : une LUT 1D élévation locale →
  transmittance, produite par le même modèle.
- **USD/Hydra/Cycles.** Le `DistantLight` du stage (angle, `color`,
  `intensity`) et le ciel (dôme ou ciel physique Blender) sont **générés par
  Rust** à partir de la même `ReferenceIllumination`, par exemple une
  sous-commande qui écrit le layer de look à la place de `make_look.py`. On ne
  les réinvente pas en Python. Il y a aujourd'hui **trois soleils** : `Sun` en
  Rust, une approximation NOAA à ~0,1° dans
  `integrations/hydra/tests/make_look.py`, et un soleil figé
  `rotation_euler = (0.7, 0.2, 0.3)` dans `integrations/blender/render_usd.py:243`.
  Il faut les réduire à un seul. L'adaptation et l'exposition vont dans le view
  transform et l'exposition Blender, déjà épinglés (`render_usd.py:58-80`).
  Limite connue : dans Cycles, un `DistantLight` a une seule couleur. Pour un
  plan à l'échelle d'un continent, c'est le volume d'atmosphère ou le ciel
  physique qui porte la variation.

### 6.4 Déterminisme

- Le soleil est calculé en f64 à partir de l'horodatage de la frame, puis
  quantifié (direction arrondie à 1e-9, couleurs en f32) avant d'entrer dans le
  digest du **rendu**, en même temps que `AtmosphereParams`, `adaptation` et
  `exposure_ev`.
- Le `ColorField` (dé-éclairage compris) reste dans le digest du **pack** ;
  l'illumination n'y entre pas.
- Les fonctions transcendantes (`sin`, `exp`) de l'illumination sont évaluées
  une fois par frame sur CPU, puis transmises comme nombres au shader et au
  stage USD. Aucune divergence possible entre wgpu et Cycles sur *quelle*
  lumière on demande. Il reste celle, inévitable, de *comment* chaque moteur
  l'intègre.
- Le dé-éclairage dépend de la direction d'acquisition estimée. Cette
  estimation est un produit versionné (couche du magasin ou paramètre du
  `CorrectionParams`), jamais recalculé implicitement au rendu.

### 6.5 Mesure

1. **Soleil** : écart avec le SPA ≤ 0,01° sur une grille d'instants et de
   lieux, dont le cas de test NREL. Égalité à 1e-6 près entre `sun_dir` wgpu,
   la direction du `DistantLight` relue dans le stage et le soleil Blender.
2. **Dé-éclairage** : pente de la régression luminance BF ~ `cos i` avant et
   après, qui doit tendre vers 0 (critère standard d'évaluation des
   corrections topographiques). Écart de luminance médiane entre versants
   nord et sud d'une même vallée, à couvert égal.
3. **Carte grise** : un patch synthétique d'albédo 18 % posé sur le globe,
   rendu de 06 h à 20 h. Sa couleur rendue suit `albédo × (sun_rgb·cos + sky_rgb)`
   du modèle, à ΔE près, en wgpu et en Cycles.
4. **Sol et ciel** : rapport de luminance et ΔE entre le sol lointain (après
   perspective aérienne) et le ciel juste au-dessus de l'horizon, comparé à la
   prédiction du modèle.
5. **Time-lapse A/B** : l'orbite pyrénéenne rendue à 08 h, 13 h et 19 h 30 avec
   le même pack (preuve que le pack est indépendant de l'heure : même digest,
   mêmes octets de texture), avec et sans dé-éclairage. Films dans `videos/`
   avec l'heure, les paramètres d'atmosphère et l'adaptation dans leur entrée
   README. Chaque film est ouvert.

## 7. Banc de tests, métriques et plan d'expérimentation

**Métriques (un outil `tuile-bake color-bench`, ou exemple dédié).**
1. **Contraste de couture** : pour chaque frontière de tuile d'imagerie visible
   (identifiée par une vue diagnostic « coord/niveau »), ΔE OKLab moyen entre
   deux bandes de 2 px de part et d'autre, divisé par le ΔE entre deux bandes
   parallèles décalées de 8 px à l'intérieur de chaque tuile. 1,0 = couture
   invisible ; séparer niveaux égaux et niveaux différents. CIEDE2000 en
   contrôle ([Sharma 2005](https://hajim.rochester.edu/ece/sites/gsharma/papers/CIEDE2000CRNAFeb05.pdf)).
2. **Erreur de pyramide** (sans rendu) : ΔE entre la tuile L+1 corrigée
   réduite ×2 et le quadrant du parent corrigé, sur toutes les tuiles d'une
   région, par niveau.
3. **Pop temporel** : ΔE moyen frame à frame sur les pixels dont le drapé
   change, orbite pyrénéenne, comparé aux pixels dont il ne change pas.
4. **Fidélité référence** : ΔE entre LP(corrigée) et Sentinel-2 à 40 m/px.
5. **Préservation du détail** : rapport d'énergie de la bande haute
   (image − LP) corrigée/brute, SSIM de la bande haute (le piège Mapbox).
6. **Budget** : ms/tuile 256² (stats, champ, apply), mémoire des champs.

**Tests automatiques (écrits avec le comportement, puis vérifiés en
revertant).** Fixtures **synthétiques** commitées (les tuiles Bing ne peuvent
pas l'être) : une texture procédurale découpée en pyramide, chaque tuile
altérée d'un gain/offset/voile connus ; la vérité terrain est l'originale.
- identité exacte au bit près quand les paramètres sont neutres ;
- récupération : ΔE(corrigée, vérité) < seuil, et régression si on coupe l'étape ;
- continuité : deux voisines issues d'une même source → ΔE de bord < ε ;
- propriété (`proptest`) : pour toute texture et tout champ borné, aucun texel
  non noir ne devient noir, alpha inchangé, sortie ∈ [0,255] ;
- déterminisme : même entrée → même hash, en parallèle et dans un ordre
  permuté ; `cargo check --target wasm32-unknown-unknown -p tuile-core` ;
- intégration réelle (non commitée, comme les tests sur bucket réel) : région
  Pyrénées lue dans le magasin, métriques 2–5 enregistrées en instantanés
  (`insta`) pour détecter les dérives.

**Benchmarks de performance.** `criterion` (ou `divan`) sur `apply`,
`TileStats`, construction du champ, version scalaire vs SIMD (`wide`/`pulp`)
vs compute shader ; cible < 0,5 ms/tuile/cœur, parité wasm mesurée dans
`tuile-web`.

**Rendus A/B.** Même trajectoire pyrénéenne, même pack sauf le digest de
correction : recorder wgpu et Cycles, correction off / étape 1 / étape 2.
Films dans `videos/` avec leur entrée `videos/README.md` (trajectoire, clé de
pack, digest de scène, asset d'imagerie, viewport, échantillons) — **et chaque
film ouvert**. Cas durs ajoutés : versant enneigé, littoral (eau), ville.

## 8. Crates et techniques utilisables

| Besoin | Choix | Remarque |
|---|---|---|
| Décodage/écriture | `image` 0.25 (déjà) | inchangé |
| Espaces couleur | code maison (LUT sRGB, OKLab) ; `palette` 0.7 pour les tests/outils | le core évite une dépendance pour 30 lignes |
| f16 | `half` 2.7 (déjà) | stockage des champs |
| Réduction d'image | `fast_image_resize` 6 (SIMD) côté outils | le core fait ses moyennes de blocs à la main (entiers) |
| SIMD portable | `wide` 1.7 / `pulp` 0.22 | wasm `simd128` à valider (règle 8 : justification écrite) |
| Parallélisme CPU | `rayon` côté bake/outils ; le core reste sur `offload` | pas de rayon dans le core |
| Moindres carrés creux (étape 3) | `faer` 0.24 (creux + Cholesky) ou `nalgebra-sparse` | offline uniquement, jamais dans le core |
| ΔE | `deltae` 0.3 / `empfindung` 0.2 | outils de mesure |
| SSIM | `dssim-core` 3.5, `image-compare` 0.5 | vérifier la licence de `dssim-core` avant intégration |
| Tests | `proptest`, `insta` | |
| Benchmarks | `criterion` 0.8 ou `divan` 0.1 | |
| GPU compute | `wgpu` 29 compute shaders WGSL ; WebGPU sur le web via le même code | réductions/histogrammes en **atomiques entiers** (`atomicAdd` u32) → résultat indépendant de l'ordre, donc déterministe ; pas d'atomiques flottants, pas de subgroups (non portables) |
| Stockage | `pmtiles` 0.24 via `tuile-tile-server` | couche `tile_type = other` |
| Position du soleil (oracle) | `spa` 0.5 ou `solar-positioning` 0.6, en `dev-dependencies` | le rendu garde `tuile_atmosphere::Sun` ; vérifier les licences |

**Compute shaders, quand ?** Le travail par tuile est minuscule (65 k texels,
quelques opérations) : le CPU dans le job offloadé suffit et reste
déterministe sans effort. Le compute devient intéressant (a) pour les
**métriques** sur frames rendues (couture, pop) sans readback complet,
(b) pour l'étape 3 sur une région entière, (c) si le profil wasm montre la
correction sur le chemin critique. Tout passage GPU reste soumis au test de
hash contre la référence CPU.

## 9. Risques et questions ouvertes

- **Halos** : une marche nette de prise de vue *à l'intérieur* d'une tuile
  produit un halo de la largeur d'une cellule. Pistes : filtre guidé /
  bilatéral pour LP, cellules plus petites, métadonnées de prise de vue Bing
  (`x-ve-tilemeta-capturedatesrange`, `vintageStart/End`) pour segmenter.
- **Saison et contenu** : neige, cultures, eau, chantiers diffèrent de la
  référence ; bornes + confiance, sinon on peint la neige en prairie.
- **Ancre de mauvaise qualité** : un niveau d'ancrage laid impose sa couleur à
  tout ce qui est plus fin ; choisir A par région, ou étape 2.
- **Dérive de chaîne** : erreurs cumulées sur 8–10 niveaux ; mesurer la
  métrique 2 par profondeur.
- **Licences** : dérivés Bing (validation juridique), NC-SA de Sentinel-2
  2018+, persistance des champs dérivés (les faire expirer avec les epochs de
  la couche source ?).
- **Identité de drapé** : `DefaultHasher` non garanti stable ; la migrer vers
  un hachage spécifié avant d'y ajouter la correction.
- **Invalidation** : tout pack existant est invalidé par le digest ; prévoir
  `--no-color-correction` pour reproduire les anciens films.
- **Look vs données** : ne pas laisser un réglage artistique glisser dans
  l'harmonisation, qui doit rester neutre, mesurable et identique en wgpu et
  en Cycles.
- **Soleil d'acquisition inconnu** : l'estimation par corrélation au relief échoue sur terrain plat ou couvert hétérogène ; sans confiance, pas de dé-éclairage (facteur 1), jamais de surcorrection qui noircirait un versant.
- **Double ombrage** : tant que le dé-éclairage n'existe pas, le lambert du rendu s'ajoute à l'ombrage imprimé ; à midi c'est tolérable, au soleil rasant opposé c'est faux. Un réglage transitoire peut atténuer le lambert sur l'imagerie.
- **Frontière avec le chantier ciel** : si le modèle d'atmosphère change, toutes les illuminations changent. Le trait `AtmosphereModel` est le contrat, et ses `params()` entrent dans le digest de rendu.
