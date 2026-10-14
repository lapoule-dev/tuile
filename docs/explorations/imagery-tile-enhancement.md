<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Améliorer les tuiles d'imagerie — état de l'art et ce qui est actionnable

Note de recherche exploratoire, rédigée le 2026-10-14. **Rien n'est implémenté
ici.** La note rassemble la littérature, théorique et appliquée, sur
l'amélioration des tuiles raster d'imagerie (orthophoto, satellite) servies en
pyramide :

- **détail** : netteté, super-résolution ;
- **flou, bruit et artefacts** de compression ;
- **luminosité, exposition et colorimétrie** ;
- **atmosphère** : voile, nuages ;
- **contenu indésirable** : objets transitoires, trous, dévers.

Elle dit d'abord **ce qui est actionnable dans Tuile** (§1).

Elle **complète** [`docs/18-color-harmonization.md`](../18-color-harmonization.md)
sans le répéter. Le doc 18 couvre l'égalisation de mosaïque classique : Wallis,
Reinhard, Brown & Lowe, Poisson, multibande, normalisation radiométrique
relative, correction topographique, référence Sentinel-2. Le crate
`tuile-radiometry` en implémente le cœur. Ici, on traite tout le reste, et les
méthodes récentes.

**Méthode et limites.** Environ 150 recherches web réparties sur cinq axes.
**Aucun article n'a été lu en texte intégral** : arxiv.org, cvf.com,
copernicus.org et huggingface.co ne résolvaient pas depuis l'environnement de
recherche. Chaque entrée repose sur un résumé ou un extrait. *(non vérifié)*
signale un détail non recoupé. Les licences de code et de poids sont à relire
avant tout usage.

---

## 0. En une page

- **Une tuile web a subi une chaîne de dégradations** : la FTM du capteur,
  l'orthorectification, la reprojection en Web Mercator, la réduction de
  pyramide, puis un JPEG 4:2:0. **Tuile ajoute ensuite trois étapes de plus**
  (reprojection bilinéaire, mipmaps en sRGB, échantillonnage sans anisotropie,
  §1). Ces dernières sont à nous, donc corrigibles sans IA, en heures.
- **La couleur a déjà sa fondation** (`tuile-radiometry`). La littérature récente
  apporte un motif qui lui est taillé sur mesure : **paramètres basse
  résolution, application pleine résolution**. Ce sont les LUT 3D adaptatives,
  la grille bilatérale de HDRNet, PCT-Net. Ancrés dans une grille géographique,
  ils deviennent un « champ » comme `GainField`, appliqué par simple lecture de
  texture (§3.10).
- **La super-résolution est mûre, mais elle a un prix.** La théorie
  (perception–distorsion, Blau & Michaeli 2018) interdit d'avoir à la fois le
  plus net et le plus fidèle. Les méthodes en une étape et **déterministes**
  (OSEDiff, PiSA-SR, Apache-2.0) rendent la super-résolution générative
  compatible avec un cache et une ferme. **Genesis** (SIGSPATIAL 2026) traite
  littéralement la complétion de quadtree de tuiles (§3.2, §3.3).
- **Au-delà du niveau maximal, la contrainte qui protège est la cohérence de
  pyramide.** L'enfant généré, réduit, doit redonner le parent ; c'est le
  principe de CEM (Bahat & Michaeli 2020). C'est le même principe que la
  radiométrie de Tuile, appliqué au détail (§3.3).
- **Le retrait des artefacts JPEG est un levier sous-estimé.** QGAC lit la
  matrice de quantification du fichier, donc gratuite. FBCNN modélise le double
  JPEG, c'est-à-dire la perte générationnelle des fonds de carte. Il faut le
  faire *avant* toute super-résolution (§3.7).
- **Nuages et voile** : la littérature est massivement Sentinel-2 avec SAR, NIR
  ou SWIR, soit des bandes que des tuiles RVB n'ont pas. Pour du RVB seul, ce qui
  transfère : le canal sombre (DCP), HOT/HTM, les réseaux RVB, et surtout **le
  composite médian ou « meilleur pixel »** quand une pile temporelle existe
  (§3.14–3.16).
- **Les trous de la littérature sont précisément ceux d'un serveur de tuiles :**
  - raccords entre tuiles générées indépendamment ;
  - cohérence entre niveaux de zoom ;
  - déterminisme ;
  - hallucination mesurée à l'échelle sub-métrique ;
  - aucune base d'évaluation humaine (MOS) d'orthophotos (§6).
- **Licences.** SUPIR, StableSR (probablement), FluxSR et les images haute
  résolution de WorldStrat sont non commerciaux. DSen2-CR est en GPL-3.0.
  Real-ESRGAN (BSD-3), OSEDiff et PiSA-SR (Apache-2.0, sur une base SD 2.1
  OpenRAIL), NAFNet (MIT, non vérifié) sont utilisables (§5).

---

## 1. Ce qui est actionnable dans Tuile

### 1.1 La chaîne de dégradation, et la part qui est à nous

```
capteur (FTM, bruit) → orthorectification (rééchantillonnage) → mosaïquage
(lignes de raccord, dates, sources) → reprojection Web Mercator → réduction
de pyramide → JPEG 4:2:0 q≈75–85 (parfois ré-encodé)            [fournisseur]
──────────────────────────────────────────────────────────────────────────────
décodage (image::load_from_memory, raster.rs:1520)
→ reprojection géographique, bilinéaire        (raster.rs:645 / :681)
→ mipmaps moyennés en sRGB                       (raster.rs:1593)
→ échantillonnage trilinéaire, sans anisotropie  (tuile-wgpu context.rs:216)  [Tuile]
```

Aucune publication ne modélise la chaîne complète du fournisseur (§6). Les trois
dernières étapes, en revanche, sont dans notre code.

### 1.2 Déjà en place

`tuile-radiometry` fait l'harmonisation de tonalité : un champ de gains basse
fréquence chaîné le long de la pyramide, un réglage par niveau (point noir, gain,
contraste, saturation) et un look de film. La luminosité et la colorimétrie
partent donc d'une base, pas d'une page blanche. Les étapes 2 et 3 du doc 18
(ancrage sur une référence, résolution globale hors ligne) sont à confirmer dans
le code.

### 1.3 Niveau 1 — au rendu, sans IA, déterministe (heures à jours)

| # | Action | Où | Ce que ça corrige | Références |
|---|---|---|---|---|
| 1 | **Filtrage anisotrope** dans le viewer (`anisotropy_clamp`, déjà à 16 dans `tuile-film-gpu`) | `tuile-wgpu/src/context.rs:216` | le flou du sol vu en oblique, le plus visible du viewer | Manson & Schaefer ; Heckbert (EWA) §3.1 |
| 2 | **Mipmaps en lumière linéaire** (LUT déterministes de la radiométrie) | `tuile-core/src/raster.rs:1593` | le ternissement du détail contrasté au loin, que le commentaire du code reconnaît | §3.1 |
| 3 | **Noyau de reprojection** plus fin (bicubique ou Lanczos-2), ou reprojection à partir de z+1 | `raster.rs:681` | l'adoucissement à chaque rééchantillonnage | Nehab & Hoppe ; Mitchell–Netravali §3.1 |
| 4 | **Netteté dans le look** : filtre guidé ou laplacien local, uniforme, optionnel | shader du look (doc 18 §4) | le manque de « piqué » | He 2010 ; Paris 2011 ; Aubry 2014 §3.1 |

Les numéros 1 et 4 interagissent : l'accentuation de netteté crée du
scintillement en vue rasante si elle est mal filtrée. Il faut mesurer sur un
film.

### 1.4 Niveau 2 — hors ligne, stocké en couche dérivée (semaines)

**Principe.** Tout traitement lourd ou appris tourne **une fois**, dans
`tuile-bake` (« sur une machine qui a le temps »). Le résultat est stocké comme
**couche versionnée** dans `tuile-tile-server` : PMTiles, qui accepte déjà
jpeg, webp, avif et `other`. Il est lu par un `ImageryProvider` ordinaire. Cela
règle trois choses :

- **le déterminisme** : l'inférence GPU n'est pas reproductible au bit près, mais
  les octets stockés le sont, comme les epochs ;
- **le coût**, payé une seule fois ;
- **le noir** : si la couche dérivée manque, on retombe sur la source.

Le digest des paramètres entre dans l'identité de drapé, comme le prévoit le
doc 18 §5.

Candidats, par ordre de rapport valeur/coût :

1. **Retrait des artefacts JPEG** (QGAC, FBCNN ; §3.7) sur les niveaux fins. Il
   est préalable à tout le reste, puisque la super-résolution amplifie les blocs.
2. **Super-résolution « overzoom »** : générer z_max+1 et z_max+2 là où la caméra
   descend sous la résolution de la source (films en rase-mottes). On utilise
   une régression fidèle (HAT, NAFNet) ou une diffusion en une étape
   déterministe (OSEDiff, PiSA-SR). La contrainte de cohérence est de type CEM :
   la réduction de l'enfant redonne le parent (§3.2–3.3).
3. **Champ de couleur appris** : étendre `GainField` vers un champ de LUT 3D ou
   une grille bilatérale ancrée géographiquement (§3.10). C'est le même
   emplacement et le même mode d'application, avec plus d'expressivité.
4. **Composite sans nuages ni voile** pour la couche de référence satellite
   (médiane ou meilleur pixel ; §3.16). Les orthos aériennes ont rarement des
   nuages.

