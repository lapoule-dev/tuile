<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Proxy GPU : des couches de tuiles dérivées par des modèles

Note d'architecture exploratoire, rédigée le 2026-10-14. **Rien n'est
implémenté.** Elle décrit comment calculer des tuiles améliorées par des réseaux
de neurones sur un serveur GPU, à la demande, une seule fois, puis les servir
comme n'importe quelle couche :

- super-résolution au-delà du niveau maximal ;
- retrait des nuages et du voile ;
- déblocage JPEG ;
- plus tard, la 3D crédible.

Elle s'appuie sur ce que `tuile-tile-server` fait déjà et dit ce qu'il faudra
**mesurer** avant de s'engager.

Notes liées : [`imagery-tile-enhancement.md`](imagery-tile-enhancement.md) (les
traitements),
[`webgpu-neural-networks.md`](webgpu-neural-networks.md) (ce qui tient côté
client), [`onnx-webgpu-wgpu.md`](onnx-webgpu-wgpu.md) (les runtimes),
[`3d-from-imagery-tiles.md`](3d-from-imagery-tiles.md) (le client lointain de ce
proxy).

---

## 0. En une page

- **Le principe.** Une *couche dérivée* est une couche dont chaque tuile est une
  fonction pure de :
  - tuiles source : la tuile, une marge prise sur ses voisines, son parent, et
    éventuellement d'autres dates ;
  - un modèle versionné et ses paramètres.

  Elle est calculée **à la première demande**, stockée, puis servie à tous.
- **Le branchement existe déjà.** Pour `tuile-tile-server`, le worker GPU est un
  **`Upstream`** comme un autre : `TemplateUpstream` vers
  `http://worker/{z}/{x}/{y}` suffit pour une v0. Le magasin, les epochs, le
  cache mémoire, les absences et le partage entre instances (`peers`)
  s'appliquent tels quels. **Le serveur de tuiles ne connaît ni wgpu ni aucun
  framework de réseaux de neurones.**
- **Le client ne dépend jamais du proxy.** Il affiche la tuile brute, demande la
  dérivée, et échange quand elle arrive : créer, activer, puis désactiver
  l'ancienne. Jamais de noir, jamais d'attente bloquante.
- **Ce que ça débloque par rapport au tout-navigateur :**
  - les modèles lourds : super-résolution générative, inpainting des nuages
    épais ;
  - les composites multi-dates calculés près des archives ;
  - un calcul payé une fois pour tous les utilisateurs ;
  - les clients sans WebGPU (Linux, mobiles, vieux GPU) servis aussi ;
  - des octets identiques, donc des films reproductibles.
- **Ce que ça coûte :**
  - la latence d'une tuile pas encore calculée ;
  - les GPU et le stockage ;
  - surtout, **une couche dérivée stockée est une œuvre dérivée persistante**.
    Elle est réservée aux sources ouvertes ou propres (BD ORTHO, Sentinel-2,
    NAIP…). Pour les sources commerciales, le traitement dans le navigateur, qui
    ne stocke rien, reste la seule voie défendable.
- **À mesurer avant de s'engager** (§7) : tuiles par seconde par GPU et par
  modèle, taux de succès du cache sur de vrais vols, latence d'une tuile non
  calculée, octets par tuile dérivée.

---

## 1. Trois étages, un repli commun

| Étage | Traitements | Pourquoi là | Sources |
|---|---|---|---|
| **Navigateur** (wgpu/WebGPU) | anisotropie, mipmaps linéaires, dé-voilage en shader (canal sombre, HOT), petite super-résolution par régression (moins de 1 M de paramètres) | gratuit ; rien n'est stocké ; une tuile traitée une fois à son arrivée, pas à chaque frame | **toutes**, commerciales comprises (pixels modifiés en mémoire client) |
| **Proxy GPU** | super-résolution générative en une étape, déblocage JPEG, comblement des nuages, composites multi-dates, champs de couleur appris, puis la 3D crédible | lourd ; payé une fois ; mis en cache ; octets déterministes | **ouvertes ou propres seulement** |
| **Ferme** (`tuile-bake`) | précalcul le long d'une trajectoire de film connue | coût connu à l'avance ; même pack → mêmes octets | idem |

**Le repli commun : la tuile brute**, toujours résidente côté client. Tout
étage peut échouer, être lent ou absent (Linux sans WebGPU, proxy surchargé)
sans jamais produire de trou.

---

## 2. Où ça se branche dans le code existant

### 2.1 Le serveur de tuiles ne change pas (v0)

`tuile-tile-server` sert des couches. Chaque couche a **une source, une grille,
un format et une durée de vie** (`layer.rs`). Elle est découpée en zones et en
epochs. Une tuile absente du magasin est demandée **une fois** à l'amont, puis
stockée (`service.rs`) :

