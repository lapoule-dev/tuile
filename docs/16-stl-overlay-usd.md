# 16 — La couche de vol STL en OpenUSD

Comment répliquer, par composition côté stage, la couche de visualisation de
vol de SportsTrackLive — telle qu'elle existe dans le client Unity (qui est
aussi le player web : le « viewer Cesium » du site charge le même build WebGL
via `CesiumManagerWrapper._attemptInitPlayer`). Étude du 2026-09-08 sur
`sportstracklive-unity` et `sportstracklive-rails`.

Doctrine inchangée : le globe vient du procédural TuileGlobe ; **tout ce qui
est propre au vol (track, rideau, badges, labels) est du contenu de stage**,
écrit par `tuile-usd` dans le manifeste ou une sublayer — de la composition,
pas du code C++.

## 1. Inventaire — ce que fait le client aujourd'hui

| Élément | Source de données | Construction Unity | Fichiers |
|---|---|---|---|
| **Track orange** | `TrackPoint[]` (lon/lat/alt/date/speed/bearing) servis par rails ; côté services, le Parquet du pipeline `stl-3d-rendering` | Ruban **face caméra** (`CustomLineMesh`) : `offset = Cross(dir, camForward) × width × camDist` → **largeur constante à l'écran** ; matériau `_StartColor/_EndColor` + `_Float` = fondu de queue (~30 derniers points) ; clamp au sol des points anciens (`CoordToWorldPosAlwaysAboveGround`) | `Assets/Scripts/PlayerTrack.cs:423`, `Assets/Scripts/Tools/CustomLineMesh.cs:142` |
| **« Ombre » au sol** | positions des ≤ 30 derniers points + hauteur terrain (`WorldPosGroundLevel`, requête de hauteur sur le quadtree) | Ce n'est **pas une ombre projetée** : un **rideau vertical** (triangle strip) entre la position aérienne et le sol, alpha dégradé 1→0 le long de la queue (vertex colors) | `PlayerTrack.cs:521` (`SetShadowMeshData`), `:552` (UVs/alphas) |
| **Badge pilote** (barre verticale + nom + avatar) | nom + avatar (URL) + couleur user | Canvas world-space **billboard** (`canvas.worldCamera`), TMPro + images ; variantes simple/détaillée ; stats alt/vitesse dans la variante détaillée | `Assets/Scripts/Markers/PlayerUIMarker.cs`, `UIMarkerBase.cs` |
| **Vitesse « 14.5 km/h »** | `TrackPoint.speed` interpolé (`GetCoordLerp`) | HUD écran (UI), pas un objet 3D | `PlayerTrack.cs:377`, UI |
| **Localités / sommets « [841m] »** | API `v1/home/points_of_interest` : table `PointsOfInterest` (extrait OSM via job Overpass, tri `osm_type`/`population`/`altitude`, limite 150+150), repli `Locality` (base), balises | Canvas billboards avec fade-in, dé-chevauchement et clustering côté client ; libellé `nom [ele m]` | `Assets/Scripts/MarkerController.cs:780`, `POIUIMarkerBase.cs`, rails `app/controllers/api/v1/home_controller.rb:54` |
| **Curseur/modèle 3D** | position interpolée + bearing | mesh coloré (paraglider), échelle ∝ distance caméra (`SetModelSize`) | `PlayerTrack.cs:119` |

## 2. Le mapping OpenUSD, élément par élément

Principe transversal : **le writer connaît la caméra de chaque frame** (la
tape). En batch, tout ce qui est « face caméra » ou « constant à l'écran »
n'est pas un problème de runtime, c'est une **donnée calculable à
l'écriture** : billboards = timeSamples d'orientation, largeur écran-
constante = timeSamples de `widths`. Zéro shader, zéro code hôte.

Et le pilier précision : chaque position (track, POI, rideau) est rebasée sur
**le même `renderOrigin`** que la caméra du manifeste, soustraction en f64
dans le writer — jamais d'ECEF brut en f32 (règle n° 7 du projet).

- **Track** — `UsdGeomBasisCurves` `type="linear"`, `points` rebasés,
  `primvars:displayColor` (couleur user) et `primvars:displayOpacity`
  *varying* pour le fondu de queue. Le curseur de replay : `points` et
  `curveVertexCounts` en **timeSamples** (la courbe grandit frame par
  frame, comme la caméra — un sample par frame, même horloge) ; ou, moins
  lourd, courbe complète + fenêtre d'opacité animée. Largeur : `widths` en
  timeSamples calculés par le writer (`k × distance(caméra_t, point)`) pour
  répliquer la constance à l'écran du ruban Unity. Storm rend les
  BasisCurves nativement.
