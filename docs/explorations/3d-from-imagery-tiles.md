<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Une 3D crédible à partir des tuiles d'imagerie — état de l'art

Note de recherche exploratoire, rédigée le 2026-10-14. **Rien n'est implémenté,
rien n'est proposé à l'implémentation.** Ce n'est pas un jalon de
`docs/03-roadmap.md` : c'est une piste post-v1, rangée avec la génération à la
volée que `docs/00-vision.md` repousse après la v1.

La note répond à la question : *peut-on, à partir des tuiles raster 2D
(orthophoto ou satellite) et de la tuile de terrain, déjà affichées par Tuile,
synthétiser la 3D de toute la scène — relief fin, bâtiments, arbres, ponts —
de façon **crédible**, sans prétendre à l'exactitude ?* Elle complète
[`shape-from-photorealistic-tiles.md`](shape-from-photorealistic-tiles.md), qui
traitait l'autre entrée possible (les maillages photogrammétriques déjà en 3D).

**Méthode et limites de cette revue.** Environ 200 recherches web, réparties sur
quatre axes. Aucun article n'a pu être ouvert en texte intégral : arxiv.org,
cvf.com, copernicus.org et huggingface.co ne résolvaient pas depuis
l'environnement de recherche. Chaque entrée repose donc sur le résumé ou
l'extrait vus dans les résultats. Un chiffre ou un lieu de publication non
recoupé porte la mention *(non vérifié)*. Les licences de jeux de données ou de
code non lues directement portent *(licence non vérifiée)*. Avant de s'appuyer
sur un chiffre, relire l'article.

---

## 0. En une page

- **Le critère est la crédibilité, pas la vérité.** Une scène est crédible si,
  vue d'en haut, elle redonne la tuile ; si ses ombres ont la longueur de celles
  de l'image ; si ses volumes ont des proportions vraisemblables ; si une rue
  garde un style cohérent ; et si rien ne saute d'une image à l'autre ou d'un
  niveau de détail à l'autre (§1).
- **Une orthophoto a perdu presque toute la 3D**, mais pas toute. Il reste les
  ombres, le dévers résiduel des bâtiments, la sémantique, et ce qu'un réseau a
  appris des villes (§2).
- **La hauteur à partir d'une image unique est un problème mûr.** Les réseaux
  dédiés (IM2HEIGHT, IMG2nDSM, HTC-DC Net) sont dépassés depuis 2024 par les
  modèles fondation de profondeur affinés (Depth Anything V2, MoGe-2, DINOv3).
  L'erreur typique est de 1,5 à 4 m (§3.1).
- **La faisabilité à l'échelle mondiale est démontrée.** Il existe une hauteur
  de canopée à 1 m tirée de RVB seule (Meta/WRI, erreur absolue moyenne 2,8 m).
  Il existe des hauteurs de bâtiments tirées de Sentinel-2 à 10 m/px (Open
  Buildings 2.5D), et 2,68 milliards de bâtiments LoD1 tirés de PlanetScope
  (GlobalBuildingAtlas) (§3.5).
- **Ce qui ne se voit pas d'en haut s'invente bien en 2.5D.** RoofDiffusion
  (ECCV 2024, MIT) complète des toits avec 99 % de points manquants. Les
  façades n'ont presque aucune littérature à partir d'une vue verticale : ce
  sont les grammaires procédurales et la diffusion qui les rendent crédibles
  (§3.8, §3.9).
- **Il existe des générateurs « image satellite → ville 3D » de bout en
  bout** : Sat2Scene, Sat2City v1/v2, MagicCity, EarthCrafter, Skyfall-GS. Ils
  sont spectaculaires mais travaillent sur des carrés isolés, presque toujours
  évalués sur des métriques perceptuelles, souvent entraînés sur des rendus de
  données commerciales (§3.13).
- **Le jeu de référence existe, et il est français.** FLAIR-HUB (IGN) apparie
  ortho 20 cm, modèle de surface 20 cm et modèle de terrain sur 2 528 km². Le
  LiDAR HD, la BD TOPO et la BD ORTHO sont sous Etalab 2.0 (§4).
- **Les licences imposent la trajectoire.** Les tuiles 3D commerciales
  interdisent explicitement la dérivation, et les rendus de Google Earth sont
  non commerciaux. Plusieurs briques clés sont sous GPL-3.0 : roofer, City3D,
  PolyFit, HEAT et le code de Building-PCC. Il faut réimplémenter à partir des
  articles, ou s'en tenir aux briques MIT/BSD (§5).
- **Le trou principal de la littérature : personne n'estime le soleil à partir
  des ombres d'une vue verticale sans métadonnées.** Or une tuile web n'en porte
  aucune (§9).

---

## 1. La question, recadrée

**Entrée.** Une tuile d'imagerie (z17–z19, soit environ 1,2 m à 0,3 m/px), plus
la tuile de terrain sous-jacente. C'est exactement ce que Tuile drape déjà
(`tuile-core::raster`, `tuile-terrain`).

**Sortie.** Des tuiles 3D (glb dans un 3D Tiles) qui remplacent le sol drapé
plat :

- un micro-relief au sol ;
- des bâtiments en volume, avec toits et façades ;
- des arbres comme objets ;
- des ouvrages : ponts, talus.

### Ce que « crédible » veut dire ici

| Critère | Ce qui le casse | Comment on le mesure |
|---|---|---|
| **Cohérence verticale** : rendue de dessus, la 3D redonne la tuile | toit décalé par rapport à l'ortho, texture étirée | PSNR / SSIM / LPIPS entre rendu nadir et tuile source |
| **Ombres cohérentes** : avec le même soleil, les ombres calculées recouvrent les ombres de l'image | immeuble trop bas ou trop haut | IoU entre masque d'ombre calculé et masque détecté |
| **Proportions plausibles** | tours écrasées, arbres en champignon | écart à une référence ouverte quand elle existe (LiDAR HD), distributions de hauteur par classe |
| **Cohérence de quartier** : style de façade homogène dans une rue | chaque bâtiment invente un style | revue visuelle, diversité contrôlée |
| **Stabilité** : rien ne saute entre frames ni entre niveaux de détail, et le résultat est déterministe | pop au zoom, scintillement, résultat qui change d'un rendu à l'autre | différence inter-frames sur un film, octets identiques à pack identique |
| **Jamais de trou** (règle de `CLAUDE.md`) | 3D synthétique qui découpe le sol sans le remplacer | une tuile 3D échouée retombe sur le drapé plat, jamais sur du noir |

### Trois niveaux de confiance

Le même bâtiment mélange trois niveaux de confiance, qu'il faut garder
distincts dans les données :

- **observé** : l'empreinte du toit, sa couleur ;
- **inféré** : sa hauteur, déduite de l'ombre et du réseau ;
- **inventé** : ses façades, ses fenêtres, la face arrière d'un arbre.

Le niveau peut être porté par face dans les métadonnées 3D Tiles
(`EXT_mesh_features` / `EXT_structural_metadata`), pour qu'un client puisse le
savoir ou l'afficher.

### Ce que la crédibilité n'excuse pas

La licence de la source (§5). Un bâtiment *inventé* à partir d'une tuile
commerciale reste un objet *dérivé* de cette tuile.

---

## 2. Ce qu'une tuile d'imagerie contient encore de la 3D

L'orthorectification projette tout à la verticale. Ce qui survit :

1. **Les ombres.** C'est l'indice le plus fort. Hauteur ≈ longueur d'ombre ×
   tan(élévation du soleil). L'ombre est d'abord une *mesure* ; on ne l'efface
   de la texture qu'ensuite. Le soleil n'est pas connu, mais il est constant sur
   un lot de prise de vue.
2. **Le dévers résiduel.** Les orthos classiques ne sont pas des « vraies
   orthos » : les immeubles penchent radialement, d'autant plus qu'ils sont
   hauts. Le décalage toit → pied est proportionnel à la hauteur (§3.2). Une
   vraie ortho (*true ortho*) efface cet indice.
3. **Les connaissances apprises.** Taille de toit, type de toit, densité du
   tissu urbain : un réseau en tire une hauteur probable.
4. **La sémantique.** Bâtiment, arbre, eau, route : chaque classe a sa
   géométrie typique.
5. **Le terrain.** On n'estime que la hauteur *au-dessus du sol* (nDSM), et le
   terrain place le tout sur le globe.
6. **La redondance entre sources.** Deux niveaux de zoom ou deux fournisseurs
   viennent souvent de vols différents. Leurs ombres et leurs dévers diffèrent,
   ce qui donne une pseudo-stéréo. Je n'ai trouvé aucune publication qui
   l'exploite entre orthos : c'est une piste ouverte (§9).

---

## 3. État de l'art

Format des entrées : **Nom** — auteurs, lieu, année. Ce que ça fait et pourquoi
c'est pertinent ici. Chiffre clé. Code et données. — lien.

### 3.1 Hauteur à partir d'une image unique (nDSM monoculaire)

**Réseaux dédiés (2017–2023).**

- **IM2HEIGHT** — Mou & Zhu, arXiv 2018. Encodeur-décodeur résiduel qui régresse
  la hauteur à partir d'une image aérienne, testé sur Berlin. Pose le problème
  comme mal posé (ambiguïté d'échelle). — https://arxiv.org/pdf/1802.10249
- **IMG2DSM** — Ghamisi & Yokoya, IEEE GRSL 2018. GAN conditionnel image → DSM,
  testé sur Vaihingen, Potsdam et Stockholm. C'est la première formulation
  générative du problème. — https://elib.dlr.de/119293
- **U-IMG2DSM** — Paoletti et al., IEEE GRSL 2021. Traduction image → DSM *non
  appariée* (VAE + GAN) : pas besoin de paires co-registrées. —
  https://portalcientifico.uned.es/documentos/6145ab1f65b6b477913b3f66?lang=ca
- **Amirkolaee & Arefi** — ISPRS J. P&RS 149, 2019. Hauteur relative par
  patches qui se recouvrent, puis assemblage en DSM absolu. C'est le même
  problème de raccord que pour des tuiles. —
  https://www.rlp-forschung.de/public/publications/151884
- **IM2ELEVATION** — Liu et al., Remote Sensing 2020. Élévation à partir d'une
  image aérienne unique *(détails non vérifiés)*. —
  https://mural.maynoothuniversity.ie/15104/1/RB_im2elevation.pdf
- **Régression ordinale** — Li, Wang & Fang, arXiv 2020. La hauteur est traitée
  comme une classification ordonnée par intervalles. —
  https://arxiv.org/pdf/2006.02801
- **Raffinement sémantique et géométrique** — arXiv 2020 (2011.10697). Prédiction
  de hauteur raffinée par guidage sémantique. — https://arxiv.org/pdf/2011.10697
- **IMG2nDSM** — Karatsiolis, Kamilaris & Cole, Remote Sensing 13(12), 2021.
  Hybride U-Net/ResNet qui prédit directement la hauteur au-dessus du sol, testé
  sur Manchester et DFC2018. — https://www.mdpi.com/2072-4292/13/12/2417/html
- **HTC-DC Net** — Chen, Shi, Xiong & Zhu, IEEE TGRS 61, 2023. Classification
  plus régression avec coupure tête/queue et intervalles adaptatifs. Il
  s'attaque à la sous-estimation des immeubles hauts (distribution à longue
  traîne). C'est la référence supervisée. — https://arxiv.org/pdf/2309.16486