**Limite de licence.** Une couche dérivée *stockée* est une œuvre dérivée
persistante. Elle est acceptable pour BD ORTHO, Sentinel-2, NAIP ou sa propre
imagerie. Elle ne l'est pas pour une imagerie commerciale dont les conditions
interdisent copie, cache ou dérivation. Pour celle-là, on reste au « pixel
modifié en mémoire client » du doc 18.

### 1.5 Niveau 3 — recherche, pas encore actionnable

- super-résolution générative à fort facteur (×8 et plus, invention de routes
  ou de toits) ;
- normalisation saisonnière ;
- retrait d'objets transitoires ;
- correction du dévers dans l'image ;
- délitage pour un rendu ré-éclairé.

Les références sont aux §3.2–3.3 et §3.17–3.18, et les trous au §6.

**Ordre conseillé :**
1. anisotropie ;
2. mipmaps linéaires ;
3. un film avant/après, **regardé** ;
4. noyau de reprojection ;
5. squelette « couche dérivée hors ligne », avec le retrait des artefacts JPEG
   puis l'overzoom comme premiers clients.

---

## 2. Principes qui découlent de la littérature

1. **Corriger la basse fréquence, préserver ou restaurer la haute fréquence,
   n'inventer qu'en dernier.** C'est l'échelle de risque de la
   perception–distorsion. Elle s'aligne avec le principe du doc 18.
2. **Paramètres à basse résolution, application à pleine résolution.** C'est le
   motif de HDRNet, des LUT adaptatives et de PCT-Net. Il est compatible avec un
   champ géographique continu, donc sans raccord, et s'applique par lecture de
   texture, donc en temps réel.
3. **Cohérence de pyramide comme contrainte dure.** La réduction de l'enfant
   doit redonner le parent, pour la couleur (déjà fait) comme pour le détail
   (CEM).
4. **Une tuile est une fonction pure d'un voisinage fixe.** Les méthodes
   génératives ne sont sans raccord que débruitées *conjointement* (MultiDiffusion,
   InfScene-SR). Un serveur rend (z, x, y) indépendamment : il faut un halo fixe
   et une graine par clé de tuile.
