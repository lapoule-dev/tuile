<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Représentations neuronales de texture pour une pyramide de tuiles — état de l'art

Note de recherche exploratoire, rédigée le 2026-10-14. **Rien n'est implémenté.**
La question : les représentations neuronales d'images et de textures peuvent-elles
servir à un moteur de tuiles ouvert ? On en attend moins d'octets sur le réseau,
moins de mémoire GPU, un zoom continu et de nouveaux types de contenu. Les
familles examinées :

- compression neuronale de textures ;
- codecs d'images appris ;
- représentations implicites (INR) ;
- gaussiennes 2D ;
- champs d'embeddings.

La note dit aussi **à quelles conditions** ces représentations sont compatibles
avec les règles de Tuile : jamais de noir, déterminisme des films, wasm et
WebGPU.

**Méthode et limites.** Environ 100 recherches web sur trois axes, plus la
documentation de wgpu 29 (Context7). **Aucun article lu en texte intégral** :
arxiv.org, cvf.com et huggingface.co ne résolvaient pas. Les chiffres viennent
des résumés, des pages projet et de la presse spécialisée ; *(non vérifié)*
signale ce qui n'a pas été recoupé. Les chiffres BD-rate dépendent de la version
de VTM prise pour ancre : ne pas les comparer d'un article à l'autre sans
vérifier.

---

## 0. En une page