- **TSE-Net** — arXiv 2025 (2511.13552). Auto-entraînement semi-supervisé pour
  pallier le manque de vérité terrain. — https://arxiv.org/pdf/2511.13552
- **Hauteur + LiDAR épars** — arXiv 2025 (2505.06905). Hauteur monoculaire
  corrigée par un LiDAR clairsemé (GEDI, ICESat-2, LiDAR ouvert). —
  https://arxiv.org/pdf/2505.06905v1
- **Captures Google Earth → MNE** — Najaf, Arefi et al., ISPRS Annals 2023.
  Erreur quadratique moyenne de 2,07 m sur des captures d'écran contre 2,04 m
  sur les données ISPRS : *l'imagerie « grand public » se prête à l'exercice*.
  La licence de la source, elle, ne s'y prête pas (§5). —
  https://isprs-annals.copernicus.org/articles/X-4-W1-2022/589/2023/

**La vague des modèles fondation (2024–2026).** Depuis 2024, on affine des
modèles de profondeur monoculaire génériques plutôt que d'entraîner des réseaux
dédiés. Le verrou connu : ils prédisent une profondeur *relative* ou *affine*,
qu'il faut recaler en hauteur métrique au-dessus du sol.

- **Depth Any Canopy** — Rege Cambrin, Corley & Garza, ECCV 2024 (atelier
  CV4E). Depth Anything V2 affiné pour la hauteur de canopée. Il égale ou bat
  l'état de l'art pour **moins de 1,30 $ d'entraînement**. Code et poids : oui. —
  https://arxiv.org/pdf/2408.04523
- **Depth2Elevation** — Hong, Wu, Xu & Zhao, IEEE TGRS 2025. Ajoute un
  modulateur d'échelle aux encodeurs Depth Anything et un décodeur indépendant
  de la résolution. Code : non trouvé. —
  https://openrepository.aut.ac.nz/items/4d5713c5-f7ea-4a08-9a73-4c2555e29743
- **Depth Anything V2 pour la hauteur aérienne** *(titre et auteurs non
  vérifiés)*. Sur DFC2018, Potsdam et Vaihingen : −7,2 % d'erreur absolue
  moyenne par rapport au seul encodeur ; garder le décodeur de profondeur
  compte. — https://www.ipu.ru/node/82914
- **Depth2CHM** — Lai et al., arXiv 2026 (2602.06503). Depth Anything V2
  affiné sur environ 16 000 km² de LiDAR de canopée. Erreur quadratique moyenne
  de 2,54 m et 5,75 m selon le site ; environ 1,5 m de mieux qu'une carte
  mondiale à 1 m existante. — https://arxiv.org/pdf/2602.06503
- **CNN contre Depth Anything V2 à l'échelle d'un continent** — ISPRS Annals
  XI-3, 2026. Canopée australienne à partir de PlanetScope *(résultats non
  vus)*. —
  https://isprs-annals.copernicus.org/articles/XI-3-2026/779/2026/isprs-annals-XI-3-2026-779-2026.pdf
- **Profondeur relative sur grandes images** — ISPRS Archives XLVIII-1-W5,
  2025. Patches qui se recouvrent pour tenir en mémoire, directement lié aux
  raccords entre tuiles. —
  https://isprs-archives.copernicus.org/articles/XLVIII-1-W5-2025/7/2025/isprs-archives-XLVIII-1-W5-2025-7-2025.pdf
- **Prior2DSM** (titre exact : *Test-Time Adaptation for Height Completion via
  Self-Supervised ViT Features and Monocular Foundation Models*) — Rafaeli,
  Svoray & Nahlieli, arXiv 2026 (2604.02009). Sans entraînement : au moment de
  l'inférence, une LoRA et un petit MLP sur des descripteurs DINOv3 prédisent un
  facteur d'échelle et un décalage variables dans l'espace. Ils transforment la
  profondeur relative en DSM métrique. Il faut un DSM partiel en entrée : c'est
  de la complétion et de la mise à jour. Jusqu'à −46 % d'erreur quadratique
  moyenne par rapport à un recalage linéaire ; **MoGe-2** donne de meilleures
  proportions que Depth Anything V2 et Depth Pro. —
  https://arxiv.org/abs/2604.02009
- **Marigold** — Ke et al., CVPR 2024. Stable Diffusion réaffecté à la
  profondeur, entraîné sur des données synthétiques seulement. C'est un socle
  possible, sans article de télédétection vérifié. —
  https://arxiv.org/abs/2312.02145v1

**Lecture pour la crédibilité.** Une erreur de 2 à 4 m sur la hauteur est
invisible à l'œil si les **ombres calculées collent aux ombres de l'image**.
Recaler l'échelle sur les ombres revient à imposer la crédibilité, plutôt que
viser l'exactitude : cette contrainte paraît inexplorée (§9).

### 3.2 Le dévers comme indice de hauteur (off-nadir)

- **Geocentric pose** — Christie et al., CVPR 2020. À partir d'une image
  oblique unique, le réseau prédit la hauteur au-dessus du sol et un champ de
  « flux » de parallaxe par pixel, supervisé par du LiDAR public. C'est la
  formulation canonique du dévers comme géométrie apprenable. —
  https://openaccess.thecvf.com/content_CVPR_2020/html/Christie_Learning_Geocentric_Object_Pose_in_Oblique_Monocular_Images_CVPR_2020_paper.html
- **Overhead Geopose Challenge** — NGA, JHU/APL, NASA, 2021. Hauteur par pixel,
  plus angle et échelle par image, sur environ 7 000 images. —
  https://blog.drivendata.org/blog/overhead-geopose-challenge-winners
- **LOFT + BONAI** — Wang et al., arXiv 2022 (TPAMI, non vérifié). Prédit le toit
  plus un vecteur toit → pied ; sa longueur est proportionnelle à la hauteur.
  BONAI : 3 300 images, 268 958 instances. Code et données : oui. —
  https://arxiv.org/pdf/2204.13637
- **MTBR-Net** — Li et al., ICCV 2021. Sept tâches (sémantique et décalages) sur
  une image oblique, puis optimisation en modèles 3D. Plus de 40 % de mieux sur
  la hauteur. —
  https://openaccess.thecvf.com/content/ICCV2021/html/Li_3D_Building_Reconstruction_From_Monocular_Remote_Sensing_Images_ICCV_2021_paper.html
- **Supervision multi-niveaux** — arXiv 2024 (2404.04823). Angles d'off-nadir
  et de décalage, emprise et hauteur réelle, pour moins dépendre des
  annotations de décalage ; ajoute des décalages à OmniCity. —
  https://arxiv.org/pdf/2404.04823
- **OBM / OBMv2** — arXiv 2023 (2310.16717). Transformer « promptable » pour
  l'extraction toit + décalage ; critique la généralisation de LOFT en
  production. — https://arxiv.org/html/2310.16717v4
- **ONBuildingNet** — IEEE JSTARS 2024. Polygone de toit + décalage → emprise et
  hauteur → LoD1. C'est la chaîne la plus directe. —
  https://doaj.org/article/b8351d3cddae4a1592b9d1cfcaca0129
- **DragOSM** — arXiv 2025 (2509.17951). Recale des emprises OSM sur les toits
  d'une image oblique. Aucun décalage global ne convient, car il dépend de la
  hauteur de chaque bâtiment. — https://arxiv.org/pdf/2509.17951
- **ObliCity / DragRoof** — arXiv 2026 (2607.25210). Banc d'essai de correction
  du déplacement toit → sol (drone et satellite). Le déplacement compte d'autant
  plus que la résolution est fine. — https://arxiv.org/pdf/2607.25210
- **Relief displacement (principe)** — note ACRS 2004 sur les bâtiments qui
  penchent dans les orthos classiques. —
  https://a-a-r-s.org/proceeding/ACRS2004/Papers/DPH04-1.htm

**Lecture.** Sur une tuile web, l'angle de vue est inconnu, et la mosaïque
mélange des images prises à des angles différents. L'indice existe sur les orthos
classiques, mais il faut l'*apprendre* ; on ne peut pas le *calculer*.

### 3.3 Les ombres comme indice de hauteur

- **Régression de longueur d'ombre** — Qureshi, Chaudhry, Jabbar & Taj, arXiv
  2024 (2411.09411). YOLOv7 détecte bâtiment et ombre, ResNet18 régresse la
  longueur, l'élévation solaire la convertit en hauteur. Erreur quadratique
  moyenne de 3,84 m sur 42 villes. Code : oui. —
  https://arxiv.org/abs/2411.09411v1
- **Carte d'ombres en entrée** — Karatsiolis & Kamilaris, arXiv 2021
  (2104.10874). Une carte d'ombres bon marché est ajoutée en entrée d'un réseau
  de hauteur. — https://arxiv.org/abs/2104.10874v1
- **Métadonnées + instances d'ombre** — SPIE 11729, 2021. Pour chaque bâtiment,
  on cherche la hauteur dont l'ombre projetée recouvre le mieux l'ombre
  détectée (xView2, US3D). —
  https://medicalimaging.spiedigitallibrary.org/conference-proceedings-of-spie/11729/117290L/Building-height-estimation-via-satellite-metadata-and-shadow-instance-detection/10.1117/12.2585012.full
- **Simulation + Jaccard** — Kadhim & Mourshed, IEEE GRSL 2018. Référence
  classique : ombre simulée contre ombre détectée, par hauteur candidate. —
  https://orca.cardiff.ac.uk/id/eprint/105351/
- **Tours à partir des ombres** — Li et al., ISPRS IJGI 2024. Erreur moyenne
  inférieure à 5 m sur des immeubles de grande hauteur. —
  https://doaj.org/article/a982311581d149799b2828c151c2f86a
- **ICESat-2 + Google Earth** — IJAEOG 2023. Les hauteurs ICESat-2 servent
  d'étiquettes clairsemées pour un U-Net à ombres ; erreur absolue moyenne de
  4,08 m. — https://doaj.org/article/9f225e79ada545c3aac540b24f052687