- **Rideau au sol** — `UsdGeomMesh` généré par le writer : paires de
  sommets (aérien, sol) sur la fenêtre des ~30 derniers points,
  `displayOpacity` *varying* dégradé, timeSamples sur `points` pour suivre
  le curseur. **Dépendance : les hauteurs terrain.** Le writer les obtient
  hors-ligne à la génération via `TerrainHeights` (déjà renvoyé par
  `globe_on` côté `tuile-planetary`) — un nouveau binaire « track-to-
  overlay » qui ouvre les sources ion (token en env) et échantillonne la
  hauteur sous chaque point. Pas de requête au rendu : le stage est
  autonome, reproductible ferme.
- **Badge pilote** — pas de schéma texte en USD. Un **générateur de
  cartes** (Rust, crate `image` déjà dans l'arbre) produit un PNG par
  pilote : barre verticale couleur user + nom vertical + avatar rond.
  Dans le stage : un quad `UsdGeomMesh` (UsdPreviewSurface + UsdUVTexture,
  `opacityThreshold` pour la découpe), **billboardé par timeSamples de
  xform** (orientation vers la caméra de la frame, cuite par le writer),
  ancré au point courant (timeSamples de translation), échelle ∝ distance
  (répliquant `SetModelSize`).
- **Vitesse (HUD)** — hors stage. C'est de l'écran, pas du monde : au
  montage, `ffmpeg drawtext` par segment (les valeurs sortent du même
  Parquet), ou plus tard le compositor Blender sur le chemin ferme. Mettre
  un HUD dans le stage (quad parenté caméra) reste possible mais n'apporte
  rien en batch.
- **Localités / sommets** — même API rails que le client
  (`points_of_interest` sur la bbox du trajet, tri population/altitude,
  plafond N ≈ 20 pour un shot) interrogée **à la génération** ; le
  générateur de cartes fabrique un PNG par label (`nom` / `nom [ele m]`) ;
  dans le stage : quad billboardé (comme le badge, xform timeSamples) + un
  jalon vertical `BasisCurves` 2 points (sol → label). Le
  dé-chevauchement/clustering dynamique du client ne se réplique pas : en
  batch on **choisit** les N labels du shot à l'écriture (tri +
  espacement minimal en écran, calculable puisque la caméra est connue).
- **Curseur 3D** — un petit mesh (delta-aile stylisée) dans le manifeste,
  translation/orientation en timeSamples depuis le Parquet (position
  interpolée + bearing, la même interpolation que `GetCoordLerp`).

## 2 bis. La voie glTF : importer, puis composer

Précision de Laurent : certains éléments peuvent être **réalisés en glTF**
(ou récupérés tels quels) puis importés en USD et manipulés par composition —
plutôt que re-modélisés en USD natif.

**Ce qui existe déjà côté Unity** (aucun glTF ; des FBX utilitaires) :
`Assets/Models/STL 3D Arrow.fbx` (25 K), `STL Target Circle.fbx` (37 K),
`3D RightArrow.fbx`, `Arrow-centered.obj`, `TourEiffel.fbx` (193 K) ; le
curseur pilote est un mesh de prefab (`Assets/Prefabs/Markers/Player
Track.prefab`, nœud `Cursor`), les badges/labels sont des canvases UI sans
asset 3D. Un vrai modèle de parapente serait donc un asset **nouveau** — et
glTF est le bon format d'échange pour le commander/produire.

**Trois voies d'import, évaluées :**

| Voie | État chez nous | Verdict |
|---|---|---|
| (a) Plugin `SdfFileFormat` glTF (Adobe usd-fileformat-plugins / guc) : `references = @aile.glb@` **en direct** dans le manifeste | Absent de notre 26.08 local et du 25.08 pxrBlender ; à compiler contre **deux ABI** | Élégant mais coûteux — plus tard, si le va-et-vient d'assets devient quotidien |
| (b) **Conversion hors-ligne** glTF/FBX → `.usdc` committé, référencé par le stage | **Disponible aujourd'hui** : Blender 5.1 local (import glTF/FBX natif + export USD) ; le `.usdc` est un asset versionné, relu par usdview 26.08 ET le fork 25.08 sans plugin | **Recommandée** — un script `assets/convert-gltf.sh` (blender -b --python) rend la conversion reproductible |
| (c) Import Blender natif sur la ferme (le job importe le `.glb` dans la scène Blender) | Marche, mais ne sert que le chemin Blender — usdview/usdrecord ne le voient pas | Non : casse la parité des deux hôtes |