- **Le plus mûr est la compression neuronale de textures (NTC).** NVIDIA
  (SIGGRAPH 2023, SDK RTXNTC), AMD (NTBC, 2024), Intel (HPG 2025, SDK TSNC
  2026), Qualcomm (ECCV 2024). Ubisoft en a **livré** dans un jeu
  (*Assassin's Creed Mirage*). Mais tout est pensé pour des *matériaux* de 9 à
  16 canaux, où la corrélation entre canaux fait l'essentiel du gain. Une tuile
  d'imagerie n'a que **3 canaux**. Personne ne l'a appliquée à une pyramide de
  terrain ou de satellite.
- **La famille « latents en blocs BC » est la plus compatible avec une
  pyramide.** Ubisoft (BC6H), Intel (BC en LDR) et DBC stockent les latents dans
  des textures compressées *ordinaires*. Le **filtrage matériel, l'anisotropie,
  les mipmaps et le streaming continuent de marcher**, et un petit réseau décode
  dans le shader.
- **Le décodage dans le shader (« inference on sample ») est risqué pour un
  globe.** La sortie n'est pas filtrée, et il faut un filtrage stochastique (STF)
  avec accumulation temporelle, donc **du bruit sur une vue fixe**. En revanche,
  pour les **films de ferme** à plusieurs échantillons par pixel, le STF
  converge : ce défaut disparaît (§5, piste 6).
- **Le décodage au chargement (« inference on load ») est le seul chemin
  compatible avec « jamais de noir » dès aujourd'hui.** On décode une fois par
  tuile vers une texture résidente, on garde le parent jusqu'à l'échange, et un
  shader de calcul ordinaire suffit : c'est faisable en WebGPU.
- **Les codecs ajustés à chaque image (Cool-chic, C3) sont les plus légers à
  décoder.** Ils demandent 300 à 3 000 multiplications par pixel, soit 20 à
  200 MMAC par tuile 256², pour un débit proche de VVC. Leurs latents sont
  *déjà* une pyramide multi-résolution. Leur coût est à l'encodage : une
  optimisation par image.
- **Les gaussiennes 2D sont les plus portables.** Image-GS demande environ 300
  MAC/pixel, avec accès aléatoire et niveaux de détail natifs. Le rendu WebGPU
  de splats est mûr. Elles sont indépendantes de la résolution, ce qui donne un
  zoom au-delà du niveau maximal **lisse sans être halluciné**. Leur
  débit-distorsion ne bat JPEG que sous environ 0,4 bpp : intéressant pour les
  niveaux d'ensemble.
- **Un nouveau type de contenu à fort potentiel : les tuiles d'embeddings.**
  AlphaEarth Foundations (DeepMind 2025) publie des embeddings annuels à 64
  dimensions, à environ 10 m. Un shader peut en tirer à la volée des cartes
  thématiques ou une recherche par similarité (« clique un pixel, montre ce qui
  lui ressemble »). Cela demande peu de calcul et je ne connais aucun moteur de
  globe qui le fasse.
- **Trou commun à toutes les familles : la pyramide.** Aucune publication ne
  code un quadtree z0–z19 avec des tuiles enfants conditionnées par leur parent
  décodé. Aucune ne traite les raccords entre tuiles, ni la cohérence
  parent/enfant, ni le déterminisme entre GPU. Ce sont exactement les problèmes
  d'un serveur de tuiles (§7).
- **Une référence à mesurer d'abord, sans réseau de neurones :**
  - **KTX2/Basis** (textures compressées GPU) ;
  - **JPEG XL**, avec recompression sans perte des archives JPEG et décodeur
    Rust `jxl-rs`, qui serait livré dans Chrome 155 *(non vérifié)*.

  Toute piste neuronale doit battre ces deux-là sur des tuiles 256² réelles.

---

## 1. Pourquoi c'est intéressant pour Tuile, et d'où l'on part

**Ce que Tuile fait aujourd'hui :**

- les tuiles d'imagerie arrivent en JPEG ou PNG ;
- elles sont décodées sur le CPU (`image`, `crates/tuile-core/src/raster.rs:1520`)
  en RGBA8 ;
- la chaîne de mipmaps est calculée sur le CPU (`raster.rs:1593`), puis envoyée
  au GPU non compressée (`crates/tuile-wgpu/src/prepare.rs:423`) ;
- pas de KTX2, pas de format compressé GPU.

Une tuile 256² coûte donc 256 Ko de VRAM, plus environ un tiers pour les
mipmaps.

**Ce que la pile GPU permet :**

- **wgpu 29** (version du dépôt) expose `Features::EXPERIMENTAL_COOPERATIVE_MATRIX`,
  activée en WGSL par `enable wgpu_cooperative_matrix;`. Disponible sur **Metal**
  (Apple7/Mac2, MSL 2.3+) et sur **Vulkan** (`VK_KHR_cooperative_matrix`, selon
  le fabricant), en natif seulement ; ailleurs, la liste des configurations est
  vide. API instable.
- **WebGPU** (Chrome) :
  - `shader-f16` depuis Chrome 120 ;
  - `subgroups` livrés dans Chrome 134 ;
  - les *subgroup matrices* sont expérimentales et **disponibles sur 0 % des
    machines réelles** selon web3dsurvey.
  - Firefox et Safari n'ont pas été vérifiés.

  Dans le navigateur, il n'y a donc pas aujourd'hui d'accélération matricielle :
  on a de l'ALU ordinaire, du f16 et des sous-groupes.

**Ce qu'on pourrait gagner :**

1. **Les octets sur le réseau et dans le cache** : moins de stockage PMTiles et
   de transfert CDN.
2. **La VRAM** : plus de tuiles résidentes, donc moins de recours au parent
   flou. Utile pour le viewer, et surtout pour les films (`tuile-film-gpu`), qui
   tiennent une scène entière.
3. **Le zoom continu** : une représentation indépendante de la résolution
   (gaussiennes, INR) au-delà du niveau maximal, sans inventer de contenu.
4. **De nouveaux contenus** : des embeddings plutôt que des pixels.

---

## 2. Les familles, comparées sur ce qui compte pour des tuiles

| Famille | Décodage | Accès aléatoire | Filtrage / mips | Encodage | LOD natif | Chemin WebGPU | Licence type |
|---|---|---|---|---|---|---|---|
| **NTC** (latents + MLP par matériau) | MLP par texel, rapide avec matrices coopératives | oui | non filtré : STF + accumulation temporelle | optimisation par matériau | mips appris ensemble | non aujourd'hui (pas de matrices coopératives dans le navigateur) | SDK NVIDIA **non OSI** |
| **Latents en blocs BC** (Ubisoft, Intel, DBC) | MLP par texel sur des latents BC | oui | **matériel** (bi/trilinéaire, anisotrope) | entraînement à travers le décodage BC | mips BC | partiel (BC sur desktop, ASTC/ETC ailleurs) | articles ; code DBC |
| **Transcodage neuronal vers BC** (NTBC) | une fois au chargement → BC1/BC4 | oui (après transcodage) | **matériel** | par matériau | — | **oui** (shader de calcul) | article AMD |
| **Codecs autoencodeurs** (Ballé → TCM/MLIC, JPEG AI) | 7 à 216 kMAC/px (JPEG AI) | par image | après décodage | rapide (passe avant) | rarement (COMPASS, CAESR) | lourd ; JPEG AI absent des navigateurs | variable |
| **Codecs ajustés** (Cool-chic, C3) | **0,3 à 3 kMAC/px** | par image | après décodage | **lent** (optimisation par image) | **latents multi-résolution** | WGSL à écrire (aucun trouvé) | Cool-chic open source (licence à lire) ; C3 code public |
| **INR multi-échelle** (ACORN, MINER, BACON) | MLP par pixel ou par patch | oui | sorties limitées en bande = LOD | lent (par image) | **oui** | à écrire | articles |
| **Gaussiennes 2D** (GaussianImage, Image-GS, LIG) | **≈ 0,3 kMAC/px**, sans tri | **oui** | indépendant de la résolution | 2 à 10 s par image (Fast 2DGS) ; passe avant (Instant GaussianImage) | **oui** (Image-GS, LIG) | **mûr** (splats WebGPU) | codes publics |
| **Champs d'embeddings** (AlphaEarth) | tête linéaire dans le shader | oui | moyenne de vecteurs unitaires (à renormaliser ?) | fourni par le producteur | pyramide à construire | **oui** | jeu de données à vérifier |

---

## 3. Contraintes propres à une pyramide de tuiles

C'est la grille d'évaluation de toute piste. Aucune publication ne la remplit
entièrement (§7).

1. **Accès aléatoire aux bords des tuiles.** Les latents sont à plus basse
   résolution que les texels. Échantillonner au bord exige les latents de la
   voisine, donc il faut une **marge** (comme les bordures de textures
   virtuelles). Sinon : raccords, et avec des décodeurs entraînés séparément,
   raccords *de couleur*.
2. **Cohérence parent/enfant.** NTC apprend les mips *dans* une texture. Dans une
   pyramide, chaque niveau est une tuile distincte, donc parent et enfant peuvent
   diverger et produire des sauts. Il faut une contrainte du type « l'enfant
   décodé, réduit, redonne le parent », comme dans `tuile-radiometry` et dans la
   piste overzoom de [`imagery-tile-enhancement.md`](imagery-tile-enhancement.md).
3. **Jamais de noir.** Avec le décodage dans le shader, une tuile n'est visible
   que si latents *et* poids sont résidents : c'est un nouveau mode d'échec. Le
   décodage au chargement vers une texture résidente, avec le parent gardé
   jusqu'à l'échange, respecte la règle d'échange de `CLAUDE.md`.
4. **Déterminisme.** Matrices coopératives, DP4a et f16 accumulent dans des
   ordres et des précisions différents, donc les texels décodés diffèrent selon
   le fabricant. Pour les films, il faut décoder sur un chemin de référence ou
   stocker le décodé. Pour un codec entropique, il faut des **réseaux entiers**
   (Ballé et al., ICLR 2019) : un a priori calculé en flottant diverge entre
   serveur et client, et le décodage arithmétique casse.
5. **Vue fixe sans accumulation temporelle.** Le STF fait du bruit tant que la
   caméra ne bouge pas. Le filtrage collaboratif (NVIDIA, HPG 2025) donne un
   bilinéaire exact à un décodage par pixel, mais exige des sous-groupes dans le
   fragment shader.
6. **Trois canaux seulement.** L'avantage de NTC vient de la corrélation entre
   les 9 à 16 canaux PBR. Une tuile RVB en a peu : le gain doit venir de la
   corrélation *spatiale et inter-niveaux*, la pyramide.
7. **Débit d'encodage.** Des milliards de tuiles excluent l'optimisation par
   tuile. Il faut un ajustement amorti en une passe avant (Instant GaussianImage,
   COIN++, méta-apprentissage), ou bien un **décodeur partagé par région ou par
   niveau** avec des latents par tuile.
8. **Comparer à ce qui est réellement servi**, sur des tuiles 256² réelles :
   JPEG, WebP, AVIF, KTX2/Basis, JPEG XL. Les surcoûts de poids et d'entropie
   pèsent lourd sur une petite tuile, et les chiffres Kodak ou CLIC ne
   transfèrent pas tels quels.

---

## 4. État de l'art

Format : **Nom** — auteurs, lieu, année. Ce que ça fait et pourquoi c'est
pertinent ici. Chiffres. Code et licence. — lien.

### 4.1 Compression neuronale de textures (NTC) et descendants

- **NTC, Random-Access Neural Compression of Material Textures** — Vaidyanathan,
  Salvi, Wronski, Akenine-Möller, Ebelin, Lefohn (NVIDIA), SIGGRAPH 2023 / TOG.
  Tous les canaux et toute la chaîne de mips sont compressés ensemble dans des
  grilles de latents quantifiés ; un petit MLP par matériau décode, avec accès
  aléatoire par texel. Environ 16× plus de texels que BC à mémoire égale ou
  moindre ; bat AVIF et JPEG XL à bas débit. —
  https://research.nvidia.com/labs/rtr/neural_texture_compression/ · version
  arXiv : https://ar5iv.labs.arxiv.org/html/2305.17105
- **SDK RTXNTC** — NVIDIA, v0.10.0 bêta (2026). Trois modes :
  - **décodage dans le shader** (*on Sample*) ;
  - **décodage au chargement**, avec transcodage vers BCn (*on Load*) ;
  - **décodage des seules tuiles visibles**, guidé par le *sampler feedback*,
    vers une texture BCn éparse (*on Feedback*).

  Pour 2K×2K : 2,5 Mo sur disque contre 12 Mo en BCn et 32 Mo bruts. Environ
  5 bits/texel pour 9 à 10 canaux à 40–50 dB. Sortie **non filtrée**, STF
  recommandé. Licence **NVIDIA RTX SDK, non OSI** : non réutilisable dans un
  projet MIT/Apache. — https://github.com/NVIDIA-RTX/RTXNTC
- **Annonce RTXNTC 0.5** — Phoronix 2025. — https://www.phoronix.com/news/NVIDIA-RTXNTC-0.5-Beta/
- **RTX Kit** — NVIDIA 2025. « Jusqu'à 8× » moins de mémoire (chiffre
  fournisseur). — https://developer.nvidia.com/blog/get-started-with-neural-rendering-using-nvidia-rtx-kit/