- **Méthode manuelle et automatique** — ISPRS Archives XL-8, 2014. Erreur de
  0,67 m (manuel) et 0,96 m (automatique), le soleil étant connu par les
  métadonnées. —
  https://isprs-archives.copernicus.org/articles/XL-8/1185/2014/isprsarchives-XL-8-1185-2014.pdf
- **Soleil à partir de l'image (vue au sol)** — Hold-Geoffroy et al., CVPR 2017.
  Position du soleil par CNN sur des photos au sol. **Aucun équivalent trouvé
  pour la vue verticale.** — https://arxiv.org/pdf/1611.06403

### 3.4 Multi-vues satellite et champs neuronaux avec ombres

C'est utile si l'on dispose de plusieurs dates ou sources pour la même zone,
avec des soleils différents. C'est aussi là que la littérature traite les
ombres comme **indice géométrique** de premier ordre.

- **S2P** — de Franchis et al., ISPRS Annals 2014. Stéréo satellite classique
  par tuiles. C'est la référence à battre. —
  https://isprs-annals.copernicus.org/articles/II-3/49/2014/
- **VisSat / SatelliteSfM** — Zhang, Sun, Snavely, ICCVW 2019. Les caméras RPC
  sont approximées en perspective pour réutiliser COLMAP. —
  https://github.com/Kai-46/SatelliteSfM
- **S-NeRF (Shadow NeRF)** — Derksen & Izzo, CVPRW 2021. Premier NeRF satellite :
  champ de visibilité solaire et couleur du ciel selon le soleil. —
  https://arxiv.org/abs/2104.09877v1
- **Sat-NeRF** — Marí, Facciolo, Ehret, CVPRW 2022. Caméras RPC, irradiance
  sensible aux ombres, gestion des objets transitoires. Le DSM est au niveau de
  la stéréo de l'état de l'art. —
  https://openaccess.thecvf.com/content/CVPR2022W/EarthVision/html/Mari_Sat-NeRF_Learning_Multi-View_Satellite_Photogrammetry_With_Transient_Objects_and_Shadow_CVPRW_2022_paper.html
- **EO-NeRF** — Marí, Facciolo, Ehret, CVPRW 2023, « *The Detail Is in the
  Shadows* ». Les ombres sont rendues à partir de la géométrie elle-même, donc
  les ombres multi-dates *contraignent la hauteur*. —
  https://openaccess.thecvf.com/content/CVPR2023W/EarthVision/papers/Mari_Multi-Date_Earth_Observation_NeRF_The_Detail_Is_in_the_Shadows_CVPRW_2023_paper.pdf
- **SpS-NeRF** — Zhang & Rupnik, ISPRS Annals 2023. Deux ou trois vues
  seulement, supervision par la profondeur d'un appariement dense. —
  https://arxiv.org/abs/2309.00277v1
- **Sat-Mesh** — Qu & Deng, Remote Sensing 2023. Champ de distance signée et
  vrai maillage 3D texturé, plutôt que du 2.5D. —
  https://api.crossref.org/works/10.3390%2FRS15174297
- **Sat-DN** — Liu et al., arXiv 2025. Champ de distance signée sur grille de
  hachage, supervisé par profondeur et normales ; plus rapide, vise les façades
  peu texturées. — https://arxiv.org/abs/2502.08352v2
- **EOGS** — Savant Aira, Facciolo, Ehret, CVPR 2025. Premier Gaussian splatting
  pour l'observation de la Terre, avec ombres par passe de rendu solaire. Il
  égale EO-NeRF environ **300× plus vite**. — https://arxiv.org/html/2412.13047v2
- **EOGS++** — Bournez et al., arXiv 2025 / ISPRS Annals 2026. Images
  panchromatiques brutes et caméras affinées pendant l'apprentissage. —
  https://arxiv.org/abs/2511.16542v1
- **ShadowGS** — arXiv 2026 (2601.00939). Marche de rayon vers le soleil depuis
  chaque gaussienne, plus une perte de cohérence d'ombre. —
  https://arxiv.org/html/2601.00939v1
- **SatSplat** — Song, Kim, Qin, arXiv 2026. Splatting 2D avec ombres et
  correction de couleur par caméra. Sur DFC2019 : −11,9 % d'erreur absolue
  moyenne et −31 % de mémoire GPU. — https://arxiv.org/abs/2606.28581
- **RPC-GS** — arXiv 2026 (2606.06690). Rendu RPC natif ; ses travaux connexes
  cartographient bien la famille. — https://arxiv.org/pdf/2606.06690
- Également vus, non caractérisés :
  - SatSplatDiff : https://papers.cool/arxiv/2606.27223
  - SatSurfGS : https://arxiv.org/pdf/2605.07181
  - SatGeo-NeRF : https://arxiv.org/pdf/2603.21931
  - NeRF satellite HDR du DLR : https://isprs-annals.copernicus.org/articles/XI-2-2026/341/2026/isprs-annals-XI-2-2026-341-2026.pdf
  - NeRF conditionné par saison et soleil : https://ar5iv.labs.arxiv.org/html/2308.01262

### 3.5 Produits de hauteur à l'échelle mondiale (preuves de faisabilité)

- **Meta/WRI canopée 1 m (v1)** — Tolan et al., RSE 300, 2024. Descripteurs
  auto-supervisés type DINOv2 sur imagerie Maxar, décodeur entraîné sur LiDAR
  aérien. Erreur absolue moyenne de 2,8 m. Données ouvertes (licence non
  vérifiée). — https://arxiv.org/abs/2304.07213v3
- **Meta/WRI CHMv2** — mars 2026 (arXiv 2603.06382). Passage à DINOv3 pré-entraîné
  sur imagerie satellite. Le R² passerait de 0,53 à 0,86 *(non vérifié)*. Poids
  publiés. —
  https://ai.meta.com/blog/world-resources-institute-dino-canopy-height-maps-v2/
- **Canopée mondiale 10 m** — Lang, Jetz, Schindler & Wegner (ETH), Nature Ecol.
  Evol. 2023. Sentinel-2 + GEDI, incertitude par pixel. Code MIT. —
  https://arxiv.org/abs/2204.08322v1
- **Canopy height at scale** — Pauls et al., ICML 2024. Carte mondiale 10 m,
  perte robuste aux étiquettes GEDI bruitées. —
  https://arxiv.org/html/2406.01076v1
- **Open Buildings 2.5D Temporal** — Google Research, 2024. Présence, compte et
  **hauteur** des bâtiments par an de 2016 à 2023, **à partir de Sentinel-2**.
  Résolution effective de 4 m ; Afrique, Asie du Sud et du Sud-Est, Amérique
  latine. CC-BY 4.0 / ODbL. —
  https://sites.research.google/gr/open-buildings/temporal
- **Sirko et al.** — arXiv 2023 (2310.11622). Distillation d'un professeur haute
  résolution vers un élève Sentinel-2 : c'est la méthode derrière le produit
  précédent. — https://arxiv.org/abs/2310.11622v1
- **GHS-BUILT-H** — JRC, 2023. Hauteur moyenne des bâtiments à 100 m, mondiale. —
  https://developers.google.com/earth-engine/datasets/catalog/JRC_GHSL_P2023A_GHS_BUILT_H
- **3D-GloBFP** — Che et al., ESSD 16, 2024. Hauteur par emprise, mondiale ;
  erreur quadratique moyenne de 1,9 à 14,6 m selon la région. CC BY 4.0. —
  https://essd.copernicus.org/articles/16/5357/2024/
- **GlobalBuildingAtlas** — Zhu et al., ESSD 17, 2025. Hauteur à 3 m tirée de
  PlanetScope : **2,68 milliards de bâtiments LoD1**, erreur quadratique
  moyenne de 1,5 à 8,9 m. Données et code ouverts. —
  https://essd.copernicus.org/articles/17/6647/2025/
- **Microsoft Global ML Building Footprints** — 1,4 milliard d'emprises ; 174
  millions de hauteurs estimées dans la version de 2023 (méthode non
  documentée). CDLA-Permissive-2.0 (ODbL pour les anciennes versions). —
  https://github.com/microsoft/globalmlbuildingfootprints

**Lecture.** Si la hauteur des bâtiments est estimable à 10 m/px, elle l'est
*a fortiori* à 30 cm/px. La question n'est pas *si*, c'est *avec quelle
crédibilité de forme* (toits, détails).

### 3.6 Emprises et structure de toit à partir de l'image

**Emprises polygonales.**

- **Frame Field Learning** — Girard et al., CVPR 2021. La segmentation
  s'accompagne d'un champ de repères qui guide la conversion en polygones
  réguliers. Code **BSD-3**. —
  https://openaccess.thecvf.com/content/CVPR2021/html/Girard_Polygonal_Building_Extraction_by_Frame_Field_Learning_CVPR_2021_paper.html
- **PolyWorld** — Zorzi et al., CVPR 2022. Sommets, puis connexions par GNN et
  transport optimal. Inférence seulement (licence non lue). —
  https://openaccess.thecvf.com/content/CVPR2022/html/Zorzi_PolyWorld_Polygonal_Building_Extraction_With_Graph_Neural_Networks_in_Satellite_CVPR_2022_paper.html
- **HiSup** — Xu et al., ISPRS J. P&RS 198, 2023. Supervision hiérarchique
  sommets / segments / masque. Code **MIT**. — https://arxiv.org/abs/2208.00609v1
- **P2PFormer** — Zhang et al., arXiv 2024. Primitives de contour ordonnées, sans
  post-traitement. — https://arxiv.org/abs/2406.02930v2
- **PolyR-CNN** — Jiao, Persello, Vosselman, ISPRS J. P&RS 218, 2024. 79,2 AP sur
  CrowdAI ; gère les cours intérieures. — https://www.arxiv.org/abs/2407.14912
- **Pix2Poly** — Adimoolam, Poullis, Averkiou, WACV 2025. Séquence de sommets
  par transformer. Code **MIT**. —
  https://openaccess.thecvf.com/content/WACV2025/html/Adimoolam_Pix2Poly_A_Sequence_Prediction_Method_for_End-to-End_Polygonal_Building_Footprint_WACV_2025_paper.html
- **GCP** — Zhang, Shi, Zhu (TUM), arXiv 2025. Conversion en polygones à partir
  de n'importe quel masque d'instance. — https://arxiv.org/abs/2505.01385
- **Google Open Buildings** — Sirko et al., arXiv 2021. U-Net d'instances, 1,8
  milliard d'emprises en V3, CC-BY 4.0. — https://arxiv.org/abs/2107.12283v1

**Structure de toit (lignes de faîte, pans).**