```rust
// crates/tuile-tile-server/src/upstream.rs
pub trait Upstream: Send + Sync {
    async fn fetch(&self, level: u8, x: u32, y: u32) -> Result<Option<Bytes>, UpstreamError>;
}
```

Une couche dérivée est donc une couche **dont l'amont est le worker GPU** :

```
client ──► tile-server [couche « bdortho.sr2x-v3 »] ──(absente)──► Upstream
                                                                   = worker GPU
           tile-server [couche « bdortho »] ◄── tuile, marge, parent ──┘
```

- **v0** : `TemplateUpstream` pointe vers `http://worker/sr2x-v3/{z}/{x}/{y}`. Le
  worker lit ses entrées (tuile, voisines pour la marge, parent) dans la couche
  source du même tile-server, calcule, puis renvoie des octets ou `404`. Les
  absences sont mises en cache comme pour toute source.
- **Ce qui vient gratuitement** : stockage PMTiles, compaction, cache mémoire
  des tuiles chaudes, partage entre instances pendant qu'une tuile n'est pas
  encore dans le magasin (`Source::Shared`, utile quand deux clients demandent la
  même tuile froide), indexation, et projection d'une scène vers des archives
  locales pour la ferme.
- **Ce qui manque peut-être** : un délai par requête plus long que pour une
  source HTTP classique, et la limitation des requêtes simultanées vers le
  worker (une file bornée par GPU). C'est à voir après mesure.

### 2.2 Le worker GPU est un service à part

- **Hors du workspace livré** au départ : un service Python (PyTorch, ORT CUDA)
  suffit pour la v0 de recherche. Une v1 en Rust pourrait utiliser `ort` avec
  CUDA ou TensorRT (cf. [`onnx-webgpu-wgpu.md`](onnx-webgpu-wgpu.md) §6), ou Burn
  sur wgpu/CUDA.
- **Il ne connaît que** « (couche dérivée, z, x, y) → octets ». Il lit ses
  entrées par HTTP sur le tile-server et ne stocke rien lui-même.
- **Le règles du dépôt sont respectées** : rien n'entre dans `tuile-core` (règle
  1), le serveur de tuiles ne dépend d'aucun framework ML, et aucun nom de marque
  n'apparaît dans les types publics (règle 3).

### 2.3 Côté client

- Deux couches sont demandées pour une même zone : la **brute** (toujours) et la
  **dérivée** (si configurée).
- La dérivée n'est publiée qu'une fois **résidente** sur le GPU, puis l'ancienne
  est désactivée. Un chevauchement d'une frame est permis, jamais un trou.
- Si la dérivée tarde au-delà d'un délai ou échoue, on garde la brute. La tuile
  est redemandée plus tard ; le serveur l'aura peut-être calculée entre-temps.
- **Préchargement** : demander les dérivées le long de la trajectoire prévue de
  la caméra, pour lisser la latence des tuiles froides.

---

## 3. Nommage, versions, déterminisme, provenance

- **Une couche dérivée par (source, transformation, version)**, par exemple
  `bdortho.sr2x-v3` ou `s2.cloudless-median-2025q2`. La version inclut le digest
  des poids et des paramètres. **Changer de modèle, c'est une nouvelle couche ou
  un nouvel epoch, jamais une réécriture en place.**
- **Déterminisme** : l'inférence GPU n'est pas reproductible au bit près d'un
  matériel à l'autre, mais **les octets stockés le sont**. Les films lisent la
  couche stockée, donc même pack → mêmes octets, comme avec les epochs actuels.
- **Provenance** : chaque couche porte dans le catalogue son type :
  - *restaurée* : déblocage, voile ;
  - *composée* : composite multi-dates, rien d'inventé ;
  - *générée* : super-résolution générative, inpainting.

  Une couche « générée » peut être accompagnée d'une couche **masque** (pixels
  inventés) pour le client qui veut le montrer. Cf. le principe de provenance
  dans [`imagery-tile-enhancement.md`](imagery-tile-enhancement.md) §2.
- **Cohérence entre niveaux** : toute super-résolution du proxy respecte la
  contrainte « l'enfant réduit redonne le parent », par une projection de type
  CEM après le réseau. **Les raccords** se traitent par une marge fixe lue sur
  les voisines. Une tuile reste une fonction pure d'un voisinage fixe, même si
  l'ordre des demandes varie.

---

## 4. Ce que le proxy rend possible, par ordre d'intérêt

1. **Super-résolution au-delà du niveau maximal.** Régression (HAT, NAFNet,
   Real-ESRGAN compact) ou générative en une étape (OSEDiff, PiSA-SR,
   Apache-2.0), avec projection de cohérence. Entraînement sur des paires
   (z, z+1) de la BD ORTHO.