- **Banc indépendant RTXNTC** — Tom's Hardware 2026. Plus de 80 % de VRAM en
  moins sur Sponza ; sur RTX 5090, +7 % de temps de frame en mode shader et
  +29 % en mode feedback par rapport au mode chargement. —
  https://tomshardware.com/pc-components/gpus/benchmarking-nvidias-rtx-neural-texture-compression-tech-that-can-reduce-vram-usage-by-over-80-percent
  · PCGH : https://www.pcgameshardware.de/Grafikkarten-Grafikkarte-97980/News/Nvidias-Neural-Texture-Compression-Test-Vorteile-VRAM-Nutzung-1525079/
- **Démo NTC** — TweakTown 2025. 6,5 Go → 970 Mo (méthode non précisée). —
  https://www.tweaktown.com/news/110886/nvidias-neural-texture-compression-cuts-vram-usage-from-6-5gb-down-to-970mb/index.html
- **NTBC, Neural Texture Block Compression** — Fujieda, Harada (AMD), EGSR 2024.
  Deux MLP par matériau produisent des **blocs BC1/BC4 standard au chargement** :
  shaders et filtrage inchangés, le gain porte sur le disque et le
  téléchargement (environ −70 % selon la presse, non vérifié). C'est **le schéma
  « transcodage neuronal » adapté à des tuiles servies par CDN**. —
  https://arxiv.org/pdf/2407.09543 · https://wccftech.com/amd-ntbc-neural-texture-block-compression-shrinks-storage-footprint-by-70/amp/
- **Neural Materials using Block-Compressed Features** — Weinreich, de Oliveira,
  Houdard, Nader (Ubisoft La Forge), Eurographics 2024. L'entraînement émule le
  décodage BC6H, donc les latents s'exportent en **textures BC6 mipmappées
  ordinaires**. Filtrage matériel, anisotropie, mips et streaming fonctionnent ;
  poids du décodeur en fp16 dans un buffer. **La conception la plus compatible
  avec une pyramide et avec WebGPU.** — https://arxiv.org/abs/2311.16121v2 ·
  GDC 2024 : https://gdcvault.com/play/1034439/Machine-Learning-Summit-Real-time
- **NTC livrée dans *Assassin's Creed Mirage*** — Ubisoft La Forge. Première
  livraison en production trouvée. —
  https://www.ubisoft.com/en-us/studio/laforge/news/415slmB3ZGzv8d2LhO9QT/shipping-neural-texture-compression-in-assassins-creed-mirage
- **Neural Block Texture Compression with Cooperative Vectors** — Belcour, Benyoub
  (Intel Labs), HPG 2025. Des formats BC en LDR servent de latents. 4K, 9 canaux,
  anisotrope en 1080p : environ 28 Mo de VRAM et 0,55 ms sur Arc B580, contre
  13 ms en FMA logiciel (**23×**). — https://arxiv.org/abs/2506.06040
- **SDK Intel TSNC** — GDC 2026. Environ 9× (A) et 18× (B) plus petit que BCn ;
  annoncé déterministe ; alpha *(non vérifié)*. —
  https://wccftech.com/intel-texture-set-neural-compression-sdk-tsnc-18x-smaller-textures/amp/
- **Neural Graphics Texture Compression Supporting Random Access** — Farhadzadeh et
  al. (Qualcomm), ECCV 2024. Encodeur convolutif + MLP qui prend position et
  niveau de mip ; les mips sont servis en changeant le pas sur **une seule** grille
  de latents. Pertinent pour la cohérence de LOD. —
  https://arxiv.org/html/2407.00021v1
- **NBC, Neural Block Compression** — Shi et al., AAAI 2025. Débit variable,
  orienté mobile. — https://ojs.aaai.org/index.php/AAAI/article/view/32738
- **DBC, Differentiable Block Compression** — Zhuang, Liu, Liu, EGSR 2025 (CC BY
  4.0). Encodage BC différentiable : les latents s'entraînent à travers les
  formats BC natifs. Code LibTorch (licence non vérifiée). —
  https://diglib.eg.org/handle/10.2312/sr20251199 · https://github.com/Ubpa/dbc

### 4.2 Filtrage et mipmaps des textures neuronales

- **STF, Filtering After Shading with Stochastic Texture Filtering** — Pharr,
  Wronski, Salvi, Fajardo, I3D 2024 (meilleur article). Un texel tiré au hasard
  par échantillon remplace la moyenne pondérée, et la convergence vient du TAA,
  du débruiteur ou de plusieurs échantillons. C'est ce qui rend le décodage dans
  le shader possible ; sans TAA, sur vue fixe, il **fait du bruit**. —
  https://research.nvidia.com/labs/rtr/publication/pharr2024stochtex · arXiv
  https://arxiv.org/abs/2407.06107v1 · slides
  https://research.nvidia.com/labs/rtr/publication/pharr2024stochtex/stochtex-slides.pdf
- **Réutilisation d'échantillons STF** — Wronski et al., 2025. Partage des texels
  décodés entre pixels voisins en agrandissement : c'est le cas de l'imagerie au
  zoom maximal. —
  https://research.nvidia.com/labs/rtr/publication/wronski2025stfreuse/stf_sample_reuse.pdf
- **Collaborative Texture Filtering** — Akenine-Möller, Ebelin, Pharr, Wronski,
  HPG 2025. Les lanes d'un front d'onde décodent des texels distincts et les
  échangent : bilinéaire exact avec au plus un décodage par pixel au-delà d'un
  agrandissement d'environ 1,6–2,4×. Exige des **sous-groupes dans le fragment
  shader**. —
  https://research.nvidia.com/labs/rtr/publication/akeninemoller2025collaborative
- **En résumé, quatre façons de traiter les mips :**
  - NTC apprend la chaîne de mips d'un bloc ;
  - Ubisoft, Intel et DBC délèguent au matériel BC ;
  - Qualcomm fait varier le pas sur une grille unique ;
  - NeuMIP prend la taille du filtre en entrée du réseau.

### 4.3 Matériaux neuronaux (contexte)

- **NeuMIP** — Kuznetsov et al., SIGGRAPH 2021. Pyramides de textures neuronales
  et « mip-mapping neuronal ». — https://cseweb.ucsd.edu/~viscomp/projects/NeuMIP/