- **Vectorizing World Buildings** — Zhang, Nauata, Furukawa, ECCV 2020 *(non
  vérifié)*. Graphe planaire du bâtiment, y compris les faîtes, à partir d'une
  image RVB. — https://arxiv.org/pdf/1912.05135
- **HEAT** — Chen, Qian, Furukawa, CVPR 2022. Coins, puis transformer qui classe
  les arêtes candidates. C'est la référence image → filaire de toit. Code
  **GPL-3.0**. —
  https://openaccess.thecvf.com/content/CVPR2022/html/Chen_HEAT_Holistic_Edge_Attention_Transformer_for_Structured_Reconstruction_CVPR_2022_paper.html
- **RSGNN** — Zhao et al. (ITC Twente), ISPRS J. P&RS 2022. Primitives puis GNN
  qui assemble le graphe des lignes de toit. —
  https://research.utwente.nl/en/publications/extracting-planar-roof-structures-from-very-high-resolution-image/
- **HEAT appliqué au LoD2** — Campoverde et al. *(non vérifié)*, Remote Sensing
  2024. — https://doaj.org/article/57fbd468c1254cbfa9ac41bf5845a6c2
- **Graphe de toit sur photos non orthorectifiées** — Foss, mémoire UiT 2024.
  F1 de 0,77, erreur de coin de 0,43 m. — https://munin.uit.no/handle/10037/34204
- **Pans de toit YOLOv8** — arXiv 2025 (2503.09187). —
  https://arxiv.org/html/2503.09187v1
- **KIBS** — Lussange et al., arXiv 2023. Deux Mask R-CNN : pans de toit et
  hauteur des coins, à partir d'**une seule image satellite non orthogonale**.
  Erreur de hauteur de 1,6 à 2,1 m. — https://arxiv.org/abs/2307.05409v1
- **Alidoost et al.** — Remote Sensing 2019 et ISPRS Annals 2020. Réseaux avec
  a priori de forme de toit, image unique *(appariement des liens à vérifier)*. —
  https://isprs-annals.copernicus.org/articles/V-2-2020/321/2020/
- **Choix de type de toit** — Partovi et al., ISPRS Archives 2017. Bibliothèque
  de 7 types, choisis à partir d'un DSM satellite + ortho. —
  https://isprs-archives.copernicus.org/articles/XLII-1-W1/653/2017/
- **DFC2023, piste 1** — IEEE GRSS, JSTARS 2024. Classification du type de toit
  (12 classes, 17 villes, environ 300 000 bâtiments). —
  https://www.grss-ieee.org/community/technical-committees/2023-ieee-grss-data-fusion-contest/
- **Lignes de toit non supervisées sur vraie ortho** — Gao, Peters, Stoter, 3D
  GeoInfo 2023. Partition cinétique, sans apprentissage. Code **GPL-3.0**. —
  https://github.com/tudelft3d/Roofline-extraction-from-orthophotos

### 3.7 LoD2 à partir de hauteurs et d'emprises

- **SAT2LoD2** — Gui, Qin, Tang, ISPRS Archives XLIII-B2-2022 ; méthode : Gui &
  Qin, ISPRS J. P&RS 181, 2021. Ortho + DSM → décomposition en rectangles →
  primitives de toit → LoD2. C'est l'outil ouvert le plus proche de « tuile
  satellite → LoD2 » (licence non lue). —
  https://isprs-archives.copernicus.org/articles/XLIII-B2-2022/379/2022/ ·
  https://github.com/gdaosu/lod2buildingmodel
- **Vectorisation 3D depuis le satellite** — Wang, Zorzi, Bittner, CVPRW 2021.
  DSM nettoyé par GAN, arêtes et coins de toit, puis LoD2. —
  https://openaccess.thecvf.com/content/CVPR2021W/EarthVision/html/Wang_Machine-Learned_3D_Building_Vectorization_From_Satellite_Imagery_CVPRW_2021_paper.html
- **roofer / 3DBAG** — TU Delft / 3DGI. Nuage + emprise → LoD1.2, 1.3 et 2.2
  (plans, régularisation, coupe de graphe). Environ 10 millions de bâtiments.
  Code **GPL-3.0**. — https://github.com/3DBAG/roofer
- **City3D** — Huang, Stoter, Peters, Nan, Remote Sensing 2022. PolyFit étendu ;
  **infère les murs que le LiDAR aérien ne voit pas**. Code **GPL-3.0**. —
  https://arxiv.org/abs/2201.10276v2
- **PolyFit** — Nan & Wonka, ICCV 2017. Sélection de faces par programmation
  linéaire binaire. Code **GPL-3.0**. —
  https://openaccess.thecvf.com/content_ICCV_2017/papers/Nan_PolyFit_Polygonal_Surface_ICCV_2017_paper.pdf
- **PolyGNN** — Chen et al., ISPRS J. P&RS 218, 2024. Un GNN classe intérieur et
  extérieur sur une partition polyédrique ; 500 000 bâtiments synthétiques.
  Code et données **MIT**. — https://arxiv.org/abs/2307.08636v2
- **Point2Building** — Liu, Obukhov, Wegner, Schindler, ISPRS J. P&RS 215, 2024.
  Génère le maillage de façon autorégressive (sommets puis faces) ; robuste aux
  façades manquantes. — https://arxiv.org/abs/2403.02136v2
- **PBWR** — Huang et al., CVPR 2024. Régression directe du filaire 3D. —
  https://openaccess.thecvf.com/content/CVPR2024/html/Huang_PBWR_Parametric-Building-Wireframe_Reconstruction_from_Aerial_LiDAR_Point_Clouds_CVPR_2024_paper.html
- **BWFormer** — Liu et al., CVPR 2025. Projection en carte de hauteur 2D, coins
  détectés en 2D puis relevés en 3D. Cette formulation « image » est adaptée aux
  tuiles. Code sans licence déclarée. —
  https://cvpr.thecvf.com/virtual/2025/poster/32868
- **Point2WSS** — Queffélec, Trouvé, Roussel, Wu, Vallet, ISPRS Annals 2026.
  Paramètres de squelette droit pondéré → LoD2 régulier et compact, robuste à
  l'entrée incomplète. Ce format paramétrique se prête bien au streaming. —
  https://isprs-annals.copernicus.org/articles/XI-2-2026/331/2026/
- **DSM → LoD2** — Wang, Zorzi, Bittner, 2021. —
  https://ar5iv.arxiv.org/html/2104.06485
- **Squelette droit (principe)** : le toit en croupe « canonique » d'une emprise
  quelconque. C'est l'outil de base pour un toit crédible quand l'image ne dit
  rien de plus.

### 3.8 Compléter ce qu'on ne voit pas (« entraîné avec des trous »)

Le principe : on prend des formes complètes, on y creuse des trous qui imitent
la visibilité réelle, et on apprend à reconstruire. **Le LiDAR aérien voit les
toits et presque pas les façades : c'est le même schéma de visibilité qu'une
orthophoto.** Les modèles de complétion du LiDAR aérien apprennent donc le bon
type de trou.

**Autoencodeurs masqués (pré-entraînement).**

- **Point-MAE** — Pang et al., ECCV 2022. Masque 60 à 80 % des patches de points
  et les reconstruit. Code **MIT**. — https://arxiv.org/pdf/2203.06604
- **Point-M2AE** — Zhang et al., NeurIPS 2022. Version multi-échelle. —
  https://arxiv.org/abs/2205.14401v1
- **PointGPT** — Chen et al., NeurIPS 2023. Prédiction autorégressive du patch
  suivant. —
  https://proceedings.neurips.cc/paper_files/paper/2023/hash/5ed5c3c846f684a54975ad7a2525199f-Abstract.html
- **DiffPMAE** — Li et al., ECCV 2024. Autoencodeur masqué avec décodeur de
  diffusion ; complétion évaluée selon le taux de masquage. —
  https://arxiv.org/abs/2312.03298v1
- **MAESTRO** — Labatie et al. (IGN), WACV 2026. Autoencodeur masqué
  multimodal pour l'observation de la Terre. Je n'ai pas pu vérifier si le DSM
  fait partie des modalités masquées. Code : github.com/ignf/maestro. —
  https://arxiv.org/pdf/2508.10894

**Complétion de nuages (encodeur-décodeur supervisé).**

- **PCN** — Yuan et al., 3DV 2018. Le premier réseau de complétion sur points
  bruts. — https://arxiv.org/abs/1808.00671v1
- **PoinTr / AdaPoinTr** — Yu et al., ICCV 2021 / TPAMI 2023. Traduction
  d'ensemble à ensemble par transformer. Code **MIT** ; le dépôt implémente
  aussi PCN, GRNet, TopNet, FoldingNet et SnowflakeNet. —
  https://arxiv.org/pdf/2301.04545
- **SnowflakeNet** — Xiang et al., ICCV 2021. Croissance de points qui garde les
  arêtes vives, adaptée aux toits plans. —
  https://openaccess.thecvf.com/content/ICCV2021/html/Xiang_SnowflakeNet_Point_Cloud_Completion_by_Snowflake_Point_Deconvolution_With_Skip-Transformer_ICCV_2021_paper.html
- **SeedFormer** — Zhou et al., ECCV 2022. —
  https://www.ecva.net/papers/eccv_2022/papers_ECCV/html/5127_ECCV_2022_paper.php
- **AnchorFormer** — Chen et al., CVPR 2023. Ancres dans les régions vues *et*
  manquantes. —
  https://openaccess.thecvf.com/content/CVPR2023/html/Chen_AnchorFormer_Point_Cloud_Completion_From_Discriminative_Nodes_CVPR_2023_paper.html

**Spécifique aux bâtiments.**

- **Building-PCC** — Gao, Peters, Stoter (TU Delft), ISPRS Annals 2024.
  50 000 bâtiments, LiDAR aérien réel troué contre LoD2 complets ; huit modèles
  pré-entraînés. F-score de 26,5 % (TopNet) à 70,2 % (AdaPoinTr) : **les réseaux
  génériques ne transfèrent que partiellement**. Code **GPL-3.0**. —
  https://arxiv.org/abs/2404.15644v1 ·
  https://github.com/tudelft3d/building-pcc-building-point-cloud-completion-benchmarks
- **APC2Mesh** — Akwensi, Bharadwaj, Wang, arXiv 2024. Complétion puis maillage
  pour des bâtiments dont **les façades sont entièrement occultées**. —
  https://arxiv.org/abs/2404.02391v1
- **Complétion + primitives** — Ding et al., Remote Sensing 17(3), 2025.
  Complétion par GAN, puis sélection de plans de type PolyFit → LoD2 léger. —
  https://www.mdpi.com/2072-4292/17/3/399/htm