**Le pattern de composition** (l'asset n'est JAMAIS édité) :

```usda
def Xform "Cursor" (
    prepend references = @assets/paraglider.usdc@
)
{
    # Placement : rebasé sur LE renderOrigin du manifeste, f64 au writer.
    matrix4d xformOp:transform.timeSamples = { 1: (...), 2: (...), ... }
    uniform token[] xformOpOrder = ["xformOp:transform"]

    # Le look par over — la couleur user sans toucher au glTF converti :
    over "Materials" {
        over "Wing" {
            over "Preview" {
                color3f inputs:diffuseColor = (1.0, 0.45, 0.12)
            }
        }
    }
}
```

S'anime **côté stage** : position/orientation le long de la track
(timeSamples écrits par le writer, bearing compris), échelle ∝ distance
caméra, overs de matériaux (couleur user, opacité). Reste **dans l'asset** :
la géométrie, les UVs, la hiérarchie des matériaux, les éventuelles
animations squelettiques internes (hors périmètre v1). Le prototype
`integrations/hydra/tests/overlay.usda` démontre le pattern avec
`cursor.usda` en doublure d'un glTF converti.

## 3. Découpage du travail

1. **`tuile-usd`** : le writer gagne un module `overlay.rs` — BasisCurves
   (track + jalons), mesh rideau, quads billboardés, tout en texte comme
   `stage.rs`, tout rebasé sur le `renderOrigin` du manifeste. Entrées :
   Parquet de track (le format du pipeline `stl-3d-rendering`), JSON POI,
   la tape caméra (pour billboards/widths). Sortie : une **sublayer**
   `overlay.usda` référencée par le manifeste — le globe et le vol restent
   deux couches composables.
2. **Générateur de cartes** (`tuile-usd/src/bin/label-cards.rs` ou module) :
   PNG badges + labels (crate `image` ; fontes : embarquer une fonte libre,
   pas de dépendance système). Les avatars se téléchargent à la génération
   (URL API), jamais au rendu.
3. **`track-to-overlay`** (binaire) : Parquet + tape + POI → hauteurs
   terrain (`TerrainHeights`) → `overlay.usda` + cartes. En ligne à la
   génération, hermétique au rendu.
3 bis. **Assets glTF** : `assets/` versionné (licences notées, règle du
   repo) + `convert-gltf.sh` (Blender headless → `.usdc`) ; premier client :
   le modèle de parapente du curseur, puis les FBX utilitaires existants
   (flèches, cercle cible) si les shots contest en ont besoin.
4. **hdCycles (plus tard)** : vraies ombres portées du curseur/de la track
   (le rideau STL n'est pas une ombre — il passe sur Storm dès maintenant),
   DoF, éclairage physique du look docs/15.

## 4. Risques nommés

- **Storm et l'opacité des courbes** : `displayOpacity` sur BasisCurves est
  honoré par Storm, mais le tri de transparence est basique — le rideau
  (mesh alpha) derrière la track (courbe alpha) peut popper selon l'angle.
  Parade : rideau très transparent (α ≤ 0,35, comme Unity) et track opaque
  sauf la queue.
- **Volume des timeSamples** : widths + xforms billboards par frame sur
  6 min × 60 fps = ~21 600 samples par attribut. Texte `.usda` volumineux
  (dizaines de Mo) — prévoir `.usdc` via `usdcat` en post-génération si ça
  pèse, sans rien changer au writer.
- **Le même renderOrigin, sinon rien** : une track rebasée sur un autre
  origin que la caméra jitter exactement comme l'ECEF brut. Le writer doit
  refuser de composer deux couches aux origins différents (assert à la
  génération).
- **Billboards vs scrub interactif** : les orientations cuites ne valent
  que pour la caméra de la tape. En usdview interactif (caméra libre), les
  badges regardent ailleurs — assumé : la couche est faite pour le batch ;
  l'interactif restera le viewer wgpu.
- **POI : dépendance API à la génération** — bbox sans réseau = pas de
  labels. Le générateur doit dégrader proprement (overlay sans localités,
  warning, pas d'échec).