- **Real-Time Neural Appearance Models** — Zeltner et al. (NVIDIA), TOG 2024.
  Graphes de matériaux cuits en latents hiérarchiques et petits décodeurs,
  plus de 10× plus rapides. —
  https://research.nvidia.com/labs/rtr/neural_appearance_models/assets/nvidia_neural_materials_author_paper.pdf
- **MIPNet** — Gauthier et al. Préfiltrage de mips SVBRDF. Utile seulement si des
  cartes de normales de terrain sont un jour streamées. —
  https://papercopilot.com/cat/cs/cg/materials-and-textures/

### 4.4 Substrat GPU (où l'inférence peut tourner)

- **D3D12 Cooperative Vector (préversion)** — Microsoft, juin 2025. NTC d'Intel
  environ 10× plus rapide sur Arc. — https://devblogs.microsoft.com/directx/cooperative-vector/
- **Shader Model 6.9 et la suite** — Microsoft, septembre 2025. —
  https://devblogs.microsoft.com/directx/shader-model-6-9-and-the-future-of-cooperative-vector/
- **Shader Model 6.10 + Agility SDK 720 (préversion)** — Microsoft, avril 2026.
  `linalg::Matrix`. Le chemin DX12 de RTXNTC y est « pour test seulement ». Des
  sources de la GDC 2026 sont contradictoires. —
  https://devblogs.microsoft.com/directx/shader-model-6-10-agilitysdk-720-preview/
- **Microsoft à la GDC 2026** — la NTC est au centre de la feuille de route
  DirectX. —
  https://windowsreport.com/microsoft-showcases-neural-rendering-and-dxr-2-0-plans-for-directx-at-gdc-2026/
- **Cooperative Vectors and Neural Rendering (exposé)** — S. Hargreaves, 2025. —
  https://shawnhargreaves.com/CooperativeVectorsAndNeuralRendering.pdf
- **VK_NV_cooperative_vector** — Khronos/NVIDIA, 2025. Produit matrice-vecteur
  par thread. Explique pourquoi la *matrice* coopérative, partagée par un
  sous-groupe, convient mal à l'ombrage divergent. —
  https://docs.vulkan.org/features/latest/features/proposals/VK_NV_cooperative_vector.html
- **VK_KHR_cooperative_matrix** — Khronos. La primitive multi-fabricants sur
  laquelle s'appuient wgpu et Dawn. —
  https://docs.vulkan.org/tutorial/latest/Advanced_Vulkan_Compute/09_Specialized_Math/02_cooperative_matrices.html
- **Slang coopVec** — Slang émet du WGSL, mais l'abaissement des vecteurs
  coopératifs vers WGSL n'est pas documenté. —
  https://shader-slanggithubio-aidanfnv-wip.readthedocs.io/en/latest/external/core-module-reference/global-decls/coopvecmatmuladd-47ad.html
- **OptiX + cooperative vectors** — NVIDIA 2025. —
  https://developer.nvidia.com/blog/neural-rendering-in-nvidia-optix-using-cooperative-vectors
- **WMMA sur RDNA 4** — AMD GPUOpen 2025. La NTC est citée comme charge
  motivante. — https://gpuopen.com/learn/wmma-guide-amd-rdna-4-gpus-part-3/
- **Metal 4 Shader ML** — Apple WWDC25. Petits réseaux dans les shaders,
  matériaux neuronaux en exemple. —
  https://developer.apple.com/videos/play/wwdc2025/262/?time=678
- **WebGPU** :
  - `shader-f16` (Chrome 120) : https://developer.chrome.com/blog/new-in-webgpu-120
  - sous-groupes en essai (Chrome 128) : https://developer.chrome.com/blog/new-in-webgpu-128
  - **sous-groupes livrés** (Chrome 134) : https://developer.chrome.com/blog/new-in-webgpu-134
  - la suite annoncée (subgroup matrices) : https://developer.chrome.com/blog/next-for-webgpu
  - **disponibilité réelle de 0 %** des subgroup matrices :
    https://web3dsurvey.com/webgpu/features/chromium-experimental-subgroup-matrix
  - implémentation Dawn :
    https://dawn.googlesource.com/dawn/+/1a95a0a500099a285bfdfd9ea0682afcc89c7c31
- **wgpu, matrices coopératives** — gfx-rs, wgpu 29. Natif seulement, instable ;
  tailles interrogées à l'exécution (typiquement 8×8 sur Metal, 16×16 f16 sur
  AMD Vulkan). — https://wgpu.rs/doc/wgpu/documentation/extensions/cooperative_matrices/index.html
  · exemple : https://wgpu.rs/doc/wgpu_examples/cooperative_matrix/index.html
- **Régression de qualité fp16 sous WebGPU (Anime4K)** — PR communautaire. Mise en
  garde sur les décodeurs f16 et la cohérence entre appareils. —
  https://git.tdem.in/bloc97/Anime4K/pull/221
- **Ordres de grandeur sans matériel matriciel** : Intel mesure 13 ms en FMA
  logiciel contre 0,55 ms en vecteurs coopératifs (23×) ; NVIDIA mesure 2 à 4× ;
  le chemin DP4a de RTXNTC sert « à la validation ». **En WebGPU aujourd'hui,
  l'inférence par pixel est risquée ; l'inférence par tuile au chargement est
  réaliste.**

### 4.5 Codecs d'images appris

**Fondations :**

- **Ballé et al., ICLR 2017** — le gabarit analyse → quantification → synthèse +
  entropie apprise. — https://arxiv.org/pdf/1611.01704
- **Hyperprior d'échelle** — Ballé et al., ICLR 2018. Décodage entièrement
  parallèle ; la référence « bon marché » pour le GPU. —
  https://arxiv.org/pdf/1802.01436
- **Autorégressif + hiérarchique** — Minnen et al., NeurIPS 2018. Premier codec
  appris à battre BPG, mais son contexte décode position par position : mauvais
  pour le GPU. —
  https://papers.neurips.cc/paper/2018/hash/53edebc543333dfbf7c5933af792c9c4-Abstract.html
- **Cheng 2020** — CVPR 2020. Premier codec appris au niveau de VVC en PSNR. Code
  public. —
  https://openaccess.thecvf.com/content_CVPR_2020/html/Cheng_Learned_Image_Compression_With_Discretized_Gaussian_Mixture_Likelihoods_and_Attention_CVPR_2020_paper.html
- **Autorégressif par canal** — Minnen & Singh, 2020. Le modèle d'entropie
  favorable au GPU, derrière ELIC, TCM et MLIC. — https://alphaxiv.org/abs/2007.08739
- **ELIC** (CVPR 2022) ; **TCM** (CVPR 2023, code public) ; **MLIC++** (2023,
  environ −13 à −15 % contre VTM) ; **HPCM** (2025, environ −19 à −22 % contre
  VTM-22) ; **ChWDTA** (2026, domaine des ondelettes) —
  https://openaccess.thecvf.com/content/CVPR2022/html/He_ELIC_Efficient_Learned_Image_Compression_With_Unevenly_Grouped_Space-Channel_Contextual_CVPR_2022_paper.html
  · https://openaccess.thecvf.com/content/CVPR2023/html/Liu_Learned_Image_Compression_With_Mixed_Transformer-CNN_Architectures_CVPR_2023_paper.html
  · https://arxiv.org/pdf/2307.15421 · https://arxiv.org/pdf/2507.19125 ·
  https://arxiv.org/pdf/2606.00111