- **RoofDiffusion** — Lo, Peters, Spellman, ECCV 2024. Diffusion sur **cartes de
  hauteur de toit 2.5D**, auto-supervisée sur des trous synthétiques. Tolère
  **99 % de points manquants et 80 % d'occlusion** ; une variante prédit aussi
  l'emprise. Code et données **MIT**. **C'est la brique la plus directement
  utilisable** dans une chaîne image → hauteur. — https://arxiv.org/abs/2404.09290v2
- **Complétion de carte de hauteur guidée par la structure** — Xiaduo Zhao,
  mémoire TU Delft, 2025. Diffusion en deux étapes : hauteur et lignes de toit
  liées, puis sur-échantillonnage. 160 000 bâtiments réels. Thèse CC BY 4.0,
  code non trouvé. —
  https://repository.tudelft.nl/file/File_e62e7731-1e7d-445f-a7ad-213b03b454b6
- **OCCDiff** — Sui, Liu, Zhang (HKU), arXiv 2025. Diffusion latente dans
  l'espace d'occupation, conditionnée par un LiDAR bruité. —
  https://arxiv.org/abs/2512.08506v1

**Bouchage de trous de MNE/DSM (raster).**

- **Bouchage de MNE par GAN** — Gavriil et al., IEEE GRSL 2018. WGAN avec
  attention contextuelle. — https://arxiv.org/pdf/1811.12693
- **Diff-DEM** — Lo & Peters. Diffusion (DDPM) ; bat GAN, krigeage et
  pondération inverse à la distance. —
  https://www.cise.ufl.edu/research/SurfLab/papers/24DEMpaint.pdf
- **Dfilled** — arXiv 2025. Bouchage de DSM *guidé par l'imagerie optique* : le
  cas le plus proche d'une chaîne de tuiles. — https://arxiv.org/html/2501.15440v2

**Inpainting dans un latent 3D génératif** : voir §3.15.

### 3.9 Façades : structure, grammaires, texture

C'est là que la crédibilité se joue, puisque rien n'est observé d'en haut.

- **Façades depuis le satellite** — Zhang, May, Aliaga, ECCV 2020. Apprend les
  paramètres d'une grammaire de façade à partir de vues satellite bruitées, et
  l'applique aux faces non vues. **C'est le plus proche de « tuile → façades
  plausibles ».** —
  https://www.ecva.net/papers/eccv_2020/papers_ECCV/html/3678_ECCV_2020_paper.php
- **FacaDiffy** — Fröch, Wysocki, Xia et al., ISPRS Annals 2025. Stable
  Diffusion personnalisé qui complète des « cartes de conflit » de façade ;
  entraîné sur des villes synthétiques. +22 % d'ouvertures détectées. —
  https://arxiv.org/pdf/2502.14940
- **Complétion LSTM** — Hensel, Goebbels, Kada, VISIGRAPP 2021. LSTM qui complète
  les grilles de fenêtres et de portes. — https://www.scitepress.org/Papers/2021/101944
- **FaçAID** — arXiv 2024. Transformer : façade segmentée → programme de
  grammaire hiérarchique. Voie neuro-symbolique, éditable. —
  https://arxiv.org/pdf/2406.01829
- **Inférence de grammaire** — Martinović & Van Gool, CVPR 2013 ; Wu et al.,
  SIGGRAPH 2014. Les fondations du « compléter par grammaire ». —
  https://openaccess.thecvf.com/content_cvpr_2013/html/Martinovic_Bayesian_Grammar_Learning_2013_CVPR_paper.html
  · https://history.siggraph.org/?p=103601
- **LoD3 depuis drone oblique** — Kim, Carpenter, Jung, ISPRS Archives 2025.
  Coquille LoD2, vues simulées, fenêtres YOLOv5, CityJSON. —
  https://isprs-archives.copernicus.org/articles/XLVIII-G-2025/797/2025/
- **Ouvertures pour 3DBAG** — Xia, Gao, Stoter, ISPRS Annals 2025. —
  https://isprs-annals.copernicus.org/articles/X-4-W6-2025/225/2025/
- **Fenêtres sans apprentissage** — Otepka et al., ISPRS Archives 2026.
  Grounding DINO + SAM2 ; meilleur sur ortho (F1 0,85). —
  https://isprs-archives.copernicus.org/articles/XLIX-B2-2026/517/2026/
- **TwinTex** — SIGGRAPH Asia 2023. Choix de photos par face plane, puis
  diffusion pour combler la texture manquante. — https://arxiv.org/abs/2309.11258
- **Texturation générative de villes** — UrbanWorld (arXiv 2024) ; *Virtual
  Urbanism* (arXiv 2026 : SD1.5 + LoRA + ControlNet-profondeur sur blocs
  extrudés). Des façades purement inventées. — https://arxiv.org/pdf/2407.11965 ·
  https://arxiv.org/pdf/2601.13846
- **BigSUR** — Kelly, Femiani, Wonka, Mitra, SIGGRAPH Asia 2017. Fusion
  d'emprises SIG, de maillage grossier et d'imagerie de rue en façades
  structurées. — https://geometry.cs.ucl.ac.uk/projects/2017/bigsur

### 3.10 Végétation

- **DeepForest** — Weinstein et al., MEE 2020. Boîtes de houppiers sur RVB
  aérienne ; standard, faible dans l'ombre profonde. —
  https://deepforest.readthedocs.io/en/v1.3.3/landing.html
- **Detectree2** — Ball, Coomes et al., RSEC 2023. Mask R-CNN, contours de
  houppiers (F1 de 0,64 à 0,74). —
  https://www.repository.cam.ac.uk/handle/1810/348046
- **OAM-TCD** — Restor, ETH et al., NeurIPS 2024 (Datasets). 5 072 images à 10 cm,
  environ 280 000 arbres, urbain et naturel. Données CC-BY 4.0, code Apache-2.0.
  — https://arxiv.org/html/2407.11743v1
- **SAM2 sans apprentissage pour les arbres** — arXiv 2025 (2506.03114). Les
  boîtes DeepForest servent de prompts. — https://arxiv.org/html/2506.03114v1
- **Pseudo-étiquettes LiDAR + SAM 2** — Pesonen et al., arXiv 2026. **La voie
  LiDAR HD → modèle RVB seul.** — https://papers.cool/arxiv/2602.13022
- **Arbres à l'échelle nationale** — Li et al., PNAS Nexus 2023. Position, couronne
  et hauteur de chaque arbre à partir d'images aériennes seules (Danemark). —
  https://pmc.ncbi.nlm.nih.gov/articles/PMC10096914
- **Tree-D Fusion** — Lee, Benes et al., ECCV 2024. Arbre 3D prêt pour la
  simulation à partir d'une seule image et du genre (diffusion, puis croissance
  par colonisation de l'espace). Environ 600 000 arbres. —
  https://www.arxiv.org/abs/2407.10330
- **SVDTree** — Li, Benes et al., CVPR 2024. Diffusion de voxels sémantiques à
  partir d'une image masquée d'arbre. —
  https://cvpr.thecvf.com/virtual/2024/poster/29963
- **AdTree** — Du et al. (TU Delft), Remote Sensing 2019. Squelette et cylindres
  à partir de LiDAR. — https://github.com/tudelft3d/AdTree
- **Végétation LoD2 nationale** — ISPRS Annals XI-4, 2026. Polygones forestiers
  extrudés et arbres isolés en prototypes mis à l'échelle (CityGML PlantCover). —
  https://isprs-annals.copernicus.org/articles/XI-4-2026/359/2026/isprs-annals-XI-4-2026-359-2026.pdf
- **Arbres procéduraux pour Virtual Singapore** — ISPRS Archives 2018. Modèles
  procéduraux par espèce, pour des millions d'arbres. —
  https://isprs-archives.copernicus.org/articles/XLII-4-W10/55/2018/isprs-archives-XLII-4-W10-55-2018.pdf

**Lecture.** Pour la crédibilité, la chaîne évidente est : houppier détecté +
hauteur (réseau ou canopée 1 m) → instance procédurale mise à l'échelle. Aucune
publication ne valide cette chaîne complète.

### 3.11 Sol, micro-relief, routes, ponts

- **Real-GDSR** — DLR, ISPRS Annals 2024. Super-résolution de DSM *guidée par
  l'image optique*, entraînée sur des données réelles. —
  https://www.arxiv.org/pdf/2404.03930
- **SIRAN** — Paul & Gupta, arXiv 2023. GAN : MNE haute résolution à partir
  d'imagerie multispectrale. — https://arxiv.org/abs/2311.16490v2
- **D-SRGAN** — Demiray et al., arXiv 2020. Sur-échantillonnage de MNE ×4 non
  guidé. — https://arxiv.org/abs/2004.04788
- **Raffinement de DSM par cGAN** — Bittner & Körner, CVPRW 2018 ; **WNet-cGAN**,
  IGARSS 2019 (fusion DSM et panchromatique). —
  https://openaccess.thecvf.com/content_cvpr_2018_workshops/w36/html/Bittner_Automatic_Large-Scale_3D_CVPR_2018_paper.html
  · https://arxiv.org/pdf/1903.03519v1.pdf
- **Routes procédurales depuis OSM** — IEEE 2022. Graphe routier OSM → routes 3D
  et mobilier urbain. — https://ieeexplore.ieee.org/document/9887441
- **Ponts, talus, déblais depuis OSM + MNE** : aucune publication évaluée trouvée.
  C'est un trou de la recherche (budget épuisé), pas une preuve d'absence.

### 3.12 Ombres et éclairage cuit : détection, retrait, albédo

- **AISD** — Luo, Li, Shen, ISPRS J. P&RS 167, 2020. 514 images aériennes
  annotées, sans cible sans ombre. — https://sres.whu.edu.cn/info/1035/19034.htm
- **S-EO** — Masquil, Marí, Ehret, Facciolo et al., CVPRW 2025. Masques d'ombre
  calculés à partir de *LiDAR + soleil* sur 702 tuiles ; environ 20 000 images
  WorldView-3 ; généralise à l'aérien et améliore EO-NeRF. —
  https://openaccess.thecvf.com/content/CVPR2025W/EarthVision/html/Masquil_S-EO_A_Large-Scale_Dataset_for_Geometry-Aware_Shadow_Detection_in_Remote_CVPRW_2025_paper.html
- **Ombres d'orthophoto multi-étapes** — Kobayashi et al. (PASCO), ISPRS Archives
  2026. Annotations à 20 cm et 5 cm. —
  https://isprs-archives.copernicus.org/articles/XLIX-B3-2026/141/2026/
- **SARU** — Bo, Lu, Chen et al., arXiv 2026. Détection plus retrait *sans
  apprentissage* : le rapport lumière directe / ambiante est estimé sur un
  superpixel voisin de même matériau. Code et données : oui. —
  https://arxiv.org/pdf/2604.25432
