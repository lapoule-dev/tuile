<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright lapoule.dev -->

# Formes structurées à partir de tuiles maillées photoréalistes — exploration

Note de recherche exploratoire, rédigée le 2026-10-14. **Rien n'est implémenté
et rien n'est proposé à l'implémentation ici.** Le document cadre la question,
passe en revue la littérature, liste les jeux de données de référence et dit ce
qu'une expérience honnête demanderait. Il ne suit pas la feuille de route
(`docs/03-roadmap.md`) : c'est une piste post-v1.

## 0. Le verrou à lire en premier : la licence de la source

L'idée de départ est de partir des tuiles photoréalistes d'un grand fournisseur
commercial. Ses règles d'usage interdisent **précisément** cette idée. La page
des règles de l'API de tuiles dit en substance qu'on peut superposer ses propres
objets 3D aux tuiles photoréalistes *à condition que ces objets ne soient pas
extraits, décalqués ni dérivés de quelque manière que ce soit, à la main ou par
machine, de ces tuiles*. Les conditions générales ajoutent l'interdiction
d'exporter, d'extraire ou de « scraper » le contenu pour un usage hors du
service, et encadrent strictement la mise en cache.
([règles de l'API](https://developers.google.com/maps/documentation/tile/policies),
[présentation des tuiles 3D](https://developers.google.com/maps/documentation/tile/3d-tiles),
[fil communautaire citant les CGU](https://community.cesium.com/t/export-google-3d-tiles-for-unreal/38007)).
Je n'ai pas pu ouvrir la page des règles depuis ce conteneur, qui n'a pas de
résolution DNS pour ce domaine. Le texte ci-dessus est une paraphrase tirée des
résultats de recherche. **Relire le texte en vigueur avant toute décision.**

Conséquences pour Tuile :

- Un outil qui produirait des bâtiments à partir de ces tuiles-là serait, par
  construction, un outil pour enfreindre les conditions du fournisseur. Ça va
  aussi contre `docs/00-vision.md` (« pas de données de services propriétaires
  dans le dépôt, les fixtures ou les exemples »).
- **La technique, elle, ne dépend pas de la source.** Le même pipeline
  s'applique à n'importe quel maillage photogrammétrique texturé servi en
  3D Tiles. Il existe des sources ouvertes de ce type (§4) : le maillage texturé
  de Helsinki, les maillages de villes publiés en 3D Tiles sous licence ouverte,
  et la photogrammétrie de l'utilisateur lui-même. La trajectoire saine est donc :
  **développer et évaluer sur des maillages ouverts**, puis laisser à
  l'utilisateur final la responsabilité de la source qu'il branche, comme pour
  tout client 3D Tiles.
- L'affichage, lui, reste permis : superposer des formes *indépendantes*
  (issues d'un cadastre ouvert ou d'un LiDAR, voir §4) au maillage photoréaliste
  est l'usage prévu par ces mêmes règles, avec l'attribution des sources.

La suite du document parle donc de « maillage photogrammétrique texturé » en
général.

## 1. Ce que veut dire « transformer en formes »

Une tuile photoréaliste est une **soupe de triangles avec une texture
cuite** : pas d'objets, pas de sémantique, pas de plans exacts. L'éclairage du
jour de la prise de vue est imprimé dans les texels (ombres portées, soleil
rasant, saturation). « Transformer en formes » recouvre au moins trois cibles
de difficulté croissante :

| Cible | Sortie | Usage | Difficulté |
|---|---|---|---|
| **A. Segmentation** | chaque face étiquetée : sol / bâtiment / végétation / eau / véhicule | masquer, recolorer, filtrer, *classification* 3D Tiles (`EXT_mesh_features`) | maîtrisée (SUM, ~80 % mF1) |
| **B. Abstraction géométrique** | bâtiments LoD1/LoD2 : prisme ou polyèdre à toits plans, étanche, quelques dizaines de faces | CityJSON/CityGML, simulation (soleil, vent, énergie), occlusion, LOD lointain, USD | recherche active, bons outils sur LiDAR |
| **C. Réinterprétation** | modèle procédural (programme de règles) ou modèle LoD3 avec ouvertures, matériaux PBR délités | édition, re-stylisation, rendu ré-éclairable | front de recherche |

Le **délitage** (retrait des ombres et de l'éclairage cuit) traverse les trois
cibles. Il sert à l'apparence finale (texture albédo ré-éclairable). Il sert
aussi à la *géométrie* : une ombre portée est un faux bord pour tout détecteur
fondé sur l'image, et une face à l'ombre trompe les classifieurs qui utilisent
la couleur.

## 2. État de l'art

### 2.1 Segmentation sémantique de maillages urbains texturés (cible A)

- **SUM, Semantic Urban Meshes** (Gao, Nan, Boom, Ledoux, *ISPRS J.* 2021) :
  le banc d'essai de référence. Environ 4 km² du maillage texturé de Helsinki
  (photogrammétrie oblique, 2017), six classes (sol, végétation, bâtiment, eau,
  véhicule, bateau), un outil d'annotation ouvert et une évaluation des méthodes
  de l'époque.
  ([projet](https://3d.bk.tudelft.nl/projects/meshannotation/),
  [arXiv 2103.00355](https://arxiv.org/abs/2103.00355))
- **Transformers sur maillages texturés non-variétés** (Heidarianbaei,
  Mehltretter, Rottensteiner, 2026) : les pixels de texture de chaque face sont
  donnés au modèle avec la géométrie. Résultats rapportés sur SUM : 81,9 % mF1
  et 94,3 % OA. C'est le bon type d'entrée pour nous : les maillages
  photogrammétriques ne sont *pas* des variétés propres, et la plupart des
  réseaux sur maillages supposent qu'ils le sont.
  ([arXiv 2604.01836](https://arxiv.org/abs/2604.01836))
- Précédent : CNN 1D multi-branches sur descripteurs de faces, environ 80 % OA
  (*ISPRS Annals* 2019). Les auteurs y notent que *le maillage est le livrable
  de fait* des chaînes photogrammétriques.
  ([lien](https://isprs-annals.copernicus.org/articles/IV-2-W7/175/2019/))

**Lecture.** La cible A est atteignable avec l'état de l'art. La difficulté
pratique est le *transfert de domaine* : on n'entraîne pas sur ces tuiles, et
leur texture, leur densité et leur bruit diffèrent de ceux de Helsinki.

### 2.2 Abstraction : du maillage brut au modèle de ville (cible B)

**Méthodes qui partent directement d'un maillage multi-vues.**

- **LOD Generation for Urban Scenes** (Verdie, Lafarge, Alliez, *ACM TOG* 2015) :
  c'est littéralement notre problème. Entrée : un maillage brut issu de
  stéréo multi-vues. Le pipeline a trois étapes. La *classification* par champ
  de Markov répartit les faces en sol, arbre, façade et toit. L'*abstraction*
  régularise les plans et pose des « icônes ». La *reconstruction* produit des
  bâtiments étanches par coupe minimale, à plusieurs LOD.
  ([HAL](https://hal.archives-ouvertes.fr/hal-01113078))
- **Zhu et al., ECCV 2018** : la même famille de méthodes à l'échelle d'une
  ville. Champ de Markov qui combine hauteur et image, sorties sémantiques
  multi-LOD.
  ([PDF](https://eccv2018.org/openaccess/content_ECCV_2018/papers/Lingjie_Zhu_Large_Scale_Urban_ECCV_2018_paper.pdf))
- **BigSUR** (Kelly, Femiani, Wonka, Mitra, *SIGGRAPH Asia* 2017) : fusionne
  **emprises SIG + maillage grossier + imagerie de rue** par optimisation
  globale. Sortie : emprises, profils de toits et éléments de façade, testé sur
  1 011 bâtiments. C'est exactement l'idée de « s'appuyer sur un jeu de
  référence » : l'emprise cadastrale contraint la forme et le maillage fournit
  les hauteurs et les profils.
  ([projet](https://geometry.cs.ucl.ac.uk/projects/2017/bigsur))

**Méthodes à partir de nuages de points (applicables en échantillonnant le
maillage).** Un maillage photogrammétrique se ré-échantillonne en nuage dense,
comme le fait SUM (10, 30 et 300 pts/m²). Tout l'outillage LiDAR devient alors
utilisable.

- **roofer / 3DBAG** (TU Delft, Peters, Dukai, Ledoux et al.) : reconstruction
  LoD1.2, LoD1.3 et LoD2.2 à partir d'**un nuage de points et d'une emprise de
  toit**. Le pipeline détecte plans et lignes, régularise, partitionne le toit
  et sélectionne les faces par coupe de graphe. Il produit en continu les
  10 millions de bâtiments des Pays-Bas. Outil C++ avec bindings Python, sous
  **GPL-3.0** : il se lance comme binaire externe, jamais en dépendance liée
  (licence incompatible avec MIT OR Apache-2.0).
  ([3DBAG](https://3d.bk.tudelft.nl/projects/3dbag/),
  [org GitHub](https://github.com/3DBAG),
  [cours 2026](https://3d.bk.tudelft.nl/courses/backup/geo1004/2026/data/lod2recon.pdf))
- **PolyGNN** (Chen, Shi, Nan, Xiong, Zhu, *ISPRS J.* 2024) : un GNN classe les
  cellules d'une partition polyédrique en intérieur ou extérieur, ce qui donne
  un modèle étanche et compact. Entraîné sur plus de 500 000 bâtiments
  synthétiques, transfert entre villes évalué, code public.
  ([arXiv 2307.08636](https://arxiv.org/abs/2307.08636))
- **PBWR** (Huang et al., CVPR 2024) : reconstruction de toits en fil de fer
  paramétrique à partir de LiDAR aérien.
- **Point2WSS** (*ISPRS Annals* 2026) : apprentissage avec un modèle de
  squelette droit pondéré, pour des LoD2 réguliers et légers.
  ([lien](https://isprs-annals.copernicus.org/articles/XI-2-2026/331/2026/))
- **Complétion + primitives** (Ding et al., *Remote Sensing* 2025) : complète
  les nuages troués avant d'ajuster des primitives. C'est pertinent ici, parce
  que les maillages photogrammétriques ont des façades molles et des cours
  intérieures vides.
  ([PDF](https://mdpi-res.com/d_attachment/remotesensing/remotesensing-17-00399/article_deploy/remotesensing-17-00399.pdf))
- **Building3D** (Wang et al., ICCV 2023) : 160 000 bâtiments estoniens avec
  nuage, maillage et **fil de fer de toit**, plus de 60 types de toits. C'est
  un bon jeu d'apprentissage et d'évaluation pour la cible B.
  ([site](https://building3d.ucalgary.ca/),
  [arXiv 2307.11914](https://arxiv.org/abs/2307.11914))
- À partir d'images satellite (DSM + ortho) : **SAT2LoD2** (Gui et al.,
  *ISPRS Annals* 2024, segmentation en unités de toit), avec code ouvert.

### 2.3 Réinterprétation procédurale et générative (cible C)

- **Inversion de bâtiments procéduraux par transformer** (Dax et al., CVPR 2025,
  atelier SynData4CV) : à partir d'un nuage de points, le modèle prédit le
  *programme* procédural qui génère le bâtiment. Il est entraîné uniquement sur
  des bâtiments procéduraux synthétiques et leurs nuages simulés. La sortie
  est éditable et compacte, et c'est la forme la plus forte de
  « transformation en forme ».
  ([arXiv 2501.17044](https://arxiv.org/abs/2501.17044))
- **GeoTexBuild** (2025) : génère des bâtiments 3D à partir d'emprises
  cartographiques (carte de hauteur, géométrie, apparence) par diffusion
  guidée. C'est utile *a contrario* : à partir d'une emprise seule, le modèle
  invente. Le maillage observé sert à ancrer la génération dans le réel.
  ([arXiv 2504.08419](https://arxiv.org/abs/2504.08419))
- **AIM2PC** (2025) : nuage de bâtiment complet à partir d'une seule image
  aérienne, par diffusion.
  ([arXiv 2503.18527](https://arxiv.org/abs/2503.18527))
- Nishida et al. (*Eurographics* 2018) : l'ancêtre « photo + grammaire
  procédurale ».

### 2.4 Ombres et éclairage cuit (transversal)

Ici, l'avantage d'un maillage sur une image seule est décisif : **on connaît la
géométrie, donc on peut calculer les ombres au lieu de les deviner.**

- **Ombres calculées depuis un modèle de surface + direction du soleil.** La
  littérature photogrammétrique fait ça depuis vingt ans pour les vraies
  orthophotos. On lance un rayon vers le soleil depuis chaque point de surface
  (*z-buffer* solaire). Il y a une variante hybride : l'ombre projetée sert de
  région, et une analyse d'image affine ensuite le bord, ce qui compense
  l'imprécision du modèle. L'ombre solaire est le même problème que
  l'occlusion, avec le soleil à la place de la caméra.
  ([Braun, PhoWo 2003](https://phowo.ifp.uni-stuttgart.de/publications/phowo03/braun.pdf),
  [ISPRS XXXV, détection et retrait d'ombres sur photo aérienne urbaine](https://WWW.ISPRS.org/proceedings/XXXV/congress/comm3/papers/378.pdf),
  [ISPRS 2018](https://isprs-archives.copernicus.org/articles/XLII-3/2527/2018/isprs-archives-XLII-3-2527-2018.pdf))
  - **Inconnue clé : la direction du soleil.** Une tuile ne porte ni date ni
    heure de prise de vue, et la texture est une mosaïque de vols (le même
    constat que pour l'imagerie drapée, `docs/18-color-harmonization.md` §1).
    Il faut donc *estimer* le soleil, par région de texture homogène. On
    cherche la direction dont l'ombre lancée sur le maillage explique le mieux
    les pixels sombres observés. C'est un problème inverse peu dimensionné
    (2 angles par région), bien posé dès qu'il y a des bâtiments.
- **Albédo à partir d'images aériennes photogrammétriques** (Song & Qin,
  *ISPRS Annals* 2022) : un modèle de formation d'image extérieure (soleil +
  ciel), inversé en utilisant le maillage photogrammétrique comme géométrie
  initiale. C'est le papier le plus directement aligné avec « analyser et
  supprimer les ombres » sur notre type de donnée.
  ([arXiv 2204.04142](https://arxiv.org/abs/2204.04142))
- **Détection et retrait d'ombres dans l'imagerie aérienne (2D)** : AISD
  (Luo et al. 2020, 514 images annotées, longtemps le seul jeu public),
  **S-EO** (2025, 702 tuiles WorldView-3 géoréférencées, masques générés
  automatiquement à partir de la géométrie),
  [SARU](https://www.alphaxiv.org/abs/2604.25432) (2026, détection et retrait
  unifiés, retrait sans apprentissage), et un jeu d'orthophotos à 20 cm et 5 cm
  ([Kobayashi et al., ISPRS 2026](https://isprs-archives.copernicus.org/articles/XLIX-B3-2026/141/2026/)).
  ([S-EO, arXiv 2504.06920](https://alphaxiv.org/overview/2504.06920v2))
  Ces méthodes travaillent dans l'image et ignorent la 3D. Elles servent de
  *raffinement* du bord d'ombre, pas de source principale.
- **Ombres de terrain par lancer de rayons sur MNE** pour apprendre le retrait
  d'ombres (Staats, TU Wien 2024, « Alpine Terrain Relighting ») : la géométrie
  sert à *générer* des données d'entraînement. C'est l'idée exacte à
  transposer : avec un maillage et un soleil choisi, on sait fabriquer autant
  de paires « avec ombre / sans ombre » qu'on veut.
  ([thèse](https://www.cg.tuwien.ac.at/research/publications/2024/staats-2024-atr/staats-2024-atr-thesis.pdf))
- **Ré-éclairage neuronal de scènes extérieures** (2025) : ROSGS sépare soleil
  (gaussienne sphérique) et ciel (harmoniques sphériques) sur une géométrie en
  splatting 2D. OSDR-GS ajoute un module de visibilité soleil. Un autre
  travail extrait explicitement la *visibilité solaire* par résidu, puis
  retrace les ombres après ré-éclairage. LumiGauss (WACV 2025) travaille sur des
  photos « in the wild ». Ces méthodes supposent *plusieurs* images sous
  plusieurs éclairages, alors que nous n'avons qu'une texture cuite. Elles
  donnent le modèle physique, pas la recette.
  ([ROSGS](https://arxiv.org/abs/2509.11275),
  [OSDR-GS](https://www.ijcai.org/proceedings/2025/111),
  [visibilité solaire](https://arxiv.org/abs/2507.20512),
  [LumiGauss](https://openaccess.thecvf.com/content/WACV2025/papers/Kaleta_LumiGauss_Relightable_Gaussian_Splatting_in_the_Wild_WACV_2025_paper.pdf))
- Outils de production existants : délitage de textures photogrammétriques à
  partir d'AO, de normales et de *bent normals*. C'est le même principe : la
  géométrie dit ce qui est éclairé.

## 3. Une chaîne plausible (esquisse, pas un plan)

```
3D Tiles maillées (source ouverte)
   │  décodage existant (TileContent::Decoded), recentrage local f64 → f32
   ▼
① Fusion d'une zone : réunir les tuiles feuilles d'une emprise en un maillage
   continu (soudure des bords de tuiles, déduplication)
   ▼
② Estimation du soleil par région de texture (2 angles), par lancer de rayons
   sur le maillage + score contre la luminance observée
   ▼
③ Masque d'ombre = visibilité solaire calculée, affinée au bord dans l'image
   → albédo approché : texel / (E_soleil·V·cosθ + E_ciel·AO)
   ▼
④ Segmentation des faces (géométrie + albédo, plus seulement couleur brute)
   ▼
⑤ Par bâtiment : emprise de référence (cadastre ouvert) ∩ faces « bâtiment »
   → nuage échantillonné → reconstruction LoD2 (roofer externe, PolyGNN…)
   ▼
⑥ Sorties : CityJSON / 3D Tiles (glb + EXT_structural_metadata) / USD,
   texture albédo re-projetée sur les faces planes
```

Notes d'architecture, si l'exploration devenait un jour un crate :

- **Hors du cœur.** Tout ceci est lourd (lancer de rayons, apprentissage,
  binaires externes). C'est un *consommateur* comme `tuile-usd` : il lit un
  tileset via les traits existants et écrit des fichiers. `tuile-core` n'en
  sait rien (règle 1), et `tuile-server` non plus.
- L'étape ② ressemble beaucoup à ce que `tuile-bake` et `tuile-radiometry`
  manipulent déjà (texture cuite, exposition par lot de prise de vue). Le
  champ « soleil estimé par région » est cousin du champ de correction
  colorimétrique de `docs/18-color-harmonization.md`, et pourrait partager sa
  structure géographique.
- L'étape ⑤ est l'endroit où le **jeu de référence** entre : l'emprise
  cadastrale fixe la topologie, donc la question devient « quelle hauteur et
  quel toit pour cette emprise ? » au lieu de « où sont les bâtiments ? ».
  C'est l'approche de 3DBAG et de BigSUR, et c'est elle qui rend le problème
  robuste.
- Précision : fusion et reconstruction en repère local (ENU de la zone), f64
  pour l'ancrage et f32 seulement après recentrage (règle 7).

## 4. Jeux de données de référence (ouverts)

| Jeu | Contenu | Rôle | Licence / accès |
|---|---|---|---|
| **SUM** (Helsinki) | maillage texturé oblique étiqueté (6 classes), nuages échantillonnés | entraînement et évaluation de la cible A, **et** source d'entrée réaliste | public, [TU Delft](https://3d.bk.tudelft.nl/projects/meshannotation/) |
| Maillage 3D de Helsinki | maillage texturé de la ville entière | source d'entrée à grande échelle | données ouvertes de la ville |
| **3DBAG** (Pays-Bas) | 10 M de bâtiments LoD1.2–2.2, emprises BAG, nuage AHN | **vérité terrain** de la cible B, et emprises | ouvert, [3dbag.nl](https://3d.bk.tudelft.nl/projects/3dbag/) |
| **Building3D** (Estonie) | 160 000 nuages de toits + fils de fer, plus de 60 types | apprentissage et évaluation des toits | académique, [site](https://building3d.ucalgary.ca/) |
| **LiDAR HD IGN** (France) | nuage classé ≥10 pts/m² (dont classe bâtiment), MNT 50 cm | vérité terrain des hauteurs et toits en France ; calcul indépendant des ombres | Etalab 2.0, [data.gouv.fr](https://www.data.gouv.fr/datasets/lidar-hd-ign-2022) |
| **BD TOPO** (IGN) / cadastre | emprises de bâtiments (avec hauteur) | emprises de référence de l'étape ⑤ | Etalab 2.0 |
| OpenStreetMap / Overture | emprises mondiales, hauteurs parfois | emprises de référence hors de France et des Pays-Bas | ODbL / mixte : vérifier la couche |
| AISD, S-EO, SARU (RSISD, SiSRB) | ombres annotées en imagerie aérienne et satellite | raffinement 2D des bords d'ombre | académique |

**Un couple fort pour une première expérience : une ville française avec
LiDAR HD + BD TOPO + un maillage photogrammétrique ouvert de la même zone.**
Le LiDAR donne une vérité terrain *indépendante* de la texture : on mesure la
qualité des LoD2 issus du maillage en les comparant aux LoD2 issus du LiDAR,
avec la même emprise et le même outil. Le LiDAR permet aussi de valider
l'estimation du soleil (l'ombre calculée sur le nuage LiDAR doit coïncider avec
l'ombre vue dans la texture). Les Pays-Bas offrent la même chose clé en main
avec 3DBAG.

Remarque : avec LiDAR HD + BD TOPO, **on obtient des LoD2 sans passer par un
maillage photogrammétrique du tout**. La question de recherche réelle est donc :
*qu'est-ce que le maillage texturé apporte en plus ?* Réponses candidates : la
couverture là où il n'y a pas de LiDAR (hors de France et des Pays-Bas), la
texture (façades, matériaux), la fraîcheur, les détails de façade (cible C,
LoD3).

## 5. Expériences qui trancheraient (si on y allait)

1. **Faisabilité du soleil.** Sur 1 km² de maillage ouvert avec LiDAR de
   référence : l'estimation par région de la direction du soleil (②) donne-t-elle
   un masque d'ombre dont l'IoU dépasse 0,8 face aux pixels sombres annotés à
   la main ? Ça coûte peu et ça répond à « peut-on analyser les ombres
   automatiquement ».
2. **Gain de l'albédo pour la segmentation.** SUM, même réseau, entrée
   « couleur cuite » contre « albédo ③ » : l'écart de mF1 sur les classes
   sensibles à l'ombre (sol et végétation à l'ombre des bâtiments).
3. **LoD2 maillage contre LoD2 LiDAR**, même emprise et même outil :
   distance de Hausdorff et RMSE des toits, taux de bâtiments dont la
   topologie de toit est correcte. On rapporte les échecs par type (cour
   intérieure, toit végétalisé, bâtiment sous un arbre).
4. **Transfert de domaine.** Le réseau entraîné sur SUM (Helsinki), appliqué à
   un autre maillage ouvert sans réentraînement : la perte de performance
   mesure ce qui coûterait de passer à l'échelle.

Chaque expérience doit produire des **images regardées** (règle « every render
gets opened ») : masques d'ombre superposés, LoD2 superposés au maillage. Un
score seul ment.

## 6. Questions ouvertes

- **Granularité du soleil.** Une texture est une mosaïque de vols, souvent sans
  frontière visible. Une région par tuile feuille suffit-elle, ou faut-il une
  segmentation de la texture en lots de prise de vue d'abord ?
- **Ombres de végétation.** La végétation est mal reconstruite (« éponge »), si
  bien que son ombre calculée sera fausse. Faut-il traiter les arbres comme des
  volumes semi-transparents, ou les exclure ?
- **Lumière du ciel et façades.** Les façades sont vues en oblique, souvent
  floues, et éclairées surtout par le ciel. Le modèle soleil + ciel de Song & Qin
  y tient-il ?
- **Non-variétés.** Les maillages photogrammétriques ont des faces dupliquées
  aux bords de tuiles et des recouvrements entre LOD. La fusion ① est-elle un
  sous-problème à part entière ?
- **Où s'arrêter.** La cible B avec emprises de référence est, de loin, le
  meilleur rapport valeur/risque. La cible C (procédural, LoD3) est un projet de
  recherche, pas une fonctionnalité.

## 7. Résumé

- **Ne pas viser les tuiles photoréalistes commerciales comme source** : leurs
  règles interdisent explicitement les objets 3D dérivés « à la main ou par
  machine ». Développer sur des maillages ouverts. L'outil reste générique pour
  tout 3D Tiles maillé.
- **La géométrie rend les ombres calculables** : une estimation du soleil par
  région et un lancer de rayons sur le maillage donnent un masque d'ombre et un
  albédo approché. C'est l'angle le plus original et le plus à portée.
- **Le jeu de référence est la clé de la robustesse** : les emprises
  cadastrales transforment « trouver les bâtiments » en « ajuster un toit à une
  emprise ». C'est le chemin éprouvé (3DBAG, BigSUR). LiDAR HD et 3DBAG
  fournissent une vérité terrain indépendante pour mesurer.
- **La valeur propre du maillage texturé**, par rapport au LiDAR et au
  cadastre, est à démontrer. C'est la vraie question de recherche.