5. **La provenance par pixel.** Il faut distinguer *observé*, *restauré* et
   *inventé* (cf. UnCRtainTS pour l'incertitude). Un composite réel mais ancien
   vaut mieux qu'un remplissage halluciné, et vaut infiniment mieux qu'un trou.

---

## 3. État de l'art

Format : **Nom** — auteurs, lieu, année. Ce que ça fait, pourquoi c'est
pertinent ici. Chiffre clé. Code / licence. — lien.

### 3.1 Rééchantillonnage, filtrage, netteté classique

- **Cardinality-constrained texture filtering** — Manson & Schaefer, SIGGRAPH
  2013 *(titre non vérifié)*. Revue des préfiltres de mipmap (boîte, tente,
  gaussien, Lanczos, Mitchell–Netravali) et de l'EWA accéléré par mipmap de
  Heckbert (9 à 36 texels par échantillon). Le Lanczos-2 à 5× demande environ
  400 texels. — https://people.engr.tamu.edu/schaefer/research/cardinality.pdf
- **Generalized sampling in computer graphics** — Nehab & Hoppe,
  MSR-TR-2010-10. Taxonomie de Mitchell–Netravali des artefacts de
  rééchantillonnage. La théorie du filtrage de qualité est rarement appliquée
  au-delà du redimensionnement séparable : c'est le cas des orthos déformées,
  reprojetées puis réduites. —
  https://www.microsoft.com/en-us/research/wp-content/uploads/2010/02/MSR-TR-2010-10c.pdf
- **Guided image filtering** — He, Sun, Tang, ECCV 2010. Filtre linéaire local en
  O(N) qui préserve les contours : rehaussement de détail, sur-échantillonnage
  conjoint, dé-voilage. Trivial en shader. —
  https://mlanthology.org/eccv/2010/he2010eccv-guided/
- **Local Laplacian filters** — Paris, Hasinoff, Kautz, SIGGRAPH 2011. Tonalité
  et détail par pyramide laplacienne, **sans halo** même aux réglages extrêmes ;
  intégré à Lightroom. C'est un meilleur défaut que le masque flou, et il est
  naturellement multi-échelle. — https://people.csail.mit.edu/sparis/publi/2011/siggraph
- **Fast Local Laplacian Filters** — Aubry, Paris, Hasinoff, Kautz, Durand, ACM
  TOG 2014. Environ 50× plus rapide. —
  https://imagine.enpc.fr/~aubrym/projects/llf/index.html
- **NTIRE 2025 Efficient SR** — Ren et al., CVPRW 2025. Catalogue des réseaux
  sous le million de paramètres (au moins 26,90 dB) : le bon endroit pour
  chercher un restaurateur au rythme des tuiles. — https://arxiv.org/pdf/2504.10686
- **NTIRE 2025 Efficient burst HDR & restoration** — CVPRW 2025. Ordre de
  grandeur du « léger » en restauration : moins de 30 M de paramètres et moins
  de 4 TFLOPs. — https://arxiv.org/html/2505.12089v1

### 3.2 Super-résolution (détail)

Échelle utile, en Web Mercator à l'équateur (156 543 / 2^z m/px) :

| Niveau | Résolution | Équivalent |
|---|---|---|
| z14 | 9,6 m | ≈ Sentinel-2 |
| z16 | 2,4 m | ≈ cible SEN2NAIP |
| z17 | 1,2 m | ≈ SPOT |
| z18 | 0,6 m | ≈ NAIP |
| z19 | 0,3 m | |

De z16 à z19, c'est ×8.

**Théorie et régression (fidèle, peu d'invention).**

- **Perception–Distortion tradeoff** — Blau & Michaeli, CVPR 2018. Démontre
  qu'une distorsion moyenne plus faible rend la sortie plus distinguable du
  réel, quelle que soit la mesure de distorsion. Le curseur fidélité/réalisme
  est donc **une décision produit**. —
  https://openaccess.thecvf.com/content_cvpr_2018/html/Blau_The_Perception-Distortion_Tradeoff_CVPR_2018_paper.html
- **SRCNN** — Dong et al., ECCV 2014. Le point de départ ; reste un plancher bon
  marché. — https://mlanthology.org/eccv/2014/dong2014eccv-learning
- **EDSR / RCAN** — Lim et al., CVPRW 2017 ; Zhang et al., ECCV 2018.
  Régresseurs résiduels : n'inventent pas d'objet, mais flous au-delà de ×4. —
  https://openaccess.thecvf.com/content_cvpr_2017_workshops/w12/html/Lim_Enhanced_Deep_Residual_CVPR_2017_paper.html
  · https://github.com/yulunzhang/RCAN
- **SwinIR** — Liang et al., ICCVW 2021. Super-résolution, débruitage **et
  artefacts JPEG** par Swin transformer (Apache-2.0, non vérifié). —
  https://openaccess.thecvf.com/content/ICCV2021W/AIM/html/Liang_SwinIR_Image_Restoration_Using_Swin_Transformer_ICCVW_2021_paper.html
- **HAT** — Chen et al., CVPR 2023. Plus de 1 dB sur l'état de l'art d'alors.
  C'est une ossature PSNR solide pour un mode « peu d'hallucination ». —
  https://openaccess.thecvf.com/content/CVPR2023/html/Chen_Activating_More_Pixels_in_Image_Super-Resolution_Transformer_CVPR_2023_paper.html
- **DAT** — Chen et al., ICCV 2023. —
  https://openaccess.thecvf.com/content/ICCV2023/html/Chen_Dual_Aggregation_Transformer_for_Image_Super-Resolution_ICCV_2023_paper.html

**Dégradations réalistes.**

- **BSRGAN** — Zhang et al., ICCV 2021. Mélange aléatoire de flou, réduction,
  bruit, **JPEG à qualité variable** et bruit de chaîne caméra. C'est le gabarit
  pour entraîner sur des tuiles déjà compressées et rééchantillonnées. —
  https://arxiv.org/pdf/2103.14006
- **Real-ESRGAN** — Wang et al., ICCVW 2021. Dégradation d'ordre supérieur et
  filtres sinc. C'est l'agrandisseur déployable de fait, sous **BSD-3**. —
  https://arxiv.org/abs/2107.10833v2

**Échelle arbitraire (zoom continu).**

- **LIIF** — Chen, Liu, Wang, CVPR 2021. Fonction implicite coordonnées →
  RVB, extrapole jusqu'à ×30 : adaptée au zoom *continu* entre niveaux
  entiers. —
  https://openaccess.thecvf.com/content/CVPR2021/papers/Chen_Learning_Continuous_Image_Representation_With_Local_Implicit_Image_Function_CVPR_2021_paper.pdf
- **LTE** — Lee & Jin, CVPR 2022. Fréquences locales dominantes pour les hautes
  fréquences. —
  https://openaccess.thecvf.com/content/CVPR2022/html/Lee_Local_Texture_Estimator_for_Implicit_Representation_Function_CVPR_2022_paper.html
- **CiaoSR** — Cao et al., CVPR 2023. État de l'art en échelle arbitraire, mais
  coûteux. —
  https://openaccess.thecvf.com/content/CVPR2023/html/Cao_CiaoSR_Continuous_Implicit_Attention-in-Attention_Network_for_Arbitrary-Scale_Image_Super-Resolution_CVPR_2023_paper.html

**Diffusion et génératif.**

- **SR3** — Saharia et al., TPAMI 2022. Première super-résolution par diffusion :
  le réalisme se paie en étapes et en stochasticité. — https://arxiv.org/pdf/2104.07636
- **StableSR** — Wang et al., IJCV 2024. Stable Diffusion gelé, scalaire de
  fidélité ; licence S-Lab non commerciale *(non vérifié)*. —
  https://arxiv.org/pdf/2305.07015
- **DiffBIR** — Lin et al., ECCV 2024. Deux étages : dégradation retirée
  (fidèle), puis régénération (générative). C'est le **gabarit d'architecture**
  pour séparer la tuile « propre » de la tuile « détail ». —
  https://ecva.net/papers/eccv_2024/papers_ECCV/html/7690_ECCV_2024_paper.php
- **SeeSR** — Wu et al., CVPR 2024. Super-résolution sémantique guidée par des
  étiquettes. Une mauvaise étiquette sur une tuile aérienne (« bâtiments » sur un
  champ) oriente l'hallucination. —
  https://polyu.edu.hk/publications/innovationdigest/issue/202411/highlight/advancing-real-world-super-resolution-images-using-a-semantic-aware-restoration-approach
- **ResShift → SinSR (et RSD)** — Yue et al., NeurIPS 2023 → Wang et al., CVPR
  2024 ; RSD 2025. Diffusion partant de l'image basse résolution, distillée en
  **une étape**. — https://arxiv.org/pdf/2311.14760 · https://arxiv.org/pdf/2503.13358
- **OSEDiff** — Wu, Sun, Ma, Zhang, NeurIPS 2024. Une étape sur SD-2.1, part du
  latent basse qualité **sans bruit aléatoire** : c'est **déterministe**, donc
  cacheable et reproductible. Code **Apache-2.0**, base SD-2.1 sous OpenRAIL
  *(non vérifié)*. — https://github.com/cswry/osediff
- **PiSA-SR** — Sun et al., CVPR 2025. Deux LoRA (fidélité pixel, détail
  sémantique), avec deux échelles de guidage réglables à l'inférence : un curseur
  « fidélité ↔ détail » **par niveau de zoom**. **Apache-2.0**. —
  https://openaccess.thecvf.com/content/CVPR2025/html/Sun_Pixel-level_and_Semantic-level_Adjustable_Super-resolution_A_Dual-LoRA_Approach_CVPR_2025_paper.html
- **SUPIR** — Yu et al., CVPR 2024. Échelle SDXL, qualité perceptuelle très
  élevée, environ 60 Go de RAM. **Non commercial.** —
  https://openaccess.thecvf.com/content/CVPR2024/html/Yu_Scaling_Up_to_Excellence_Practicing_Model_Scaling_for_Photo-Realistic_Image_CVPR_2024_paper.html
- **FluxSR** — Li et al., ICML 2025. Une étape distillée de FLUX.1-dev ; la base
  est non commerciale *(non vérifié)*. —
  https://proceedings.mlr.press/v267/li25a.html
- **ODTSR** — arXiv 2025. Diffusion transformer en une étape, contrôlable. —
  https://arxiv.org/pdf/2511.17138v2
- **StrSR** — arXiv 2026. La distillation DiT en une étape produit des
  **artefacts périodiques en grille**, très visibles sur une carte en tuiles ;
  correction spectrale. — https://papers.cool/arxiv/2603.06275

**Génération par tuiles sans raccord.**

- **MultiDiffusion** — Bar-Tal et al., ICML 2023. Fusion de chemins de débruitage
  qui se recouvrent, sans entraînement. C'est la base du tuilage sans raccord. —
  https://proceedings.mlr.press/v202/bar-tal23a.html
- **DemoFusion** — Du et al., CVPR 2024. Agrandissement progressif guidé par le
  basse résolution : l'analogue le plus proche de « z+1 conditionné par z ». —
  https://openaccess.thecvf.com/content/CVPR2024/html/Du_DemoFusion_Democratising_High-Resolution_Image_Generation_With_No__CVPR_2024_paper.html
- **Tiled Diffusion** — arXiv 2412.15185. Contraintes de tuilage dans le latent ;
  artefacts résiduels dus à la frontière latent/VAE, le même problème qu'aurait
  un serveur de tuiles. — https://alphaxiv.org/overview/2412.15185v4
- **PatchVSR** — arXiv 2025. Le simple moyennage de patchs génératifs laisse des
  raccords ; il faut une pondération centrée. — https://arxiv.org/pdf/2509.26025
- **Prompts par tuile** — arXiv 2026. Une légende globale égare la
  super-résolution tuilée ; un prompt par tuile corrige. —
  https://arxiv.org/abs/2602.03342v1

**Super-résolution en télédétection.**

- **Revue RS SR** — arXiv 2025. Point d'entrée (CNN, transformer, GAN,
  diffusion). — https://arxiv.org/pdf/2505.23248
- **TransENet / HSENet** — Lei, Shi, Mo, IEEE TGRS 2022. Références aériennes
  (AID, RSSCN7, WHU-RS19). —
  https://levir.buaa.edu.cn/static/pdfs/2022_sen_lei_transformer-based.pdf
- **EDiffSR** — Xiao, Yuan et al., 2023. Diffusion légère pour la télédétection. —
  https://arxiv.org/pdf/2310.19288
- **LCMSR** — arXiv 2025. Modèle de cohérence latente en **une étape**, pour un
  service par tuile à la demande. — https://arxiv.org/pdf/2503.19505
- **InfScene-SR** — Univ. Idaho, arXiv 2026. Débruitage conjoint de patchs, avec
  une fusion à variance corrigée : **super-résolution de taille arbitraire sans
  raccord**, sans réentraînement. C'est le travail le plus pertinent sur les
  raccords (multi-étapes, base SR3). — https://arxiv.org/html/2602.19736v1
- **HighRes-net** — Deudon et al., 2020. Super-résolution multi-images
  (PROBA-V). — https://arxiv.org/abs/2002.06460v1
- **DeepSUM** — Molini et al., IEEE TGRS 2020. Multi-temporel non recalé ;
  vainqueur PROBA-V. Une archive d'imagerie multi-dates peut l'exploiter. —
  https://arxiv.org/pdf/1907.06490
- **WorldStrat** — Cornebise et al., NeurIPS D&B 2022. Environ 10 000 km² de SPOT
  1,5 m appariés à Sentinel-2. Imagerie haute résolution **CC-BY-NC**. —
  https://proceedings.neurips.cc/paper_files/paper/2022/hash/a6fe99561d9eb9c90b322afe664587fd-Abstract.html
- **SEN2NAIP** — Aybar et al., Scientific Data 2024. Paires Sentinel-2/NAIP
  (×4, environ z14 → z16). — https://doaj.org/article/bc28e820c10b4c1d86a63ae4846ed128
- **S2-NAIP, « Zooming Out on Zooming In »** — Wolters, Bastani, Kembhavi (AI2),
  2023. 1,2 million de paires. **CLIPScore** suit le mieux le jugement humain ;
  les GAN sont **plus fidèles sémantiquement que la diffusion**. —
  https://arxiv.org/pdf/2311.18082
- **DiffFuSR** — arXiv 2025. Diffusion sur toutes les bandes Sentinel-2. —
  https://arxiv.org/pdf/2506.11764
- **SR inter-capteurs par flow matching** — arXiv 2025. —
  https://arxiv.org/pdf/2510.23816
- **RRSGAN** — Dong et al., IEEE TGRS 2021. Super-résolution avec **image de
  référence** haute résolution : « une autre date sert de référence ». —
  https://ieeexplore.ieee.org/document/9328132
- **RefSR avec a priori de changement** — Dong et al., CVPR 2024. Diffusion qui
  modélise le changement d'occupation du sol entre la date de référence et la
  date cible. Les écarts ×8 à ×16 (z16 → z19) sont les problèmes ouverts. —
  https://arxiv.org/html/2403.17460v1
- **DiffusionSat** — Khanna et al., ICLR 2024. Modèle fondation conditionné par
  des **métadonnées** (géolocalisation, date) : super-résolution, génération
  temporelle, inpainting. — https://iclr.cc/virtual/2024/poster/18972

### 3.3 Cohérence entre niveaux de zoom

- **Explorable SR / CEM** — Bahat & Michaeli, CVPR 2020. Un module analytique
  autour de n'importe quel réseau de super-résolution projette la sortie pour que
  **sa réduction redonne exactement l'entrée**, sans jamais augmenter l'erreur.
  C'est la contrainte « z+1 généré → z ». Il faut le vrai noyau de réduction, et
  Tuile connaît le sien. — https://arxiv.org/pdf/1912.01839
- **SelfDZSR** — Zhang et al., 2022/2024. Une capture téléobjectif supervise la
  courte focale. C'est l'analogue publié de « s'entraîner sur les paires (z, z+1)
  de la pyramide là où z+1 existe ». — https://arxiv.org/pdf/2203.01325
- **ZoomLDM** — Yellapragada et al., CVPR 2025. Diffusion latente conditionnée par
  le grossissement ; sortie cohérente en 4096² ; le supplément inclut NAIP à 4
  résolutions. —
  https://openaccess.thecvf.com/content/CVPR2025/html/Yellapragada_ZoomLDM_Latent_Diffusion_Model_for_Multi-scale_Image_Generation_CVPR_2025_paper.html
- **Genesis (et EarthGen)** — Xing, Wei, Sastry, Jacobs, Khanal, ACM SIGSPATIAL
  2026. La synthèse satellite posée comme **complétion de quadtree de tuiles** :
  un opérateur vertical fait un parent 256² → mosaïque 512² de ses 4 enfants ;
  un opérateur horizontal complète les quadrants masqués. Cohérence entre zooms
  et peu de raccords. Code : github.com/mvrl/genesis. **Le plus proche du cadre
  « tuiles de carte ».** — https://arxiv.org/pdf/2609.02683 ·
  https://arxiv.org/pdf/2409.01491
- **Generative Powers of Ten** — Wang et al., CVPR 2024. Échantillonnage conjoint
  qui garde tous les niveaux cohérents. Au-delà d'un grand écart d'échelle,
  c'est de la *génération*, pas de la restauration. —
  https://openaccess.thecvf.com/content/CVPR2024/html/Wang_Generative_Powers_of_Ten_CVPR_2024_paper.html
- **Chain-of-Zoom** — Kim, Kim, Ye, NeurIPS 2025. Un ×4 réutilisé en chaîne,
  jusqu'à au-delà de ×256 : l'hallucination **se cumule** à chaque étape. —
  https://proceedings.neurips.cc/paper_files/paper/2025/hash/b66d8cbb01ac8212830068f3d75b4c5c-Abstract-Conference.html
- **MetaEarth** — arXiv 2024. Diffusion auto-cascadée guidée par la résolution,
  pour une imagerie géoconditionnée de taille illimitée. —
  https://arxiv.org/pdf/2405.13570

### 3.4 Mesurer l'hallucination

- **opensr-test** — Aybar et al. (ESA OpenSR), 2024. Cohérence, synthèse,
  correction, avec des **pourcentages d'hallucination, d'omission et
  d'amélioration**. — https://github.com/ESAOpenSR/opensr-test
- **SUPERIX** — Univ. de Valence, 2024–25. Intercomparaison communautaire ; la
  super-résolution peut fausser une détection de routes en aval. —
  https://huggingface.co/isp-uv-es/superIX
- **Beyond Pretty Pictures** — arXiv 2025. Ni le PSNR, ni le SSIM, ni le score
  d'amélioration ne prédisent l'utilité en aval ; **seul le LPIPS** le fait. Le
  multi-images (HighRes-net) hallucine le moins. — https://arxiv.org/pdf/2505.24799
- **Hallucination Score** — arXiv 2025. Juge par LLM multimodal : l'hallucination
  est orthogonale aux métriques de fidélité et de qualité. —
  https://arxiv.org/pdf/2507.14367
- **HalluGen** — arXiv 2025. La télédétection y figure parmi les domaines
  critiques. — https://arxiv.org/pdf/2512.03345v1

### 3.5 Défloutage et compensation de FTM

- **Camera shake** — Fergus et al., SIGGRAPH 2006. Fondateur de l'estimation
  aveugle du noyau. — https://cs.nyu.edu/~fergus/research/deblur.html
- **Understanding blind deconvolution** — Levin, Weiss, Durand, Freeman, CVPR
  2009. Le MAP joint naïf préfère « pas de flou », et le flou est rarement
  invariant spatialement. Il faut estimer **par région, pas par tuile**. —
  https://people.csail.mit.edu/billf/papers/deconvLevinEtalCVPR09.pdf
- **Dark channel prior deblurring** — Pan et al., CVPR 2016. Les ombres au sol
  fournissent les pixels sombres. —
  https://openaccess.thecvf.com/content_cvpr_2016/html/Pan_Blind_Image_Deblurring_CVPR_2016_paper.html
- **NAFNet** — Chen et al., ECCV 2022. Sans activation non linéaire ; 33,69 dB
  sur GoPro à 8,4 % du calcul de l'état de l'art d'alors. **Meilleure ossature
  légère** (MIT, non vérifié). — https://arxiv.org/abs/2204.04676v2
- **Restormer** — Zamir et al., CVPR 2022. Attention par canal, coût linéaire en
  résolution. —
  https://openaccess.thecvf.com/content/CVPR2022/html/Zamir_Restormer_Efficient_Transformer_for_High-Resolution_Image_Restoration_CVPR_2022_paper.html
- **RealBlur (classements)** — l'écart entre réseaux et méthodes classiques reste
  faible sur du flou réel (≈ 29 dB contre 27). —
  https://vercel.hyper.ai/cn/sota/tasks/deblurring/benchmark/deblurring-on-realblur-j-trained-on-gopro
- **Compensation de FTM KOMPSAT-2** — SPIE 8399, 2012. Richardson-Lucy et
  a priori épars ; les limites sont l'oscillation (ringing) et le bruit. —
  https://nanolithography.spiedigitallibrary.org/conference-proceedings-of-spie/8399/83990R/MTF-compensation-algorithm-based-on-blind-deconvolution-for-high-resolution/10.1117/12.920877.full
- **Compensation de FTM régularisée** — J. Appl. Remote Sens. 14(3), 2020.
  Inverser la FTM amplifie bruit et repliement ; régularisation par a prioris non
  locaux. —
  https://www.spiedigitallibrary.org/journals/journal-of-applied-remote-sensing/volume-14/issue-3/036506/Regularization-based-modulation-transfer-function-compensation-for-optical-satellite-image/10.1117/1.JRS.14.036506.full
- **Restauration Pléiades (CNES)** — ISPRS 2012. Déconvolution puis débruitage
  par paquets d'ondelettes : le précédent industriel « netteté + débruitage ». —
  https://isprs-archives.copernicus.org/articles/XXXIX-B1/555/2012/isprsarchives-XXXIX-B1-555-2012.pdf
- **SDD-GAN** — Sensors. Défloutage de mouvement orbital. —
  https://www.ncbi.nlm.nih.gov/pmc/articles/PMC9611111/
- **Défloutage drone par transformer** — Li & Zhao, IGARSS 2024. —
  https://wrap.warwick.ac.uk/id/eprint/187070
- **AKMD-Net** — arXiv 2024. Défloutage et super-résolution conjoints
  (mouvement, turbulence, bicubique), proche du mélange d'une pyramide. —
  https://arxiv.org/pdf/2412.05696
- **HDI-PRNet** — Feng et al., IEEE TGRS 2025. Dégradation « d'ordre élevé »
  (flou et bruit composés) : le modèle le plus proche de tuiles rééchantillonnées
  et ré-encodées plusieurs fois. — https://arxiv.org/pdf/2412.07195v2

### 3.6 Bruit et pansharpening

- **DnCNN** — Zhang et al., IEEE TIP 2017. Débruitage aveugle ; le même réseau fait
  la super-résolution et le déblocage JPEG. — https://arxiv.org/pdf/1608.03981
- **FFDNet** — Zhang, Zuo, Zhang, IEEE TIP 2018. Carte de niveau de bruit réglable
  et non uniforme : utile pour une mosaïque de sources au bruit différent. —
  https://arxiv.org/pdf/1710.04026
- **PanNet** — Yang et al., ICCV 2017. Pansharpening par apprentissage, moins de
  halos colorés que Brovey ou IHS. —
  https://astro.paperswithcode.com/paper/pannet-a-deep-network-architecture-for-pan
- **Analyse du pansharpening + PSGAN** — arXiv 2024. La distorsion spectrale
  reste non résolue. — https://arxiv.org/abs/2412.04896v1
- **Revue pansharpening** — J. Remote Sensing 2022 (Q4, SAM, ERGAS, QNR). —
  https://www.ygxb.ac.cn/en/article/doi/10.11834/jrs.20211325

### 3.7 Artefacts de compression (tuiles JPEG)

- **AR-CNN** — Dong et al., ICCV 2015. Blocs, oscillations, flou ; la
  super-résolution naïve amplifie le bruit de quantification. —
  https://openaccess.thecvf.com/content_iccv_2015/html/Dong_Compression_Artifacts_Reduction_ICCV_2015_paper.html
- **QGAC** — Ehrlich et al., ECCV 2020. Conditionné par **la matrice de
  quantification du fichier** : un modèle pour toutes les qualités. Les tuiles
  JPEG la portent, ce signal est gratuit. —
  https://www.ecva.net/papers/eccv_2020/papers_ECCV/html/570_ECCV_2020_paper.php
- **FBCNN** — Jiang, Zhang, Timofte, ICCV 2021. Prédit la qualité, réglable, et
  modélise le **double JPEG non aligné**, la perte générationnelle des fonds de
  carte ré-encodés. —
  https://openaccess.thecvf.com/content/ICCV2021/html/Jiang_Towards_Flexible_Blind_JPEG_Artifacts_Removal_ICCV_2021_paper.html
- **JPEG-DDRM** — Kawar et al., NeurIPS-W 2022. Diffusion inconditionnelle
  pré-entraînée, sans réentraînement. — https://arxiv.org/abs/2209.11888v2
- **Décodage explorable** — Bahat & Michaeli. Plusieurs décodages tous
  compatibles avec le fichier : cela pose la question fidélité/hallucination. —
  https://arxiv.org/pdf/2006.09332
- **Saignement de chroma (4:2:0)** — IEEE SPL 2019. Restauration de la chroma
  guidée par la luma : les franges de couleur sur toits et routes. —
  https://unpaywall.org/10.1109%2FLSP.2018.2880146
- **Décodeur JPEG à chroma guidée** — GCCE 2021. Variante classique, dans le
  décodeur, bon marché. — https://www.cs.tsukuba.ac.jp/~taizo/pdf/conference/202110_GCCE.pdf
- **Blocs aux bords de tuiles** : aucune littérature trouvée.

### 3.8 Restauration tout-en-un

- **PromptIR** — Potlapalli et al., NeurIPS 2023. —
  https://arxiv.org/pdf/2306.13090
- **InstructIR** — Conde et al., ECCV 2024. Instructions textuelles, ossature
  NAFNet. — https://www.ecva.net/papers/eccv_2024/papers_ECCV/papers/05238.pdf
- **DA-CLIP** — Luo et al., ICLR 2024. — https://www.iclr.cc/virtual/2024/poster/17626
- **AutoDIR** — Jiang et al. Détecte la dégradation dominante, puis la traite. —
  https://cs.paperswithcode.com/paper/autodir-automatic-all-in-one-image
- **MoCE-IR** — Zamfir et al., CVPR 2025. Experts de complexité variable : une tuile
  propre prend le chemin bon marché. —
  https://openaccess.thecvf.com/content/CVPR2025/html/Zamfir_Complexity_Experts_are_Task-Discriminative_Learners_for_Any_Image_Restoration_CVPR_2025_paper.html
- **AdaIR** — Cui et al. Par bandes de fréquence (FTM, bruit, JPEG). —
  https://irep.mbzuai.ac.ae/items/82e95aef-ae68-4bb7-9160-7109c8eebe32
- **PhyDAE** — Dong et al., arXiv 2025. Tout-en-un pour la télédétection, avec la
  physique atmosphère/capteur ; code non publié. — https://sotaverified.org/papers/251008653
- **Task-Guided Prompting** — arXiv 2026. Bruit, nuage, ombre, flou, chatoiement ;
  optique, SAR, thermique. — https://arxiv.org/html/2604.02742v1
- **Aether** — Dong et al., 2026. — https://www.ejournal.org.cn/en/article/doi/10.12263/DZXB.20251152/
- **ETDiffIR** — Sensors 2024. — https://pmc.ncbi.nlm.nih.gov/articles/PMC11207699
- **MDTD-ArtIR** — arXiv 2026. Éditeurs génériques contre restaurateurs
  dédiés. — https://arxiv.org/pdf/2608.00736

### 3.9 Balance des blancs et constance de couleur

- **Gray-Edge** — van de Weijer, Gevers, Gijsenij, IEEE TIP 2007. Moins biaisé que
  le gris moyen par les grandes surfaces uniformes (champs, eau). Une réduction
  GPU suffit. — https://thoth.inrialpes.fr/people/vandeweijer/papers/ColorConstancyIP.pdf
- **Pondération photométrique des contours** — Gijsenij, Gevers, van de Weijer,
  TPAMI. Les contours d'ombre dominent en aérien. —
  https://staff.fnwi.uva.nl/th.gevers/pub/GeversPAMI11-2.pdf
- **FFCC** — Barron & Tsai, CVPR 2017. Une distribution a posteriori complète :
  lissage spatial et entre zooms possible ; environ 700 images/s sur mobile. —
  https://openaccess.thecvf.com/content_cvpr_2017/html/Barron_Fast_Fourier_Color_CVPR_2017_paper.html
- **Deep White-Balance Editing** — Afifi & Brown, CVPR 2020. Corrige la balance
  des blancs d'images sRGB **déjà rendues** : le seul recours pour des orthos
  JPEG (non testé sur de l'aérien). —
  https://openaccess.thecvf.com/content_CVPR_2020/html/Afifi_Deep_White-Balance_Editing_CVPR_2020_paper.html
- **Mixed-illuminant AWB** — Afifi et al., WACV 2022. Mélange par pixel :
  balance des blancs variable dans l'espace (soleil contre voile). —
  https://openaccess.thecvf.com/content/WACV2022/html/Afifi_Auto_White-Balance_Correction_for_Mixed-Illuminant_Scenes_WACV_2022_paper.html
- **C5** — Afifi et al., ICCV 2021. S'adapte à une caméra inconnue. —
  https://www.academia.edu/71883379/Cross_Camera_Convolutional_Color_Constancy
- **CCMNet** — Kim et al., ICCV 2025. —
  https://openaccess.thecvf.com/content/ICCV2025/papers/Kim_CCMNet_Leveraging_Calibrated_Color_Correction_Matrices_for_Cross-Camera_Color_Constancy_ICCV_2025_paper.pdf
- **GCC** — Chang et al., CVPR 2025. Une diffusion peint une charte couleur dans
  la scène. —
  https://openaccess.thecvf.com/content/CVPR2025/html/Chang_GCC_Generative_Color_Constancy_via_Diffusing_a_Color_Checker_CVPR_2025_paper.html
- **VLM-CC** — CVPR 2026. Boucle de jugement par modèle vision-langage. —
  https://cvpr.thecvf.com/virtual/2026/poster/37336

### 3.10 Exposition, tonalité, faible lumière — et le motif « champ »

- **Multi-scale exposure correction** — Afifi et al., CVPR 2021. Sur- et
  sous-exposition (neige, sable, toits). —
  https://openaccess.thecvf.com/content/CVPR2021/html/Afifi_Learning_Multi-Scale_Photo_Exposure_Correction_CVPR_2021_paper.html
- **HDRNet (grille bilatérale)** — Gharbi et al., SIGGRAPH 2017. Coefficients
  affines de couleur prédits sur une **grille bilatérale basse résolution**, puis
  découpés à pleine résolution. **Temps réel**, une lecture de texture. Présenté
  ici via un article de 2025 qui le compare aux LUT 3D. —
  https://arxiv.org/pdf/2507.12135v1.pdf
- **LUT 3D adaptatives** — Zeng et al., TPAMI 2020. Un petit CNN pondère des LUT
  de base ; moins de 600 000 paramètres, **moins de 2 ms en 4K**. Une texture 3D
  33³ par région ou par niveau. — https://www.arxiv.org/pdf/2009.14468
- **AdaInt** — Yang et al., CVPR 2022. Intervalles de LUT non uniformes. Les LUT
  ne font que de la couleur, pas de tonalité locale. —
  https://openaccess.thecvf.com/content/CVPR2022/papers/Yang_AdaInt_Learning_Adaptive_Intervals_for_3D_Lookup_Tables_on_Real-Time_CVPR_2022_paper.pdf
- **Zero-DCE / Zero-DCE++** — Guo, Li et al., CVPR 2020 / TPAMI 2021. Courbes par
  pixel apprises **sans vérité terrain** ; environ 10 000 paramètres. —
  https://arxiv.org/pdf/2001.06826
- **Retinexformer** — Cai et al., ICCV 2023. Vols d'hiver à soleil bas, zones
  sombres. — https://arxiv.org/pdf/2303.06705
- **Local Laplacian** : voir §3.1.

**Le motif qui intéresse Tuile.** HDRNet, les LUT adaptatives et PCT-Net (§3.11)
**prédisent des paramètres en basse résolution et les appliquent en pleine
résolution**. Ils supposent *une* image. Pour des tuiles, leurs paramètres
doivent devenir un **champ continu en coordonnées géographiques**, interpolé à
travers tuiles et niveaux, pour que deux voisines ne puissent pas sauter. C'est
exactement la forme de `GainField` dans `tuile-radiometry` (sommets partagés sur
une grille géographique, interpolation bilinéaire). Aucune publication ne fait
ce « champ de LUT ou grille bilatérale géoancrée » (§6) : c'est une extension
naturelle et originale.

### 3.11 Transfert de couleur et harmonisation (au-delà du doc 18)

- **Transfert de PDF à N dimensions (IDT)** — Pitié, Kokaram, Dahyot, ICCV 2005.
  Ajusté une fois, puis cuit dans une LUT 3D. —
  https://mlanthology.org/iccv/2005/pitie2005iccv-n/
- **Barycentres de Wasserstein tranchés** — Bonneel, Rabin, Peyré, Pfister, JMIV
  2015. Le barycentre de N palettes source est un « look commun » fondé en
  principe pour une mosaïque multi-sources. —
  https://perso.liris.cnrs.fr/nbonneel/WassersteinSliced-JMIV.pdf
- **Transfert par patchs + SIFT Flow** — Alghamdi & Dahyot, 2020. —
  https://ar5iv.arxiv.org/html/2005.09015
- **WCT²** — Yoo et al., ICCV 2019. Style photoréaliste qui préserve la
  structure. —
  https://openaccess.thecvf.com/content_ICCV_2019/html/Yoo_Photorealistic_Style_Transfer_via_Wavelet_Transforms_ICCV_2019_paper.html
- **Revue transfert de style** — 2022 (Luan 2017, PhotoWCT). —
  https://arxiv.org/pdf/2203.06328
- **Revue composition et harmonisation** — Niu et al. (DoveNet, RainNet, CDTNet,
  PCT-Net). — https://arxiv.org/pdf/2106.14490
- **PCT-Net** — Guerreiro et al., CVPR 2023. Transformations affines par pixel
  prédites en basse résolution, appliquées en pleine résolution. —
  https://openaccess.thecvf.com/content/CVPR2023/html/Guerreiro_PCT-Net_Full_Resolution_Image_Harmonization_Using_Pixel-Wise_Color_Transformations_CVPR_2023_paper.html
- **libcom** — boîte à outils d'harmonisation ouverte. —
  https://libcom.readthedocs.io/en/latest/image_harmonization.html
- **Harmonisation par diffusion** — 2023 ; **Region-to-Region**, 2025. —
  https://ar5iv.labs.arxiv.org/html/2306.10441 · https://arxiv.org/pdf/2508.09746
- **Cohérence de couleur par CNN sensible à la texture** — Qian & Su, Remote
  Sensing 2024. Alternative apprise au Wallis. —
  https://mdpi-res.com/d_attachment/remotesensing/remotesensing-16-03269/article_deploy/remotesensing-16-03269.pdf
- **Cohérence de couleur par NeRF** — Zuo et al., IEEE JSTARS 2024. Satellite +
  drone. — https://arxiv.org/abs/2411.05557v1
- **Normalisation radiométrique par GAN (mosaïques lunaires)** — arXiv 2026.
  Normalise des tuiles vers une **mosaïque de référence basse résolution** et bat
  les méthodes par histogramme. — https://arxiv.org/pdf/2604.25208

### 3.12 Cohérence entre sources, dates et niveaux

- **HLS v2** — NASA. Landsat 8/9 et Sentinel-2 traités comme **une seule
  constellation virtuelle** : correction LaSRC commune, Fmask, BRDF normalisée au
  nadir (NBAR, facteur c de Roy), ajustement des bandes passantes. C'est **la
  recette de référence** pour qu'une mosaïque multi-sources par niveau ait l'air
  d'un seul capteur. — https://lpdaac.usgs.gov/documents/1698/HLS_User_Guide_V2.pdf
  · https://hls.gsfc.nasa.gov/algorithm/
- **sen2nbar** — FOSS4G-Europe 2024. NBAR en Python pour Sentinel-2. —
  https://arxiv.org/html/2404.15812v2
- **Harmonisation L8/S2 reproductible** — Brazil Data Cube / INPE, Frontiers
  2023. — https://www.frontiersin.org/journals/remote-sensing/articles/10.3389/frsen.2023.1254242/full
- **Sentinel-2 L2A comme référence de couleur** — Remote Sensing 13:3094, 2021.
  Mosaïques WorldView-2 ajustées à la réflectance S2, avec RANSAC. **La forme
  publiée de l'idée du doc 18.** —
  https://mdpi-res.com/d_attachment/remotesensing/remotesensing-13-03094/article_deploy/remotesensing-13-03094.pdf
- **Référence externe basse résolution + dé-voilage** — ISPRS Annals 2020 (à
  distinguer du « RRN multi-références » du doc 18). —
  https://isprs-annals.copernicus.org/articles/V-3-2020/663/2020/isprs-annals-V-3-2020-663-2020.pdf
- **Mosaïquage par GAN sur référence MODIS** — Ma et al., IEEE GRSL 2022. —
  https://modis.gsfc.nasa.gov/sci_team/pubs/abstract_new.php?id=53864
- **Brevet d'édition d'orthomosaïque sur base Landsat** — US 9600740. Un
  ajustement global par bande donne un mauvais résultat ; il faut masquer
  nuages, neige, eau et végétation avant d'apparier. C'est la pratique
  industrielle. — https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/9600740
- **Choix du capteur de référence en couleur naturelle** — blog imagico.de, non
  évalué par des pairs. Landsat convient mieux que MODIS. —
  https://imagico.de/blog/?p=7487
- **Ajustement radiométrique en bloc (RadBA)** — Honkavaara & Khoramshahi, Remote
  Sensing 2018 ; Markelin et al., ISPRS 2024 (BRDF à 4 paramètres, trois dates de
  vol). La pratique la plus proche de celle des agences de cartographie. —
  https://tuhat.helsinki.fi/portal/files/98409823/remotesensing_10_00256.pdf ·
  https://isprs-archives.copernicus.org/articles/XLVIII-2-2024/219/2024/
- **Digital Radiometric Model** — ISPRS 2013. Réflectance et BRDF par point. —
  https://isprs-archives.copernicus.org/articles/XL-1-W1/293/2013/isprsarchives-XL-1-W1-293-2013.pdf
- **Point chaud et vignettage contre Wallis** — ISPRS 2022. Le Wallis **perd du
  contraste à petite échelle**, ce qui se voit en zoom fort. —
  https://isprs-archives.copernicus.org/articles/XLIII-B3-2022/1217/2022/
- **Intercomparaison ACIX** — Doxani et al., Remote Sensing 2018. Les processeurs
  de correction atmosphérique **ne s'accordent pas**, donc mélanger leurs
  produits crée des raccords. — https://www2.mdpi.com/2072-4292/10/2/352

### 3.13 Science de la couleur et métriques couleur

- **CAM16 / CAT16 / CAM16-UCS** — Li, Luo et al., 2017. Un ΔE euclidien où 1
  correspond environ au seuil de perception : un candidat pour mesurer raccords
  et écarts entre niveaux. —
  https://colour.readthedocs.io/en/v0.4.0/_modules/colour/appearance/cam16.html
- **ACES 2.0, compression de gamut inversible** — N. Shaw. Un gabarit d'étape
  « scène → affichage » pour le look. —
  https://community.acescentral.com/uploads/short-url/xpwqpILdBN4mI0RRxMg1VPjZk76.pdf
- **Pièges d'implémentation d'ACES 2.0** — discuss.pixls.us. —
  https://discuss.pixls.us/t/integrating-the-aces-2-0-module-into-my-experimental-branch-libre-dt-lab-of-darktable/59080
- **Indices radiométriques en tampon de ligne de raccord** — Zhang & Wang, 2018.
  Un gabarit de métrique automatique de raccord de tuiles. —
  https://www.jas.shu.edu.cn/EN/abstract/article/0255-8297/2313
- **Uniformité de couleur de mosaïques satellite** — ISPRS 2018. —
  https://isprs-archives.copernicus.org/articles/XLII-3/1099/2018/
- **Erreur absolue moyenne de recouvrement après ajustement gain/offset** — arXiv
  2026. L'erreur de raccord est mesurée une fois le gain global retiré :
  transposable aux tuiles. — https://arxiv.org/pdf/2602.02758
- **Métriques de fidélité spectrale (ERGAS, SAM, Q4)** — 2023. —
  https://arxiv.org/pdf/2310.08705

### 3.14 Voile et brume

- **Dark Channel Prior** — He, Sun, Tang, CVPR 2009. La référence de base. En
  vue verticale, pas de ciel pour estimer la lumière atmosphérique, et l'a priori
  casse sur les toits clairs, l'eau et la neige. —
  https://people.csail.mit.edu/kaiming/cvpr09/index.html
- **Vision and the Atmosphere** — Narasimhan & Nayar, IJCV 2002. Le modèle de
  diffusion. En vue verticale, la profondeur est presque constante, donc la
  transmission encode *l'épaisseur* du voile. —
  https://www.cs.cmu.edu/~ILIM/publications/PDFs/NN-IJCV02.pdf
- **HOT (Haze Optimized Transform)** — Zhang, Guindon, Cihlar, 2002. Les pixels
  clairs s'alignent sur une droite bleu/rouge, et l'écart à cette droite cartographie
  le voile. Correction relative, suffisante pour des tuiles visuelles. —
  https://www.isprs.org/proceedings/XXXIV/Part4/pdfpapers/252.pdf
- **AutoHOT et variantes** — Remote Sensing 2016–2017. Les modes d'échec
  pratiques : effet patchwork, halos. — https://api.crossref.org/works/10.3390%2FRS9100972
- **Haze Thickness Map** — Makarau et al., IEEE TGRS 2014. Utilisable sur du RVB
  très haute résolution non calibré. — https://elib.dlr.de/85644
- **DehazeFormer + RS-Haze** — Song et al., 2022. Bat FFA-Net avec 25 % des
  paramètres ; RS-Haze non homogène. — https://arxiv.org/abs/2204.03883v1
- **RICE** — Lin et al., 2019. RICE-1 : 500 paires tirées de Google Earth, le
  meilleur substitut aux tuiles web (licence de la source à considérer). —
  https://arxiv.org/pdf/1901.00600
- **Revue systématique du dé-voilage en télédétection** — arXiv 2026. Plus de 30
  méthodes, 5 jeux, 12 métriques. DehazeFormer fait passer la segmentation de
  14,9 à 30,4 mIoU, mais SpA-GAN, plus joli, la **dégrade**. C'est le meilleur
  point d'entrée. — https://arxiv.org/abs/2603.20289
- **RRSHID** — arXiv 2025. Premier grand banc *réel* apparié (3 053 paires). —
  https://arxiv.org/abs/2503.17966v2
- **DehazeMamba + MRSHaze** — arXiv 2025. Guidé par SAR. —
  https://arxiv.org/pdf/2503.13073
- **RSHaze5K** — IEEE DataPort. —
  https://ieee-dataport.org/keywords/remote-sensing-dehazing-image-restoration-rshaze5k-dataset
- **O-Transformer-Mamba** — Remote Sensing 2026. —
  https://mdpi-res.com/d_attachment/remotesensing/remotesensing-18-00191/article_deploy/remotesensing-18-00191.pdf
- **RSDehamba** — arXiv 2024. Coût linéaire. — https://arxiv.org/html/2405.10030v1
- **Diff-Dehazer** — Lan et al., AAAI 2025. Non apparié : la situation d'une
  archive. — https://arxiv.org/pdf/2503.15017
- **HazeGen / DiffDehaze** — Wang et al., CVPR 2025. —
  https://mlanthology.org/cvpr/2025/wang2025cvpr-learning
- **IDDM** — arXiv 2025. — https://arxiv.org/html/2504.21385v1

### 3.15 Correction atmosphérique (physique)

- **6S / 6SV** — Vermote et al., IEEE TGRS 1997. —
  https://en.wikipedia.org/wiki/6S_(radiative_transfer_code)
- **Sen2Cor** — Main-Knorn et al., SPIE 2017. Sa couche de classification de scène
  sert directement de masque de composition. —
  https://step.esa.int/thirdparties/sen2cor/2.2.1/S2PAD-VEGA-SUM-0001-2.2.pdf
- **LaSRC** — Vermote et al., RSE 2016. —
  https://www.usgs.gov/core-science-systems/nli/landsat/landsat-collection-2-surface-reflectance
- **MAJA** — CNES/CESBIO. Multi-temporel par construction. —
  https://www.cesbio.cnrs.fr/multitemp/a-new-article-on-the-validation-of-majas-sentinel-2-surface-reflectances/
- **FORCE** — Frantz, 2019. Moteur ouvert « données prêtes à l'analyse », proche
  d'un producteur de tuiles. — https://force-eo.readthedocs.io/en/latest/about.html

### 3.16 Nuages : retrait et composites

- **SEN12MS-CR** — Ebel et al., IEEE TGRS 2020. — https://arxiv.org/pdf/2009.07683
- **DSen2-CR** — Meraner et al., ISPRS 2020. Fusion SAR-optique. Code
  **GPL-3.0**. — https://paperswithcode.com/paper/cloud-removal-in-sentinel-2-imagery-using-a
- **SEN12MS-CR-TS** — Ebel et al., IEEE TGRS 2022. Multi-dates. —
  https://arxiv.org/pdf/2201.09613
- **UnCRtainTS** — Ebel et al., CVPRW 2023. **Incertitude calibrée**, d'où un seuil
  pour décider quels pixels reconstruits publier. —
  https://openaccess.thecvf.com/content/CVPR2023W/EarthVision/html/Ebel_UnCRtainTS_Uncertainty_Quantification_for_Cloud_Removal_in_Optical_Satellite_Time_CVPRW_2023_paper.html
- **DiffCR** — Zou et al., 2023. — https://ar5iv.labs.arxiv.org/html/2308.04417
- **DB-CR** — Hu et al. (MERL), IEEE TGRS 2025. Pont de diffusion à partir de
  l'image nuageuse. — https://arxiv.org/abs/2504.03607
- **DMDiff** — Remote Sensing 2025. —
  https://mdpi-res.com/d_attachment/remotesensing/remotesensing-17-00965/article_deploy/remotesensing-17-00965.pdf
- **CM-CR** — Remote Sensing 2025. Diffusion distillée en une étape. —
  https://mdpi-res.com/d_attachment/remotesensing/remotesensing-17-03721/article_deploy/remotesensing-17-03721.pdf
- **AllClear** — NeurIPS D&B 2024. — https://arxiv.org/pdf/2410.23891
- **Apprentissage de prompts guidé par la tâche** — arXiv 2026. Un retrait de
  nuages réglé pour le réalisme dessert l'usage en aval. —
  https://arxiv.org/pdf/2601.12052
- **Flux résiduel ancré sur l'observation** — arXiv 2026. —
  https://papers.cool/arxiv/2607.02471
- **Mosaïque médiane sans nuages** — Planetary Computer. **N'invente rien.** —
  https://planetarycomputer.microsoft.com/docs/tutorials/cloudless-mosaic-sentinel2
- **Composite du meilleur pixel disponible** — Remote Sensing 2020. Moins de
  0,1 % de nuage résiduel. — https://www.mdpi.com/2072-4292/12/22/3680/html
- **Composites JRC sans nuages** — 2020. — https://forobs.jrc.ec.europa.eu/sentinel/explorer
- **SatelliteMaker** — arXiv 2025. Remplissage conditionné par le MNE. —
  https://arxiv.org/pdf/2504.12112

### 3.17 Saisons, neige, reflets

- **Neige synthétique par CycleGAN** — arXiv 2019. —
  https://arxiv.org/pdf/1911.12546
- **Retrait de neige par CycleGAN** — Remote Sensing 2024. —
  https://api.crossref.org/works/10.3390%2FRS16132352
- **Deux générateurs pour l'erreur saisonnière** — Remote Sensing 2025. —
  https://www.mdpi.com/2072-4292/17/3/523
- **W2GN Winter2Summer** — Korotin et al., ICLR 2021. **Échoue sur les grandes
  masses de neige**, ou se contente de verdir : un avertissement pour tout fond de
  carte « estivé ». — https://arxiv.org/pdf/1909.13082
- **Augmentation par transfert saisonnier** — ACRS 2022. —
  https://acrs-aars.org/proceeding/ACRS2022/ACRS22_88.pdf
- **Déreflet de Hedley** — IJRS 2005. Exige le proche infrarouge, donc inutilisable
  en RVB seul. — https://discovery.fiu.edu/display/pub69762
- **Revue des reflets solaires** — Kay, Hedley, Lavender, 2009. —
  https://doaj.org/article/0edfbd93d9554c13bc5495ac40eaa53c
- **Fumée par SWIR** — ACRS 2025. — https://acrs-aars.org/proceeding/ACRS2025/fp_abs-88.pdf
- **Détection de traînées (COCOS)** — AMT 2026. Détection seulement. —
  https://amt.copernicus.org/articles/19/3271/2026/

### 3.18 Objets transitoires, trous, dévers

- **Ligne de raccord par flux optique** — Zhang et al., Sensors 2018. Évite les
  objets mobiles. — https://www.ncbi.nlm.nih.gov/pmc/articles/PMC5948912/
- **Mélange par régions changées** — Remote Sensing 2016. —
  https://www.mdpi.com/2072-4292/8/10/842
- **Voitures fantômes en vraie ortho** — Nübel et al., DGPF 2023. —
  https://www.dgpf.de/src/tagung/jt2023/proceedings/paper/kknp/07_KKNP_dgpf2023_Nuebel_et_al.pdf
- **Nettoyer les images avant la SfM** — IJAEOG 2022. Le nettoyage appartient à
  l'amont du mosaïquage. — https://doaj.org/article/b9ae472fcb83403890ed6a2c4b9b2b7d
- **AVOR** — Xiang et al., ISPRS 2023. Retrait des véhicules sur image aérienne
  unique. — https://isprs-annals.copernicus.org/articles/X-1-W1-2023/629/2023/
- **Inpainting stratifié de véhicules** — Fraunhofer 2025. Retire aussi l'ombre
  de la voiture. — https://publica.fraunhofer.de/handle/publica/483006
- **Médiane temporelle** — tutoriel Earth Engine. Les voitures garées survivent. —
  https://developers.google.com/earth-engine/tutorials/tutorial_api_05
- **Criminisi** — IEEE TIP 2004 ; **PatchMatch** — Barnes et al., 2009 (code
  Adobe non commercial). — https://www.microsoft.com/en-us/research/?p=150053 ·
  https://www.cs.princeton.edu/gfx/pubs/Barnes_2009_PAR/
- **LaMa** — Suvorov et al., WACV 2022. Convolutions de Fourier, structures
  périodiques (rangées de toits, champs) : le défaut solide. —
  https://openaccess.thecvf.com/content/WACV2022/html/Suvorov_Resolution-Robust_Large_Mask_Inpainting_With_Fourier_Convolutions_WACV_2022_paper.html
- **MAT** — Li et al., CVPR 2022 (licence possiblement non commerciale). —
  https://openaccess.thecvf.com/content/CVPR2022/html/Li_MAT_Mask-Aware_Transformer_for_Large_Hole_Image_Inpainting_CVPR_2022_paper.html
- **RePaint** — Lugmayr et al., CVPR 2022. —
  https://openaccess.thecvf.com/content/CVPR2022/html/Lugmayr_RePaint_Inpainting_Using_Denoising_Diffusion_Probabilistic_Models_CVPR_2022_paper.html
- **Latent Diffusion (inpainting)** — Rombach et al., CVPR 2022. —
  https://openaccess.thecvf.com/content/CVPR2022/html/Rombach_High-Resolution_Image_Synthesis_With_Latent_Diffusion_Models_CVPR_2022_paper.html
- **KAO** — IEEE TGRS 2025. Inpainting satellite très haute résolution. —
  https://arxiv.org/html/2511.02462v1
- **AlignDiff** — preprint 2026. A priori MNE et prompts de lieu et de date. —
  https://www.preprints.org/manuscript/202602.1915/v1
- **NSPI (Landsat 7 SLC-off)** — Chen et al., RSE 2011. Le comblement
  multi-temporel classique. — https://pubs.usgs.gov/publication/70034729
- **Vraie ortho par z-buffer** — ISPRS Annals 2012. —
  https://isprs-annals.copernicus.org/articles/I-4/311/2012/isprsannals-I-4-311-2012.pdf
- **Vraie ortho, mise à jour** — Remote Sensing 2018 (par angle). —
  https://dx.doi.org/10.3390/rs10040581
- **Compensation d'occlusion par image conjuguée** — Rau et al., ACRS 2000. —
  https://acrs-aars.org/proceeding/ACRS2000/Papers/DPH00-3.htm
- **Tortho-Gaussian** — arXiv 2024. Vraie ortho par Gaussian splatting. —
  https://arxiv.org/pdf/2411.19594
- **BONAI / ROAM** — décalage toit → pied, mais en vecteurs, pas en pixels. —
  https://arxiv.org/abs/2204.13637v1
- **Lignes de raccord guidées par segmentation** — IET 2024. —
  https://doaj.org/article/62ba10f18ca7442ea973681869b7c29e

### 3.19 Évaluer la qualité

- **SSIM / MS-SSIM** — Wang et al., IEEE TIP 2004. **Faible sur les décalages de
  couleur globaux**, précisément ce qui rend un raccord visible. —
  https://ece.uwaterloo.ca/~z70wang/publications/ssim.html
- **LPIPS** — Zhang et al., CVPR 2018. — https://pypi.org/project/lpips/
- **DISTS** — Ding et al., TPAMI 2022. Tolère le rééchantillonnage de texture
  (herbe, eau) tout en pénalisant la structure. — https://arxiv.org/pdf/2005.01338
- **NIQE** — Mittal et al., 2013. A priori de photos au sol. —
  https://www.mathworks.com/help/images/ref/niqe.html
- **MANIQA** — CVPRW 2022. Artefacts de type GAN. —
  https://astro.paperswithcode.com/paper/maniqa-multi-dimension-attention-network-for
- **IQA sans référence comme a priori (LIQE, MUSIQ)** — arXiv 2024. On peut
  optimiser directement contre ces scores, donc **ils se trichent**. —
  https://arxiv.org/pdf/2403.06406
- **Critique de CLIP-IQA** — arXiv 2025. Biais sémantique. —
  https://arxiv.org/html/2511.09948v3
- **Q-Align / OneAlign** — Wu et al., ICML 2024. —
  https://proceedings.mlr.press/v235/wu24ah.html
- **SenseBench** — arXiv 2026 (NeurIPS 2026). Seul banc de perception bas niveau
  en télédétection pour les modèles vision-langage. — https://arxiv.org/pdf/2605.10576
- **QNR / HQNR** — Alparone et al. Pansharpening sans référence. —
  https://iris.cnr.it/handle/20.500.14243/263560
- **Évaluation du pansharpening en pleine résolution** — Scarpa & Ciotola, 2021. —
  https://arxiv.org/pdf/2108.06144
- **Indice de tampon de ligne de raccord** — 2018 (voir §3.13).

---

## 4. Jeux de données

| Jeu | Contenu | Usage ici | Licence |
|---|---|---|---|
| WorldStrat | SPOT 1,5 m ↔ Sentinel-2, environ 10 000 km² | super-résolution inter-capteurs | haute résolution **CC-BY-NC** ; S2 CC-BY |
| SEN2NAIP | 2 851 paires réelles + 35 314 synthétiques | super-résolution ×4 | à vérifier |
| S2-NAIP (AI2) | 1,2 million de paires | super-résolution, CLIPScore | à vérifier |
| opensr-test / SUPERIX | banc S2 avec hallucination et omission | **mesurer l'invention** | ouvert |
| BONAI | 268 958 bâtiments, décalage | dévers | ouvert |
| AVOR | véhicules masqués (DLR) | retrait de véhicules | à vérifier |
| RICE-1 / RICE-2 | voile (Google Earth) / nuages Landsat | dé-voilage, nuages | source GE : attention |
| SateHaze1k, RS-Haze, RSHaze5K, RRSHID | voile synthétique et réel | dé-voilage | à vérifier |
| SEN12MS-CR / -TS, AllClear | nuages S1/S2 | retrait de nuages | ouvert (vérifier) |
| SenseBench | perception bas niveau en télédétection | IQA | à vérifier |
| iHarmony4 | harmonisation | couleur | recherche |
| FLAIR-HUB, BD ORTHO, LiDAR HD | ortho 20 cm française | **paires (z, z+1) réelles et ouvertes**, entraînement auto-supervisé de pyramide | Etalab 2.0 / à vérifier |

**Point clé pour Tuile.** La BD ORTHO à 20 cm, ouverte, réduite au fil de la
pyramide, fournit des **paires (z, z+1) réelles en quantité illimitée** pour
entraîner une super-résolution ou un déblocage *au domaine exact des tuiles*
(JPEG, mosaïque, raccords). C'est la piste SelfDZSR appliquée à la pyramide,
sous licence ouverte.

---

## 5. Licences : poids et code

| Utilisable (permissif) | À éviter ou à vérifier |
|---|---|
| Real-ESRGAN (BSD-3) | SUPIR (non commercial) |
| OSEDiff, PiSA-SR (Apache-2.0 ; base SD-2.1 OpenRAIL à lire) | StableSR (S-Lab, non commercial, non vérifié) |
| NAFNet (MIT, non vérifié) | FluxSR (base FLUX.1-dev non commerciale) |
| SwinIR (Apache-2.0, non vérifié) | DSen2-CR (**GPL-3.0**) |
| Genesis (code public, licence à lire) | PatchMatch d'Adobe (non commercial) |
| LaMa, FBCNN, QGAC (licences à lire) | MAT (possiblement non commercial) |
| | WorldStrat haute résolution (CC-BY-NC) |
| | poids dérivés de Stable Diffusion (OpenRAIL : clauses d'usage) |

Rien de tout cela n'entre dans le dépôt. Un modèle s'exécute *hors* de Tuile
(étape de bake ou service externe), et seule la couche produite est consommée.
Les conditions du fournisseur de la source décident si une couche dérivée peut
exister.

---

## 6. Trous de la littérature, qui sont des questions de serveur de tuiles

- **La chaîne de dégradation réelle n'est modélisée nulle part** : FTM →
  orthorectification → Web Mercator → pyramide → JPEG 4:2:0 → ré-encodage. Le
  double JPEG de FBCNN et l'« ordre élevé » de HDI-PRNet sont les plus proches.
  Il manque un banc de vraies tuiles z12–z19.
- **Les raccords sous service indépendant.** Les méthodes sans raccord
  débruitent les voisines *conjointement*, alors qu'un serveur rend (z, x, y)
  seul. Il manque la génération fonction pure d'un halo fixe, et une métrique de
  raccord sur grille XYZ. Le VAE et les artefacts périodiques en une étape
  aggravent le problème.
- **La cohérence entre niveaux.** Rien n'impose *exactement* que z+1 généré se
  réduise en z servi (quantifié JPEG). CEM le fait analytiquement si l'on connaît
  le noyau. Rien non plus sur la couleur entre niveaux, hors le chaînage de
  `tuile-radiometry`.
- **Le déterminisme.** Les méthodes en une étape partant du latent basse qualité
  sont déterministes en principe ; l'exactitude au bit près entre GPU n'est
  traitée nulle part. D'où le principe de la couche stockée (§1.4).
- **Les champs de paramètres géoancrés.** Les LUT, HDRNet et PCT-Net supposent
  une image. Personne ne fait le champ continu inter-tuiles, alors que c'est la
  forme de `GainField`.
- **L'hallucination à l'échelle sub-métrique.** Pas de métrique pour les
  bâtiments ou routes inventés à z17–z19. opensr-test vise Sentinel-2. Au-delà
  de ×8, on est dans la génération. Il faut un drapeau de provenance et un
  plafond de fidélité par niveau.
- **Le RVB seul.** Les meilleurs dé-voileurs et retraits de nuages exigent SWIR,
  NIR ou SAR. Pour des tuiles RVB, seuls DCP, HOT/HTM, les réseaux RVB et les
  composites transfèrent.
- **Pas de base d'évaluation humaine (MOS) pour des orthophotos.** NIQE, MUSIQ,
  LIQE et CLIP-IQA ont des a prioris de photos au sol, et se trichent.
- **La balance des blancs aérienne.** Pas de littérature dédiée ; les a prioris
  caméra sont biaisés par la végétation et l'eau.
- **Le dévers dans l'image.** Corrigé en vecteurs (BONAI), jamais en pixels, et la
  vraie ortho demande DSM et images sources, dont un serveur de mosaïques ne
  dispose pas.

---

## 7. Expériences proposées

Elles sont ordonnées comme au §1. Chaque expérience produit un **film
avant/après, regardé** (`CLAUDE.md`), en plus des métriques.

1. **Anisotropie + mipmaps linéaires** (1–2 jours). Même trajectoire, avant et
   après ; LPIPS sur les frames rasantes, et l'œil.
2. **Noyau de reprojection** (2–3 jours). Bilinéaire contre bicubique contre
   Lanczos-2 contre « depuis z+1 » ; coût CPU mesuré dans la boucle de
   `reproject_to_geographic`.
3. **Déblocage JPEG hors ligne** (1 semaine). QGAC ou FBCNN sur une zone BD ORTHO
   recompressée comme une tuile web. Mesures : PSNR-B, LPIPS, et blocs aux bords
   de tuile (nouveau).
4. **Overzoom avec contrainte de pyramide** (2–3 semaines). HAT ou NAFNet
   (fidèle) contre PiSA-SR (une étape, déterministe), avec et sans projection CEM.
   Entraîné et testé sur des paires (z, z+1) de BD ORTHO. Mesures : écart
   parent/enfant réduit, hallucination façon opensr-test, raccords entre tuiles
   voisines, puis le film en rase-mottes.
5. **Champ de LUT géoancré** (2–3 semaines). Prolonger `GainField` en LUT 3D
   légère par cellule ; comparer le ΔE (CAM16-UCS) entre niveaux et aux raccords,
   contre le champ de gains actuel.

---

## 8. Relation avec les autres notes

- [`docs/18-color-harmonization.md`](../18-color-harmonization.md) : harmonisation
  de mosaïque, référence soleil et heure. La présente note en est l'extension
  (méthodes apprises, détail, atmosphère).
- [`3d-from-imagery-tiles.md`](3d-from-imagery-tiles.md) : l'ombre y est une
  *mesure* de hauteur. Ici, c'est une dégradation à retirer, ce qui renvoie aux
  références d'ombres de cette note-là (AISD, S-EO, SARU, Olbedo). Le délitage
  sert aux deux.
- [`seams-poles-islands-colour.md`](seams-poles-islands-colour.md) : raccords et
  couleur côté rendu.