- **SR-Diffusion** — Chi et al., IEEE TGRS 2025. Diffusion avec a priori
  physiques (espace couleur séparant chromaticité et éclairement). —
  https://pure.nwpu.edu.cn/en/publications/a-diffusion-model-with-physically-plausible-gradient-for-remote-s/
- **BRIC-Net** — arXiv 2026 (2608.00682). — https://arxiv.org/pdf/2608.00682
- **Ombres de maquettes 3D comme étiquettes gratuites** — Zhou, Lindenbergh,
  Gorte (TU Delft), Remote Sensing 2019. Environ 95 % de précision globale sans
  annotation manuelle. —
  https://research.tudelft.nl/en/publications/automatic-shadow-detection-in-urban-very-high-resolution-images-u
- **Alpine Terrain Relighting** — Staats, TU Wien 2024. Les ombres
  d'entraînement sont tracées par rayons sur le MNE, et le MNE sert aussi
  d'entrée au réseau. —
  https://www.cg.tuwien.ac.at/research/publications/2024/staats-2024-atr/
- **Albédo aérien** — Song & Qin, ISPRS Annals 2022. Inversion d'un modèle de
  formation d'image extérieur. — https://arxiv.org/abs/2204.04142v1
- **Olbedo** — Song, Qin et al., CVPR 2026. 5 664 images drone avec albédo et
  ombrage : le premier jeu d'entraînement conséquent pour le délitage aérien. —
  https://arxiv.org/html/2602.22025v1
- **SRCT** — CVMP 2024. Retire les ombres cuites des textures de maillages de
  bâtiments par éclairage simulé et transfert de couleur. —
  https://visual-computing.com/publications/enhanced-illumination-adjustment-in-3d-outdoor-reconstructions-via-shadow-removal-through-color-transfer

**Pour la crédibilité, deux usages opposés de l'ombre.** (1) *Mesurer* : l'ombre
fixe la hauteur (§3.3). (2) *Effacer* : une texture de toit délitée se ré-éclaire
sous le soleil du rendu. Si l'on garde l'éclairage cuit, il faut que le soleil du
rendu **soit celui de la tuile**, sinon les ombres peintes contredisent les
ombres projetées. C'est l'incohérence la plus visible d'une 3D synthétique.

### 3.13 Génération « image satellite / carte → ville 3D »

- **Sat2Vid** — Li et al., ICCV 2021. Une image satellite et une trajectoire
  donnent une vidéo panoramique de rue, cohérente grâce à un nuage 3D. —
  https://openaccess.thecvf.com/content/ICCV2021/html/Li_Sat2Vid_Street-View_Panoramic_Video_Synthesis_From_a_Single_Satellite_Image_ICCV_2021_paper.html
- **Sat2Density** — Qian et al., ICCV 2023. Densité 3D apprise sur des paires
  satellite/sol, avec des masques de ciel en guise de supervision. —
  https://openaccess.thecvf.com/content/ICCV2023/html/Qian_Sat2Density_Faithful_Density_Learning_from_Satellite-Ground_Image_Pairs_ICCV_2023_paper.html
- **Sat2Scene** — Li, Li, Cui, Pollefeys, Oswald, CVPR 2024. Diffusion 3D
  éparse qui colore une géométrie issue du satellite, puis une scène rendable
  (environ 10 min par scène, 1,6 s par image). —
  https://openaccess.thecvf.com/content/CVPR2024/html/Li_Sat2Scene_3D_Urban_Scene_Generation_from_Satellite_Images_with_Diffusion_CVPR_2024_paper.html
- **Skyeyes** — Gao et al., WACV 2025 *(non vérifié)*. Séquences au sol à partir
  d'images aériennes, entraînées sur Unreal. — https://arxiv.org/abs/2409.16685v1
- **SatDreamer360** — Ze et al., ICLR 2026. Tri-plan à partir d'une image
  satellite, puis panoramas cohérents. — https://arxiv.org/abs/2506.00600v2
- **Skyfall-GS** — arXiv 2025. Splatting à partir d'images satellite
  multi-vues, puis une diffusion texte → image *synthétise les façades*. Il
  faut des vues obliques : le nadir pur reste ouvert. —
  https://arxiv.org/pdf/2510.15869
- **Sat2City** — Hua et al., ICCV 2025. Diffusion latente à voxels épars,
  conditionnée par une carte de hauteur ; villes synthétiques. Environ 1 min par
  ville. —
  https://openaccess.thecvf.com/content/ICCV2025/html/Hua_Sat2City_3D_City_Generation_from_A_Single_Satellite_Image_with_ICCV_2025_paper.html
- **Sat2City v2** — Hua et al., arXiv 2026. Latent 3D fondation adapté au
  satellite, sortie en **maillages texturés**. 16 241 paires réelles
  satellite/maillage ; seul de la famille à annoncer des **bancs DSM
  métriques**. — https://arxiv.org/pdf/2606.24138
- **MagicCity** — Yao et al., ICCV 2025. Image satellite → géométrie et texture
  → diffusion multi-vues → splatting ; jeu CityVista. Le passage par carte de
  hauteur exclut les porte-à-faux. —
  https://openaccess.thecvf.com/content/ICCV2025/papers/Yao_MagicCity_Geometry-Aware_3D_City_Generation_from_Satellite_Imagery_with_Multi-View_ICCV_2025_paper.pdf
- **Sat2RealCity** — Kang et al., arXiv 2025. Un bâtiment à la fois, avec des
  générateurs d'objets 3D pré-entraînés et des a priori OSM. —
  https://arxiv.org/abs/2511.11470v1
- **EarthCrafter** — Alibaba DAMO et al., AAAI 2026. Aerial-Earth3D : 50 000
  scènes de 600 × 600 m tirées de 45 millions d'images Google Earth ; VAE épars
  doubles (voxels et gaussiennes 2D) et flow matching. —
  https://arxiv.org/pdf/2507.16535
- **GeoTexBuild** — Wang, Yang, Wang, arXiv 2025. Emprise + croquis de hauteur →
  carte de hauteur → géométrie → texture multi-vues. Qualitatif seulement. —
  https://arxiv.org/html/2504.08419v2
- **AIM2PC** — Turki et al. (DLR), ISPRS Archives 2025. Une seule image
  aérienne → nuage complet du bâtiment, *murs compris*, par diffusion. Données
  publiées. — https://arxiv.org/abs/2503.18527v1
- **InfiniCity** — Lin et al., ICCV 2023. Carte vue de dessus → voxels → rendu
  neuronal, sans limite de taille. —
  https://openaccess.thecvf.com/content/ICCV2023/html/Lin_InfiniCity_Infinite-Scale_City_Synthesis_ICCV_2023_paper.html
- **CityDreamer** — Xie et al., CVPR 2024. Champs séparés pour les bâtiments et
  le fond ; données OSM + Google Earth ; licence S-Lab. —
  https://openaccess.thecvf.com/content/CVPR2024/html/Xie_CityDreamer_Compositional_Generative_Model_of_Unbounded_3D_Cities_CVPR_2024_paper.html
- **GaussianCity** — Xie et al., CVPR 2025. Représentation BEV-Point, 60× plus
  rapide que CityDreamer. —
  https://openaccess.thecvf.com/content/CVPR2025/html/Xie_Generative_Gaussian_Splatting_for_Unbounded_3D_City_Generation_CVPR_2025_paper.html
- **UrbanWorld** — Tsinghua et al., arXiv 2024. OSM → disposition → LLM
  multimodal urbain → actifs par diffusion 3D. — https://arxiv.org/pdf/2407.11965
- **Urban Architect** — Lu et al., arXiv 2024. Texte → ville 3D guidée par une
  disposition de primitives. — https://arxiv.org/abs/2404.06780v1
- Également vus : CityGen (évalué en FID/KID)
  https://astro.paperswithcode.com/paper/citygen-infinite-and-controllable-3d-city ;
  CityCraft
  https://www.themoonlight.io/en/review/citycraft-a-real-crafter-for-3d-city-generation

**Lecture pour la crédibilité.** C'est exactement le critère que vise cette
famille, et elle s'évalue d'ailleurs presque toujours en FID/KID et par études
utilisateurs. Trois réserves pour Tuile :

1. **Des carrés isolés.** Aucune ne traite la continuité entre tuiles voisines
   ni entre niveaux de détail.
2. **Pas de déterminisme.** Rien n'est garanti d'un rendu à l'autre.
3. **La provenance des données.** Plusieurs sont entraînées sur des rendus
   Google Earth (§5).

### 3.14 Modélisation procédurale inverse

- **Inverting Procedural Buildings** — Dax, Berbel, Stria, Guibas, Bergmann,
  CVPRW 2025. Un transformer entraîné sur des bâtiments procéduraux synthétiques
  produit un *programme* à partir d'un nuage de points. Inpainting cohérent avec
  la structure. — https://arxiv.org/abs/2501.17044v1
- **ArcPro** — Huang et al., CVPR 2025. Langage hiérarchique de disposition
  architecturale. 1 090 bâtiments en environ 37 s sur une RTX 4090 ; essais en
  entrée multi-vues. —
  https://openaccess.thecvf.com/content/CVPR2025/html/Huang_ArcPro_Architectural_Programs_for_Structured_3D_Abstraction_of_Sparse_Points_CVPR_2025_paper.html
- **Nishida et al.** — Nishida, Bousseau, Aliaga, Eurographics 2018. Photo +
  silhouette → grammaires de masse et de façade. C'est le canon
  image → programme. — https://diglib.eg.org/handle/10.1111/cgf13372
- **Grammaires de façade inverses** — Wu et al., arXiv 1308.0419. —
  https://arxiv.org/pdf/1308.0419
- **BuildingBlock** — Zhejiang Univ., SIGGRAPH 2025. Disposition de boîtes par
  diffusion → règles écrites par LLM → génération procédurale. —
  https://arxiv.org/pdf/2505.04051
- **ProxyBuild** — Tang, Li, Fan, arXiv 2026. Bâtiments guidés par texte via des
  proxys procéduraux ancrés sur un maillage. — https://arxiv.org/pdf/2609.23386
- Également vus :
  - Proc-GS (CVPR 2025) : https://arxiv.org/pdf/2412.07660
  - Demir et al. (3DV 2016) : https://www.cs.purdue.edu/homes/bbenes/papers/Demir163DV.pdf
  - SiZeUp (2026) : https://arxiv.org/pdf/2608.22821
  - Quick-Pro-Build : https://diglib.eg.org/items/769a5a96-722c-4520-a5b2-6f1a355fed04/full