2. **Composites multi-dates sans nuages.** Médiane ou meilleur pixel sur N dates,
   près des archives. **Rien n'est inventé.** C'est la vraie réponse aux nuages
   épais.
3. **Déblocage JPEG** (QGAC, FBCNN) avant toute super-résolution.
4. **Comblement des nuages épais quand une seule date existe.** Inpainting
   (LaMa, diffusion), marqué *généré* avec son masque.
5. **Champ de couleur appris**, prolongement de `tuile-radiometry`, calculé une
   fois par zone.
6. **Plus tard, la couche 3D crédible** : hauteur au-dessus du sol, emprises,
   toits, servie en 3D Tiles (cf.
   [`3d-from-imagery-tiles.md`](3d-from-imagery-tiles.md)).

---

## 5. Licences

- **Couche dérivée stockée = œuvre dérivée persistante.** Les conditions des
  fournisseurs commerciaux interdisent en général copie, cache et dérivation
  (cf. [`3d-from-imagery-tiles.md`](3d-from-imagery-tiles.md) §5). **Le proxy ne
  sert que des sources dont la licence le permet** : BD ORTHO et données IGN
  (Etalab 2.0), Sentinel-2, NAIP, SWISSIMAGE, ou l'imagerie de l'opérateur.
- **Les poids des modèles** ont leur propre licence : SUPIR et FluxSR sont non
  commerciaux ; les modèles fondés sur Stable Diffusion sont sous OpenRAIL. Ils
  ne vivent jamais dans le dépôt ; le worker les charge au déploiement.
- **Pour les sources commerciales**, seul l'étage navigateur s'applique.

---

## 6. Coûts : un modèle à remplir par la mesure

Toutes les valeurs ci-dessous sont **des inconnues à mesurer**, pas des
estimations à croire.

```
GPU nécessaires ≈ (tuiles neuves / s) × (1 − taux de succès du cache)
                  ÷ (tuiles / s / GPU pour le modèle)
```

- **Tuiles neuves par seconde** : un vol interactif en demande quelques
  dizaines. Un film en connaît la liste à l'avance.
- **Tuiles par seconde par GPU** : dépend énormément du modèle. Une régression
  légère en traite beaucoup par seconde ; une super-résolution générative en une
  étape, de l'ordre de quelques unités sur un GPU de classe L4. C'est un ordre
  de grandeur tiré des chiffres publiés, **à mesurer**.
- **Le levier principal est le cache**, partagé par tous les clients. Les zones
  populaires sont calculées une fois. Le préchargement le long des trajectoires
  déplace le calcul hors du chemin critique.
- **Limiter le génératif** aux niveaux au-delà de la résolution de la source, et
  faire tout le reste en régression : la facture change d'ordre de grandeur.
- **Stockage** : environ une couche source de plus par couche dérivée (WebP ou
  AVIF), sur les seules zones visitées.

---

## 7. Ce qu'il faut mesurer avant de s'engager

| Mesure | Comment | Ce que ça tranche |
|---|---|---|
| Tuiles par seconde par GPU, par modèle | banc du worker sur une L4 de la ferme, tuiles 256² et 512², par lots | le nombre de GPU |
| Latence d'une tuile froide (p50, p95) | de la demande client à la réponse, amont + inférence + encodage | le délai d'échange et le préchargement |
| Taux de succès du cache | rejouer de vrais vols (bandes caméra de `tuile-tape`) contre la couche dérivée | le coût réel |
| Octets par tuile dérivée | WebP et AVIF à qualité perçue égale | le stockage et la bande passante |
| Raccords et cohérence parent/enfant | métriques de [`imagery-tile-enhancement.md`](imagery-tile-enhancement.md) §6, plus un film | la qualité visible |
| Taux de repli côté client | part des tuiles affichées brutes au-delà du délai, sur un vol | le ressenti |

Chaque mesure se termine par **un film regardé** (règle de `CLAUDE.md`).

---

## 8. Questions ouvertes

- **Marge et parent** : le worker lit 9 tuiles (la tuile et ses 8 voisines) plus
  le parent pour chaque tuile dérivée. Faut-il un appel groupé côté tile-server ?
- **Contre-pression** : quand le worker est saturé, le tile-server doit répondre
  vite « pas encore » pour que le client garde la brute, plutôt que d'attendre.
  Code de réponse et sémantique à définir ; aujourd'hui `Ok(None)` veut dire
  « la source n'a pas cette tuile », ce qui n'est pas la même chose.
- **Invalidation** : quand la source change d'epoch, les dérivées de la zone
  doivent suivre. Une couche dérivée référence-t-elle l'epoch de sa source ?
- **Composites multi-dates** : une couche qui dépend de N couches source, ou de
  N epochs d'une même couche.
- **Placement** : le worker près du magasin (même région), pour que la lecture de
  la marge ne coûte pas plus que l'inférence.