- **Compromis débit–distorsion–perception** — Blau & Michaeli, ICML 2019. Le
  réalisme coûte du débit ou de la fidélité. —
  https://proceedings.mlr.press/v97/blau19a.html

**Génératifs à très bas débit (risque d'hallucination pour des géodonnées) :**

- **HiFiC** (NeurIPS 2020) ; **MS-ILLM** (ICML 2023) ; **DiffC** (2022, décodage
  progressif) ; **CDC** (NeurIPS 2023) ; **PerCo** (ICLR 2024, jusqu'à 0,003 bpp,
  soit environ 25 octets par tuile 256², donc presque tout est inventé) ;
  **PerCoV2** (2025) —
  https://papers.nips.cc/paper/2020/hash/8a50bae297807da9e97722a0b3fd8f27-Abstract.html
  · https://arxiv.org/pdf/2301.11189v3 · https://arxiv.org/pdf/2206.08889 ·
  https://proceedings.neurips.cc/paper_files/paper/2023/hash/ccf6d8b4a1fe9d9c8192f00c713872ea-Abstract.html
  · https://mlanthology.org/iclr/2024/careil2024iclr-image ·
  https://arxiv.org/html/2503.09368v1

**Normes et pratique :**

- **JPEG AI** — ISO/IEC 6048-1:2025. Premier standard de codage appris. Gain de
  12,5 à 27,9 % contre VVC Intra pour **7 à 216 kMAC/px**, soit 0,46 à 14 GMAC
  par tuile 256². **Aucun navigateur ne le décode.** — https://jpeg.org/jpegai/ ·
  https://jpeg.org/items/20240507_press.html · profils :
  https://www.iso.org/fr/standard/87577.html
- **JPEG XL dans Chrome** — Chrome 155 décoderait JPEG XL via **jxl-rs, un
  décodeur Rust pur** (blog Chrome, octobre 2026, *non vérifié*). JPEG XL fait
  aussi la recompression sans perte des JPEG existants et le décodage
  progressif. — https://developer.chrome.com/blog/jpeg-xl-in-chrome
- **Support de JPEG XL** (Safari partiel), méthodologie de comparaison, AVIF
  contre JPEG XL — https://www.corewebvitals.io/pagespeed/jpeg-xl-core-web-vitals-support
  · https://cloudinary.com/blog/contemplating-codec-comparisons ·
  https://speedvitals.com/blog/avif-vs-jpeg-xl/

**Télédétection :**

- **Codage prédictif neuronal contre CCSDS-123** — Valsesia, Bianchi, Magli,
  2024. — https://arxiv.org/pdf/2403.17677
- **Encodeur embarqué, CNN au sol** — Valsesia & Magli, 2019. L'inverse d'un
  serveur de tuiles. — https://ar5iv.arxiv.org/html/1907.02959
- **VAE embarqué à complexité réduite** — de Oliveira et al., 2021. —
  https://www.mdpi.com/2072-4292/13/3/447
- **TerraCodec** — 2025. Sentinel-2 : **3 à 10× mieux que les codecs classiques**
  à qualité égale. Entraîner sur le domaine paie. —
  https://arxiv.org/html/2510.12670v1
- **HyCASS** (2025), **SpectralNet-X** (ISPRS 2026) — hyperspectral. —
  https://arxiv.org/html/2507.23447v1 ·
  https://isprs-annals.copernicus.org/articles/XI-3-2026/279/2026/
- **Codec ROI sensible aux nuages** — Remote Sensing 2023. Moins de bits pour le
  nuage et l'eau. — https://www.mdpi.com/2072-4292/15/2/522/htm

**Codage échelonnable et progressif (l'adéquation clé avec une pyramide) :**

- **Hyperprior grossier → fin** — Hu, Yang, Liu, AAAI 2020. Pyramide de latents,
  mais non décodable à chaque résolution. —
  https://ojs.aaai.org/index.php/AAAI/article/view/6736
- **Multiple Scale Latents** — 2026. — https://arxiv.org/pdf/2608.10952
- **COMPASS** — CVPR 2024 *(non vérifié)*. Une couche de base basse résolution,
  puis chaque couche d'amélioration code le résidu d'une super-résolution depuis
  la couche inférieure. **Le plus proche de « envoyer z, puis un résidu pour
  z+1 ».** — https://arxiv.org/pdf/2309.07926
- **CAESR** — 2022. Le **codage conditionnel** (l'encodeur voit la source *et* la
  base agrandie) bat le simple codage de résidu. D'où l'idée de conditionner
  l'enfant sur le parent décodé, que le client possède déjà. —
  https://deepai.org/publication/caesr-conditional-autoencoder-and-super-resolution-for-learned-spatial-scalability
- **iWave++** — ondelettes à filtres appris, flux échelonnable ; version
  parallélisée **plus de 350× plus rapide** au décodage (ICIP 2024). —
  https://arxiv.org/pdf/2405.12631
- **ProgDTD** — CVPRW 2023. Tout préfixe du flux est décodable : couches de
  qualité *dans* une tuile. — https://arxiv.org/pdf/2305.02145
- **Every Packet Counts** — 2026. Robustesse aux pertes, utile si un chargement de
  tuile est interrompu. — https://arxiv.org/pdf/2608.11096
- **Réseaux entiers** — Ballé, Johnston, Minnen, ICLR 2019. Décodage entropique
  **identique au bit près** sur toutes les plateformes : obligatoire si un serveur
  CPU encode et des clients WebGPU variés décodent. —
  https://research.google/pubs/integer-networks-for-data-compression-with-latent-variable-models/
- **ONNX Runtime Web (WebGPU)** — Microsoft 2024. Aucun banc publié de codec
  appris trouvé. — https://cloudblogs.microsoft.com/opensource/author/satya-jandhyala/
- **zipnet** — codec appris en Rust → wasm : « quelques secondes » par image. Le
  chemin CPU/wasm est trop lent. — https://github.com/Conzel/zipnet

### 4.6 Représentations implicites (INR) et codecs ajustés

**Fondations :**

- **SIREN** — Sitzmann et al., NeurIPS 2020. —
  https://papers.nips.cc/paper/2020/hash/53c04118df112c13a8c34b38343b9c10-Abstract.html
- **Fourier features** — Tancik et al., NeurIPS 2020. La largeur de bande joue le
  rôle d'un réglage de LOD. — https://arxiv.org/abs/2006.10739
- **Biais spectral** — Rahaman et al., ICML 2019. Un MLP de coordonnées naïf
  floute. — https://proceedings.mlr.press/v97/rahaman19a
- **Instant-NGP** — Müller et al. (NVIDIA), SIGGRAPH 2022. Tables de hachage
  multi-résolution et MLP minuscule ; une image gigapixel ajustée en
  interactif. — https://arxiv.org/abs/2201.05989v2
- **ACORN** — Martel et al., SIGGRAPH 2021. Quadtree de blocs de descripteurs,
  adaptatif ; 51 200 × 19 456 pixels à environ 40 dB. **L'analogue INR le plus
  proche d'un quadtree de tuiles.** — https://arxiv.org/abs/2105.02788v1
- **BACON** — Lindell et al., CVPR 2022. Sorties intermédiaires limitées en bande,
  sans repliement, qui **correspondent naturellement aux niveaux de zoom**. —
  https://arxiv.org/abs/2112.04645v2
- **MINER** — Saragadam et al., ECCV 2022. Pyramide laplacienne avec un petit MLP
  par patch et par niveau ; pas de réseau là où le résidu est nul. Moins de 25 %
  des paramètres d'ACORN. **La disposition la plus proche d'une pyramide XYZ.** —
  https://arxiv.org/pdf/2202.03532
- **Revue Neural Fields** — Xie et al., CGF 2022. —
  https://arxiv.org/abs/2111.11426v1

**INR pour la compression :**

- **COIN / COIN++** — Dupont et al., 2021 / TMLR 2022. Modulations d'un réseau de
  base méta-appris, encodage environ 100× plus rapide ; testé sur ERA5. —
  https://arxiv.org/abs/2201.12904v2
- **Compression par INR** — Strümpler et al., ECCV 2022. Initialisation
  méta-apprise. — https://arxiv.org/abs/2112.04267v1
- **COOL-CHIC** — Ladune et al. (Orange), ICCV 2023. Latents hiérarchiques et
  petits réseaux : **629 paramètres, 680 multiplications par pixel**, au niveau de
  HEVC. Open source (licence à lire). —
  https://openaccess.thecvf.com/content/ICCV2023/html/Ladune_COOL-CHIC_Coordinate-based_Low_Complexity_Hierarchical_Image_Codec_ICCV_2023_paper.html
- **Codage ajusté à complexité réduite** — Orange, 2024. **300 MAC/px** bat déjà
  HEVC ; décodeur CPU quasi temps réel. — https://arxiv.org/abs/2403.11651v1
- **Cool-chic 5.0** — Orange, 2026 *(non vérifié)*. −11 % contre VVC, 10× moins
  d'itérations d'encodage, décodage 250× moins cher que MLIC++. —
  https://www.emergentmind.com/papers/2605.02726
- **C3** — Kim et al. (DeepMind), CVPR 2024. Au niveau de VTM sur CLIC2020 avec
  **moins de 3 000 MAC/px**. Code : github.com/google-deepmind/c3. —
  https://arxiv.org/abs/2312.02753v1
- **NeRV** — Chen et al., NeurIPS 2021. Un réseau par *image* plutôt que par
  pixel : décodage 38 à 132× plus rapide. Suggère « un réseau par tuile ». —
  https://papers.nips.cc/paper/2021/hash/b44182379bf9fae976e6ae5996e13cd8-Abstract.html
- **NeRV-Dec** — ECCV 2024. — https://www.arxiv.org/abs/2409.19429
- **Streaming progressif de poids** — Li & Varshney, 3DV 2022. Rendu à partir de
  moins de 7 % des poids, raffiné à mesure qu'ils arrivent. —
  https://www.cs.umd.edu/~varshney/bib/li2022progresssive.abstract
- **VQAD, Variable bitrate neural fields** — Takikawa et al. (NVIDIA), SIGGRAPH
  2022. — https://research.nvidia.com/labs/toronto-ai/vqad

### 4.7 Gaussiennes 2D

- **GaussianImage** — Zhang et al., ECCV 2024. 8 paramètres par gaussienne, somme
  indépendante de l'ordre donc **sans tri** ; **1 500 à 2 000 images/s** au
  décodage. Débit-distorsion comparable à COIN/COIN++. Code public. —
  https://ecva.net/papers/eccv_2024/papers_ECCV/html/1421_ECCV_2024_paper.php
- **GaussianImage++** — 2025. — https://arxiv.org/pdf/2512.19108
- **Image-GS** — Zhang, Li et al. (NYU, Intel, AMD), SIGGRAPH 2025. Gaussiennes
  anisotropes guidées par l'erreur ; **environ 0,3 kMAC/px, accès aléatoire,
  hiérarchie de LOD** ; la compression de textures est une application citée.
  Code : NYU-ICL/image-gs. — https://arxiv.org/abs/2407.01866
- **LIG, Large Images are Gaussians** — Zhu et al., AAAI 2025. « Niveau de
  gaussienne » : base basse fréquence puis détail. **Testé sur de grandes images
  satellite.** — https://arxiv.org/abs/2502.09039
- **SGI** — 2026. Gaussiennes structurées + entropie ; bat JPEG à bas débit. —
  https://arxiv.org/pdf/2603.07789
- **Allocation guidée par la structure** — 2025. −43 % BD-rate contre GSImage ;
  bat JPEG sous environ 0,4 bpp. — https://arxiv.org/pdf/2512.24018
- **Instant GaussianImage** — ICCV 2025. Un réseau en une passe prédit
  l'ensemble, puis un court affinage ; **10× moins d'entraînement**. —
  https://arxiv.org/html/2506.23479
- **Fast 2DGS** — 2025. 40 dB après 2 s d'affinage. —
  https://arxiv.org/abs/2512.12774v1
- **GSVC** — 2025. Vidéo, 1 500 images/s en 1080p. —
  https://arxiv.org/html/2501.12060v2
- **Textured Gaussians** — Chao et al., 2024–25. Un patch de texture par
  gaussienne. — https://arxiv.org/abs/2411.18625
- **Rendu de splats WebGPU** — PlayCanvas/SuperSplat (MIT, LOD streamé), Visionary
  (inférence ONNX et splats dans la même frame), WebSplatter (les portages naïfs
  depuis CUDA échouent sur certains appareils). —
  https://radiancefields.com/supersplat-ships-compute-based-webgpu-rendering-and-automatic-streamed-lod
  · https://arxiv.org/pdf/2512.08478v1 · https://arxiv.org/html/2602.03207v1

### 4.8 Champs géospatiaux et embeddings

- **Encodeur de position harmoniques sphériques + SIREN** — Rußwurm et al., ICLR
  2024. Sans artefacts aux pôles ; pour des champs globaux lisses. —
  https://arxiv.org/abs/2310.06743v1
- **SatCLIP** (AAAI 2025), **GeoCLIP** (NeurIPS 2023), **SINR** (ICML 2023) —
  encodeurs de position. — https://arxiv.org/abs/2311.17179v3 ·
  https://arxiv.org/pdf/2309.16020 · https://proceedings.mlr.press/v202/cole23a.html
- **Météo et climat dans des réseaux** — Huang & Hoefler, ICLR 2023. ERA5
  compressé de 300 à 3 000×, avec moins de 2 % d'erreur en aval. —
  https://arxiv.org/abs/2210.12538v3
- **CRA5** — 2024. ERA5 passe de 226 To à 0,7 To. — https://arxiv.org/abs/2405.03376v2
- **Revue : compression neuronale avec pertes pour la géoanalyse** — 2025. Les INR
  **perdent les extrêmes** (le cœur d'un ouragan). — https://arxiv.org/pdf/2503.01505
- **AlphaEarth Foundations** — Brown et al. (DeepMind), 2025. Modèle de « champ
  d'embeddings » fusionnant optique, radar, LiDAR… en une représentation par
  pixel. Couches annuelles de 2017 à 2024. — https://arxiv.org/abs/2507.22291
- **Satellite Embedding dataset** — DeepMind / Earth Engine, 2025. **64
  composantes sur la sphère unité, environ 10 m**. Une pyramide d'embeddings à 64
  canaux est un contenu de tuile plausible (licence à vérifier). —
  https://deepmind.google/blog/alphaearth-foundations-helps-map-our-planet-in-unprecedented-detail/
- **TerraMind** (IBM/ESA, 2025), **Clay** (embeddings par vignette), **Prithvi-EO
  2.0** (NASA/IBM, 2024) — modèles fondation d'observation de la Terre. —
  https://arxiv.org/abs/2504.11171 · https://data.source.coop/clay/lgnd-embeddings/README.md
  · https://arxiv.org/abs/2412.02732v1
- **SR-FEINR** (2023, ×30) et **INR Sentinel-2 temporel** (EGU 2026) —
  super-résolution par INR. — https://www.ncbi.nlm.nih.gov/pmc/articles/PMC10098664/
  · https://meetingorganizer.copernicus.org/EGU26/EGU26-14956.html
- **ImpliSat** — Cho et al., IGARSS 2025. INR modulée pour imagerie
  multispectrale ; sans accès aléatoire, ni tuilage, ni temps réel. —
  https://arxiv.org/abs/2506.01234 · aussi https://neurips.cc/virtual/2024/98200
- **Neural Dynamic GI** — 2026. Latents neuronaux à accès aléatoire **intégrés à
  une boucle de texturation virtuelle** (table de pages, retour de visibilité,
  décodage des seules tuiles visibles, cache). **Le gabarit publié le plus proche
  de « latents neuronaux + pyramide de tuiles + résidence ».** —
  https://arxiv.org/pdf/2604.12625
- **Précision** : des lat/lon globaux en f32 limitent un champ neuronal à environ
  2 m à l'équateur (24 bits de mantisse sur 180°). Un champ doit prendre des
  **coordonnées locales par tuile**, conformément à la règle 7 de `CLAUDE.md`.

### 4.9 Références classiques à battre

- **KTX 2.0 + KHR_texture_basisu** — Khronos, 2021. Basis Universal (ETC1S, UASTC)
  transcodé au chargement vers le format GPU natif ; c'est ce que glTF, donc 3D
  Tiles 1.1, transporte déjà. — https://www.khronos.org/news/press/khronos-ktx-2-0-textures-enable-compact-visually-rich-gltf-3d-assets
  · https://khronos.org/ktx/
- **ASTC** — blocs de 4×4 à 12×12, soit 0,89 à 8 bpp ; cible WebGPU
  `texture-compression-astc`. — https://en.wikipedia.org/wiki/Adaptive_scalable_texture_compression
- **Basis Universal 2.x** — paquet 2026. —
  https://mirror.arizona.edu/ubuntu/pool/universe/b/basis-universal

---

## 5. Pistes pour Tuile (innovantes, réalisables avec quelques GPU)

Les coûts de calcul sont des estimations, pas des mesures.

**Piste 0, préalable et sans réseau de neurones : la référence.** Mesurer
KTX2/UASTC (VRAM ÷ 4 à 8 avec le filtrage matériel) et JPEG XL (recompression
sans perte de l'archive JPEG, décodeur Rust `jxl-rs`) sur de vraies tuiles.
C'est le seuil que toute piste neuronale doit battre. Il s'implémente sans GPU
d'entraînement.

**Piste 1 : codec de pyramide conditionnel, décodé au chargement.**
- **L'idée** : un décodeur **partagé par région et par niveau**, des latents par
  tuile, et un **enfant conditionné par son parent décodé**, que le client a déjà
  (CAESR > résidu). La sortie est transcodée en BC7/ASTC ou RGBA *au chargement*
  par un shader de calcul ordinaire, comme NTBC.
- **Ce qui est préservé** : le filtrage matériel, l'anisotropie, les mips ; la
  règle « jamais de noir » (le parent reste affiché jusqu'à l'échange) ;
  WebGPU dès aujourd'hui.
- **Pourquoi c'est nouveau** : aucune publication ne code un quadtree de tuiles
  conditionnellement.
- **Données** : la pyramide BD ORTHO, ouverte. **Calcul** : quelques jours sur 1
  à 4 GPU (estimation).
- **Points durs** :
  - le déterminisme du décodage entropique, d'où des réseaux entiers ;
  - la marge aux bords ;
  - une chaîne de 19 niveaux dépendants casse l'accès aléatoire, d'où des
    « niveaux clés » tous les k zooms, comme les GOP vidéo.

**Piste 2 : un codec ajusté natif de la pyramide (de type Cool-chic ou C3).**
- **L'idée** : les grilles de latents multi-résolution *sont* la pyramide. La
  grille de niveau L sert la tuile de zoom L. On optimise **par région** et non
  par tuile, avec une initialisation en une passe avant pour amortir l'encodage.
- **Décodage** : 300 à 3 000 MAC/px en WGSL, à écrire (aucune implémentation
  trouvée).
- **Pourquoi c'est nouveau** : un codec ajusté dont la hiérarchie de latents est
  alignée sur une pyramide XYZ.
- **Calcul** : surtout l'encodage hors ligne, une ferme de GPU pour une région.

**Piste 3 : des tuiles en gaussiennes 2D pour l'ensemble et l'overzoom.**
- **L'idée** : Image-GS ou LIG par tuile ou par région. Un LOD natif. Au-delà du
  niveau maximal, le rendu est **lisse et sans invention**, une alternative
  honnête à la super-résolution générative.
- **Atouts** : le rendu WebGPU de splats est mûr, et l'idée est cousine des
  splats 3D côté 3D Tiles.
- **Limite** : le débit-distorsion ne bat JPEG qu'à bas débit, donc à réserver
  aux niveaux d'ensemble et à l'overzoom.
- **Calcul** : Fast 2DGS ou Instant GaussianImage, de quelques secondes à
  quelques dizaines de secondes par tuile. Une région se traite en heures.

**Piste 4 : des tuiles d'embeddings, l'analyse sur le globe.**
- **L'idée** : servir une pyramide d'embeddings à 64 dimensions (AlphaEarth, ou
  produits par un modèle ouvert comme Prithvi ou TerraMind). Un shader applique à
  la volée une tête linéaire ou une ACP pour en tirer des cartes thématiques, ou
  une similarité cosinus avec un pixel cliqué.
- **Ce que ça rend possible** : **la recherche de « tout ce qui ressemble à
  ça »** sur un globe 3D, en temps réel. Nouveau pour un moteur de globe, et peu
  coûteux.
- **Questions ouvertes** :
  - la réduction de vecteurs unitaires (moyenne puis renormalisation ?) ;
  - la quantification sur 8 bits ;
  - la licence du jeu de données.
- **Calcul** : quasi nul si les embeddings sont fournis.

**Piste 5 : latents en blocs BC pour le viewer natif.**
- **L'idée** : la voie Ubisoft/Intel/DBC. Les latents en BC6H/BC7 sont
  échantillonnés par le matériel (filtré, anisotrope), puis un MLP minuscule en
  f16 tourne dans le fragment shader.
- **Gain** : la VRAM.
- **Accélération** : wgpu 29 expose les matrices coopératives en natif (Metal,
  Vulkan) ; en WebGPU, ALU ordinaire seulement pour l'instant.
- **Calcul** : entraînement par région, de quelques heures à quelques jours.

**Piste 6 : la NTC « on sample » pour les films de ferme.**
- **L'idée** : `tuile-film-gpu` rend hors ligne, sur des GPU NVIDIA
  (`VK_KHR_cooperative_matrix` en Vulkan), avec **plusieurs échantillons par
  pixel**. Le STF y converge sans TAA, donc le bruit qui condamne la NTC dans le
  shader sur un viewer interactif **disparaît**. En échange : bien plus
  d'imagerie résidente en VRAM, donc moins de parents flous dans les plans larges.
- **Pourquoi c'est nouveau** : à ma connaissance, personne n'a appliqué la NTC au
  rendu hors ligne de globes.
- **Déterminisme** : à garantir pour un GPU et un pilote donnés (même pack →
  mêmes octets). Il faut le vérifier, car les ordres d'accumulation matriciels ne
  sont pas spécifiés.
- **Licence** : réimplémentation propre ; le SDK NVIDIA est inutilisable.

**Ordre conseillé.** D'abord **0** (la référence), puis **4** (la plus
spectaculaire, la moins chère), puis **3** et **1**. La **6** est à garder pour
la ferme. La **2** et la **5** sont plus lourdes.

---

## 6. Licences

| Utilisable ou à lire | À éviter |
|---|---|
| Articles NTC, NTBC, Ubisoft, Intel, DBC (CC BY 4.0) : réimplémentation propre | **SDK RTXNTC** (licence NVIDIA RTX SDK, non OSI) |
| Cool-chic (open source Orange, licence à lire), C3 (code public, licence à lire) | Codecs génératifs à poids sous licence restrictive (à vérifier au cas par cas) |
| GaussianImage, Image-GS, Instant GaussianImage (codes publics, licences à lire) | |
| PlayCanvas/SuperSplat (MIT) | |
| KTX-Software / Basis Universal (licences à lire), jxl-rs (à lire) | |
| Embeddings AlphaEarth (licence du jeu à vérifier) | |

Comme dans les autres notes, aucun modèle n'entre dans le dépôt. Les poids, s'il
y en a, sont des artefacts produits hors de Tuile et servis comme une couche.

---

## 7. Trous de la littérature (les problèmes d'un serveur de tuiles)

- **Le codage d'un quadtree entre niveaux de zoom** : aucune publication ne code
  z0–z19 avec des enfants conditionnés par le parent décodé. COMPASS et CAESR ne
  traitent que quelques couches d'une image.
- **Les raccords et le contexte entre tuiles** : une tuile 256² donne une grille
  de latents 16×16 au pas 16. Les modèles d'entropie y voient très peu, et les
  bords de bloc deviennent des raccords sur le globe. Partager le contexte avec la
  voisine casserait le chargement indépendant. Rien n'est publié.
- **Latents par tuile contre par région** : toutes les méthodes entraînent un
  réseau par matériau ou par image. Personne n'a mesuré un décodeur partagé avec
  des latents par tuile contre AVIF, WebP ou KTX2.
- **La cohérence de LOD entre tuiles parent et enfant** : les mips appris *dans*
  une texture ne règlent pas la cohérence entre deux tuiles distinctes.
- **Le déterminisme entre GPU** : seul Intel TSNC se dit déterministe (presse).
  Les réseaux entiers règlent l'entropie, pas la synthèse.
- **La faisabilité WebGPU** : pas de matrices coopératives dans le navigateur, et
  aucun décodeur WGSL d'INR, de Cool-chic ou de C3 trouvé. La voie réaliste
  aujourd'hui est le transcodage au chargement par shader de calcul.
- **Le débit-distorsion sur de vraies orthophotos** : les bancs sont Kodak, CLIC
  et DIV2K. Seul LIG teste du satellite. Les gains propres au domaine
  (TerraCodec, 3 à 10× sur Sentinel-2) suggèrent un gros potentiel non mesuré
  sur les orthos.
- **Pas d'extension glTF ou 3D Tiles pour des textures neuronales** :
  l'interopérabilité exige un repli KTX2 servi à côté.

---

## 8. Expériences proposées

1. **Banc de référence** (1 semaine, sans GPU) : 10 000 tuiles BD ORTHO z14–z19.
   On compare JPEG d'origine, JPEG XL sans perte, AVIF, WebP et KTX2/UASTC. On
   mesure octets par tuile, PSNR et LPIPS, temps de décodage en Rust et en wasm,
   et VRAM. Le banc sert ensuite à toutes les pistes.
2. **Tuiles d'embeddings** (2 semaines) : une région en pyramide d'embeddings,
   avec tête ACP et similarité dans le shader wgpu. Livrable : un film de la
   recherche « clique un champ, montre les champs semblables ».
3. **Gaussiennes 2D d'overzoom** (2 à 3 semaines) : Image-GS ou LIG sur les tuiles
   z_max, rendu au-delà du niveau maximal. On compare au bicubique et à la
   super-résolution (cf. [`imagery-tile-enhancement.md`](imagery-tile-enhancement.md)),
   sur film.
4. **Codec de pyramide conditionnel** (4 à 6 semaines, 1 à 4 GPU) : hyperprior et
   conditionnement par le parent, décodeur partagé par niveau, transcodage au
   chargement en WGSL. Mesures :
   - débit-distorsion contre le banc 1 ;
   - raccords et cohérence parent/enfant ;
   - identité au bit près entre CPU et WebGPU.

---

## 9. Relation avec les autres notes

- [`imagery-tile-enhancement.md`](imagery-tile-enhancement.md) : même contrainte
  de cohérence parent/enfant (overzoom par super-résolution, de type CEM). Les
  gaussiennes 2D (piste 3) en sont l'alternative non générative.
- [`3d-from-imagery-tiles.md`](3d-from-imagery-tiles.md) : les splats 3D et les
  gaussiennes 2D partagent un rendu. Les embeddings (piste 4) sont aussi une
  entrée naturelle pour la segmentation de la scène 3D.
- [`docs/18-color-harmonization.md`](../18-color-harmonization.md) : le champ de
  correction s'applique au décodé. Un codec neuronal doit se placer *avant* la
  radiométrie dans la chaîne, sans la remplacer.