**Lecture.** Le procédural est la voie naturelle de la crédibilité *contrôlée*.
Il est déterministe par graine, cohérent par quartier (mêmes règles, mêmes
paramètres) et compact. Il est éditable et se décline en niveaux de détail par
construction. La chaîne « image → hauteur → programme » n'existe pas d'un seul
tenant (§9).

### 3.15 Modèles fondation 3D génériques (complétion et inpainting)

- **TRELLIS** — Xiang et al. (Microsoft Research), CVPR 2025. Latents structurés
  (SLAT) sur voxels épars, décodables en champs de radiance, gaussiennes ou
  maillages. 2 milliards de paramètres, 500 000 objets, environ 10 s par actif,
  édition locale. Code : oui. — https://arxiv.org/pdf/2412.01506
- **InpaintSLat** — Seoul National Univ., arXiv 2026. Inpainting 3D sans
  apprentissage dans le latent TRELLIS, par optimisation du bruit initial. —
  https://arxiv.org/pdf/2605.00664
- **Easy3E** — USTC, CVPR 2026. Édition directe à partir d'une vue éditée et
  d'un masque de région. — https://arxiv.org/abs/2602.21499
- **ES3D** — arXiv 2026. Édition par composant, puis inpainting TRELLIS. —
  https://arxiv.org/pdf/2608.15749
- **Hunyuan3D 2.0** — Tencent, arXiv 2025. Forme par transformer de diffusion,
  plus un peintre de texture sur n'importe quel maillage ; poids ouverts. —
  https://arxiv.org/abs/2501.12202v1
- **BuildingWorld** — Huang, Wang, Wang, AAAI 2026. Environ **5 millions de LoD2**
  dans 44 villes, avec LiDAR réel et simulé, plus un générateur « Cyber City ».
  C'est le corpus le plus plausible pour un générateur spécialisé bâtiments
  (licence non vérifiée). — https://arxiv.org/abs/2511.06337v1
- Également vus :
  - Fuse3D : https://arxiv.org/pdf/2602.17040
  - Points-to-3D : https://arxiv.org/pdf/2603.18782
  - WorldGrow : https://arxiv.org/pdf/2510.21682
  - NANO3D : https://arxiv.org/pdf/2510.15019
- Aucun modèle fondation 3D *spécialisé bâtiments* trouvé.

---

## 4. Jeux de données de référence

| Jeu | Contenu | Rôle ici | Licence |
|---|---|---|---|
| **FLAIR-HUB** (IGN) | 2 528 km² de France : ortho RVBI 20 cm, **DSM 20 cm** du même vol, MNT 1 m, 19 classes, Sentinel-1/2, SPOT, ortho historique | **entraînement image → hauteur + classe** clé en main | à vérifier : CC BY-SA 4.0 ou Etalab 2.0 ([HF](https://huggingface.co/datasets/IGNF/FLAIR-HUB), [arXiv](https://arxiv.org/pdf/2506.07080)) |
| FLAIR #1 (IGN) | ortho et occupation du sol | segmentation | Etalab 2.0 ([HF](https://huggingface.co/datasets/IGNF/FLAIR)) |
| **LiDAR HD** (IGN) | nuage classé ≥10 pts/m² (sol, végétation, bâti, ponts…), presque toute la France | vérité terrain indépendante ; pseudo-étiquettes d'arbres | Etalab 2.0 ([data.gouv](https://www.data.gouv.fr/datasets/lidar-hd-ign-2022)) |
| **BD ORTHO / BD TOPO / RGE ALTI** (IGN) | ortho, bâti vectoriel avec hauteur, MNT 1 m | entrée ouverte ; emprises ; terrain | Etalab 2.0 depuis 2021 |
| ISPRS Vaihingen / Potsdam | vraies orthos 9 et 5 cm + DSM/nDSM | banc classique de hauteur monoculaire | usage recherche ([ISPRS](https://www.isprs.org/resources/datasets/benchmarks/UrbanSemLab/semantic-labeling.aspx)) |
| IEEE DFC2018 (Houston) | RVB 5 cm, LiDAR multispectral, hyperspectral | banc nDSM | accès ouvert ([DataPort](https://ieee-dataport.org/open-access/2018-ieee-grss-data-fusion-challenge-fusion-multispectral-lidar-and-hyperspectral-data)) |
| US3D / DFC2019 | WorldView-3 multi-vues + DSM LiDAR (Jacksonville, Omaha) | multi-vues satellite, géopose | licence non vérifiée ([DataPort](https://ieee-dataport.org/open-access/urban-semantic-3d-dataset)) |
| DFC2023 | type de toit (12) + hauteur, 17 villes, environ 300 000 bâtiments | a priori de toit | DataPort ([lien](https://ieee-dataport.org/competitions/2023-ieee-grss-data-fusion-contest-large-scale-fine-grained-building-classification)) |
| GeoNRW | 7 783 tuiles de 1 km, ortho 1 m + DSM LiDAR | hauteur, Allemagne | dl-de/by-2-0 ([DataPort](https://ieee-dataport.org/open-access/geonrw)) |
| BONAI | 268 958 bâtiments, toit + emprise + décalage | indice de dévers | code et données ouverts ([arXiv](https://arxiv.org/pdf/2204.13637)) |
| OmniCity | New York, satellite + panoramas, plus de 100 000 images | emprise, hauteur, multi-vues | ([CVF](https://openaccess.thecvf.com/content/CVPR2023/papers/Li_OmniCity_Omnipotent_City_Understanding_With_Multi-Level_and_Multi-View_Images_CVPR_2023_paper.pdf)) |
| **AHN + 3DBAG** (Pays-Bas) | LiDAR national CC0 ; 10 millions de LoD1.2–2.2 | vérité LoD2 ; source de formes complètes | AHN CC0, 3DBAG CC BY 4.0 ([3DBAG](https://3d.bk.tudelft.nl/projects/3dbag/)) |
| Building-PCC | 50 000 bâtiments troués / complets | complétion | code GPL-3.0 ([arXiv](https://arxiv.org/abs/2404.15644v1)) |
| Building3D | environ 160 000 bâtiments estoniens, filaires de toit | toits | licence non vérifiée ([ICCV](https://openaccess.thecvf.com/content/ICCV2023/papers/Wang_Building3D_A_Urban-Scale_Dataset_and_Benchmarks_for_Learning_Roof_Structures_ICCV_2023_paper.pdf)) |
| BuildingWorld | environ 5 millions de LoD2, 44 villes | pré-entraînement de formes | recherche, non vérifié ([arXiv](https://arxiv.org/abs/2511.06337v1)) |
| RoofDiffusion | 13 000 toits | complétion 2.5D | MIT ([arXiv](https://arxiv.org/abs/2404.09290v2)) |
| PolyGNN synth. | plus de 500 000 bâtiments (Munich) | formes | MIT ([arXiv](https://arxiv.org/abs/2307.08636v2)) |
| OAM-TCD | 280 000 arbres à 10 cm | houppiers | CC-BY 4.0 ([arXiv](https://arxiv.org/html/2407.11743v1)) |
| S-EO | 702 tuiles, ombres tirées de la géométrie | ombres | licence non vérifiée ([CVF](https://openaccess.thecvf.com/content/CVPR2025W/EarthVision/html/Masquil_S-EO_A_Large-Scale_Dataset_for_Geometry-Aware_Shadow_Detection_in_Remote_CVPRW_2025_paper.html)) |
| AISD | 514 images d'ombres | ombres | ([lien](https://sres.whu.edu.cn/info/1035/19034.htm)) |
| Olbedo | 5 664 images drone, albédo / ombrage | délitage | ([arXiv](https://arxiv.org/html/2602.22025v1)) |
| SUM (Helsinki) | maillage texturé étiqueté | voir la note précédente | ([TU Delft](https://3d.bk.tudelft.nl/projects/meshannotation/)) |
| H3D (Hessigheim) | LiDAR drone + maillage, multi-époques | | ([Stuttgart](https://ifpwww.ifp.uni-stuttgart.de/benchmark/hessigheim/Default.aspx)) |
| MatrixCity | ville synthétique UE5, 67 000 images aériennes | données synthétiques propres | ([arXiv](https://arxiv.org/abs/2309.16553v1)) |
| UrbanScene3D | 16 scènes, LiDAR | | ([arXiv](https://arxiv.org/abs/2107.04286v2)) |
| swissSURFACE3D + SWISSIMAGE | LiDAR classé + ortho 10 cm, Suisse | alternative à la France | ouvert avec attribution ([opendata.swiss](https://opendata.swiss/en/dataset/swisssurface3d-die-klassifizierte-punktwolke-der-schweiz)) |
| USGS 3DEP + NAIP | MNE LiDAR 1 m + ortho, États-Unis | alternative | domaine public (NAIP non vérifié) |
| Copernicus DEM GLO-30 | **DSM** mondial 30 m | repli grossier | licence libre avec attribution |
| Overture (bâtiments) | emprises mondiales, hauteurs partielles | emprises hors de France | ODbL ([notes](https://overturemaps.org/overture-december-2023-release-notes/)) |
| Open Buildings 2.5D, GlobalBuildingAtlas, 3D-GloBFP | hauteurs mondiales | a priori, repli | CC-BY 4.0 / ODbL |

**Le couple recommandé pour démarrer.** FLAIR-HUB pour entraîner (ortho et DSM
du même vol : cohérence temporelle). LiDAR HD sur un département tenu à l'écart
pour mesurer. BD TOPO pour les emprises.

---

## 5. Licences : ce qui impose la trajectoire

### Sources d'imagerie

- **Tuiles 3D photoréalistes Google.** Les objets « extraits, décalqués ou
  dérivés à la main ou par machine » sont interdits ; extraction et scraping
  aussi. — https://developers.google.com/maps/documentation/tile/policies ·
  copie : https://cesium.com/legal/terms-for-google/
- **Google Earth Studio.** Usage recherche, éducation et associatif avec
  attribution ; aucune licence commerciale. L'imagerie ne doit pas être
  modifiée significativement sans indiquer qu'il s'agit d'une simulation. Cela
  touche **tous les jeux dérivés** : BungeeNeRF, AerialMegaDepth, les données de
  CityDreamer, Aerial-Earth3D d'EarthCrafter. — https://www.google.com/earth/studio/faq/
- **Bing.** La permission de décalquer l'imagerie aérienne vaut **pour OSM
  seulement**, et les conditions interdisent de copier ou mettre en cache. Le
  service grand public est retiré (fin Enterprise en juin 2028). —
  https://wiki.openstreetmap.org/wiki/Bing · https://microsoft.com/maps/assets/docs/terms.aspx
- **Esri World Imagery.** Décalque autorisé pour créer des vecteurs OSM ; tout
  autre usage relève des conditions Esri (source secondaire). —
  https://wiki.openstreetmap.org/wiki/Esri
- **Mapbox Satellite.** Conditions de 2014 : dérivation non commerciale. Elles
  sont obsolètes, je n'ai pas trouvé les conditions actuelles. —
  https://lists.openstreetmap.org/pipermail/talk-fr/2014-April/067592.html
- **Maxar Open Data.** CC BY-NC 4.0, seulement lors de catastrophes. —
  https://registry.opendata.aws/maxar-open-data/
- **Modèle de dérogation OSMF.** C'est l'étalon de ce que veut dire « dérivation
  autorisée ». —
  https://osmfoundation.org/wiki/Licence/Waiver_and_Permission_Templates/Template_text_for_aerial_imagery_waivers

**Conséquence.** Développer et démontrer **uniquement sur BD ORTHO, SWISSIMAGE,
NAIP** ou sur ses propres captures. L'outil reste générique (toute tuile
d'imagerie). La responsabilité de la source reste à l'utilisateur, comme pour
tout client 3D Tiles. Les permissions « décalque pour OSM » n'autorisent ni la
3D dérivée ni l'entraînement.

### Code

Projet sous MIT OR Apache-2.0 : ni dépendance liée ni portage de code GPL.

- **GPL-3.0** : roofer, City3D, PolyFit (et le paquet CGAL), HEAT, le code de
  Building-PCC et le code de lignes de toit TU Delft. Usage possible comme
  **binaire externe** pour expérimenter ; sinon réimplémentation propre à partir
  des articles (inspiration algorithmique libre, règle 4 de `CLAUDE.md`).
- **Permissif** : RoofDiffusion, PolyGNN, HiSup, Pix2Poly, PoinTr/AdaPoinTr,
  Point-MAE (MIT) ; Frame Field Learning (BSD-3) ; OAM-TCD (Apache-2.0) ;
  canopée ETH (MIT).
- **Poids de modèles** : la licence des poids est distincte de celle du code
  (Depth Anything V2 : selon la taille du modèle, *à vérifier*). Rien n'est à
  embarquer dans le dépôt.

---

## 6. Mesurer la crédibilité

La littérature générative mesure surtout en FID/KID et par études
utilisateurs. La littérature de reconstruction mesure contre du LiDAR, mais en
2.5D seulement : **personne ne mesure des façades générées contre une vérité**.
Pour Tuile, un banc en cinq instruments, qui ne remplace pas le sixième :

1. **Re-projection nadir.** Le rendu vertical de la 3D, au soleil de la tuile,
   comparé à la tuile source (LPIPS, SSIM). Il attrape les toits décalés et les
   textures étirées.
2. **Cohérence des ombres.** L'IoU entre le masque d'ombre calculé (3D + soleil
   estimé) et le masque détecté dans la tuile. Il attrape les hauteurs
   aberrantes.
3. **Vraisemblance des hauteurs.** Distribution par classe, et écart au LiDAR HD
   là où il existe. Ce n'est pas un objectif, c'est un garde-fou.
4. **Perceptuel oblique.** FID/KID de rendus obliques contre des photos obliques
   réelles *sous licence ouverte* (jeux drone/obliques ouverts : à trouver, §9).
5. **Stabilité.** Différence entre frames successives d'un vol, différence entre
   niveaux de détail au seuil de bascule, et octets identiques pour un même
   pack.
6. **Regarder le film.** C'est la règle de `CLAUDE.md` : tous les compteurs
   ci-dessus ont des angles morts, et une 3D « crédible » se juge à l'œil.

---

## 7. Esquisse d'une chaîne crédible pour Tuile

Ce n'est pas un plan, c'est la chaîne que la littérature rend plausible
aujourd'hui, avec la brique la plus mûre à chaque étage.

```
tuile imagerie (z17–19) + tuile terrain             [déjà dans Tuile]
  │
  ├─① sémantique + hauteur au-dessus du sol par pixel
  │     Depth Anything V2 / MoGe-2 affiné sur FLAIR-HUB (§3.1)
  │     + tête de segmentation (bâti, arbre, eau, route, sol)
  │
  ├─② soleil du lot de prise de vue
  │     recherche des 2 angles qui font coïncider ombres calculées et
  │     ombres détectées (§3.3, trou de la littérature) → recale l'échelle
  │
  ├─③ objets
  │     bâtiments : emprise (BD TOPO, sinon HiSup/Pix2Poly)
  │                 + carte de hauteur de toit complétée (RoofDiffusion)
  │                 + toit paramétrique (squelette droit / Point2WSS-like)
  │     arbres    : houppiers (OAM-TCD/Detectree2) + hauteur → instance procédurale
  │     sol       : micro-relief guidé par l'image (Real-GDSR-like), routes OSM
  │
  ├─④ invention contrôlée
  │     façades : grammaire procédurale, paramètres tirés du quartier
  │               (style par îlot, graine = identifiant stable du bâtiment)
  │     texture : toit = tuile (délitée ou non, cf. §3.12) ; façades = diffusion
  │               conditionnée par la couleur du toit et du quartier, figée par graine
  │
  └─⑤ sortie 3D Tiles
        glb par tuile, niveaux : LoD1 extrudé (loin) → LoD2 → détail de façade (près)
        métadonnées par face : observé / inféré / inventé
        repli : la tuile drapée plate, toujours disponible (jamais de trou)
```

**Choix structurants.**

- **Par objet, pas par tuile.** Un bâtiment est généré une fois, avec un
  identifiant stable : une emprise BD TOPO, ou un hash d'emprise détectée. Il est
  ensuite *découpé* dans les tuiles. C'est la seule façon d'éviter les raccords
  entre tuiles, que tous les générateurs de §3.13 ignorent.
- **Déterminisme.** Toute part aléatoire (style, fenêtres, diffusion) est tirée
  d'une graine dérivée de l'identifiant. Même pack → mêmes octets, comme les
  rendus de ferme l'exigent.
- **Le procédural d'abord, la diffusion en finition.** Le procédural garantit la
  stabilité et les niveaux de détail ; la diffusion n'apporte que la texture et
  la variété.
- **Hors du cœur.** C'est lourd (réseaux, GPU) : un consommateur et producteur
  de tuiles, pas `tuile-core` (règle 1). Deux emplacements possibles, à décider
  plus tard :
  - **précalcul** : un outil de « bake », cousin de `tuile-bake` ;
  - **génération à la volée** derrière `tuile-server`, avec son cache
    enfichable. C'est la piste post-v1 déjà nommée dans la vision.
- **Précision** : ancrage en f64, géométrie locale en f32 après recentrage
  (règle 7).

---

## 8. Expériences qui trancheraient

Elles sont ordonnées par rapport valeur/coût. Chacune produit des **images
regardées**, pas seulement un score.

1. **Hauteur monoculaire sur FLAIR-HUB** (environ 2 semaines, 1 GPU). Affiner
   Depth Anything V2 ou MoGe-2 sur ortho → nDSM. Tester sur un département tenu
   à l'écart, contre LiDAR HD. Ce qui tranche : erreur absolue moyenne par
   classe et sous-estimation des tours. Afficher le résultat extrudé dans le
   viewer.
2. **Soleil à partir des ombres** (environ 1 semaine). Sur 1 km², chercher les 2
   angles qui maximisent l'IoU ombre calculée / ombre détectée. Puis recaler
   l'échelle de la hauteur prédite. Ce qui tranche : le recalage par ombre
   améliore-t-il la vraisemblance perçue *et* l'erreur LiDAR ?
3. **Toits complétés** (environ 2 semaines). RoofDiffusion (MIT) sur les cartes
   de hauteur de l'expérience 1, avec emprises BD TOPO, puis toit paramétrique.
   Ce qui tranche : la re-projection nadir et les films.
4. **Façades procédurales** (environ 3 semaines). Grammaire simple (étages,
   travées, ouvertures), paramètres par îlot, graine par bâtiment. Ce qui
   tranche : un film en rase-mottes, à regarder.
5. **Arbres** (environ 1 semaine). Houppiers OAM-TCD + hauteur → prototypes mis à
   l'échelle. Ce qui tranche : un film, en saison feuillée.
6. **Comparaison avec un générateur de bout en bout** (Sat2City v2 ou
   MagicCity, si les poids sont disponibles) sur la même zone, pour savoir ce
   qu'apporte la chaîne modulaire.

---

## 9. Trous de la littérature et questions ouvertes

- **Le soleil à partir des ombres en vue verticale, sans métadonnées.** Introuvable.
  Toute la littérature « hauteur par l'ombre » lit le soleil dans les
  métadonnées, absentes d'une tuile web. Or c'est la clé de voûte de la
  crédibilité (§3.12).
- **L'échelle métrique des modèles fondation.** Elle est résolue par
  affinage supervisé, ou par un DSM partiel (Prior2DSM). L'ombre et la
  résolution connue de la tuile comme a priori d'échelle gratuit paraissent
  inexplorées.
- **Les raccords entre tuiles et entre niveaux.** Seuls deux articles traitent
  l'assemblage de patches. Aucun générateur 3D ne traite la continuité ni la
  cohérence multi-niveau.
- **Les façades depuis le nadir.** Un seul travail à partir du satellite (Zhang,
  May, Aliaga 2020). Skyfall-GS exige des vues obliques. Aucune métrique admise
  pour « plausible mais non observé ».
- **Image → programme de bâtiment.** Dax et al. et ArcPro partent de nuages de
  points ; la lignée image → programme est mince depuis 2018. La chaîne « hauteur
  monoculaire → nuage → ArcPro/Dax → détail par TRELLIS/InpaintSLat » est à
  tester.
- **La pseudo-stéréo entre sources ou niveaux.** Les dévers différents de deux
  orthos de dates différentes comme indice de hauteur : aucune publication
  trouvée.
- **Ponts et ouvrages depuis OSM + MNE** : pas de travail évalué trouvé.
- **Des photos obliques ouvertes pour le FID** : à identifier (drone ou aérien
  oblique sous licence ouverte).
- **Licences à vérifier** avant tout usage : FLAIR-HUB (CC BY-SA ou Etalab),
  S-EO, US3D, Building3D, BuildingWorld, la canopée Meta, et les poids des
  modèles fondation.

---

## 10. Relation avec la note précédente

[`shape-from-photorealistic-tiles.md`](shape-from-photorealistic-tiles.md) part
des **maillages** photogrammétriques (déjà 3D, sans objets). La présente note
part des **rasters** (sans 3D). Les deux convergent à partir de l'étage ③ :
emprises, toits paramétriques, complétion, façades. La chaîne raster est plus
générale, puisque l'imagerie couvre toute la planète et les maillages seulement
les villes. Elle est aussi mieux servie par les données ouvertes françaises.
