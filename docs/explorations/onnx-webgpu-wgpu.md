<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# ONNX sur WebGPU et sur wgpu — étude

Note d'exploration, rédigée le 2026-10-14. **Rien n'est implémenté.** Elle
répond à une question : *si Tuile doit exécuter un réseau de neurones exporté
en ONNX, par quel chemin le faire* ? Trois cas :

- en natif, sur le même device wgpu que le rendu ;
- dans le navigateur (wasm32 + WebGPU) ;
- en lot hors ligne sur des serveurs GPU.

Elle complète [`webgpu-neural-networks.md`](webgpu-neural-networks.md), qui
couvre l'écosystème général : frameworks, types de réseaux, essais publiés,
contraintes des plateformes.

**Méthode et limites.** Une trentaine de recherches web. Les versions des crates
Burn et CubeCL ont été lues directement sur l'API de crates.io, le 2026-10-14.
onnxruntime.ai et docs.rs ne résolvaient pas. Plusieurs dates de version d'ONNX
Runtime viennent de miroirs (newreleases.io) aux dates relatives : elles sont
marquées *(non vérifié)*, comme tout ce qui n'a pas été recoupé.

---

## 0. En une page

- **ONNX est un format d'échange, pas un moteur.** Ce qui compte :
  - l'opset que le runtime cible couvre ;
  - les opérateurs *contrib* `com.microsoft` (MatMulNBits, GroupQueryAttention…)
    qui **enferment le modèle dans ONNX Runtime** ;
  - les formes statiques ou dynamiques ;
  - le format de quantification ;
  - la limite de 2 Go de protobuf, contournée par les données externes.
- **ONNX Runtime Web (WebGPU) est le chemin le plus complet dans le
  navigateur**, mais il est **en pleine migration** : l'ancienne couche
  JavaScript (JSEP) a été déclarée obsolète dans 1.29, au profit du fournisseur
  WebGPU C++ compilé en wasm. Les sorties peuvent rester sur le GPU
  (`gpu-buffer`). La capture de graphe exige des formes statiques. **Mais il ne
  peut pas partager le device de Tuile** : l'API publique de wgpu 29 n'expose ni
  n'adopte de `GPUDevice` JS (vérifié dans la source, §2.1).
- **ONNX Runtime natif « WebGPU » tourne sur Dawn, pas sur wgpu.** Il ne pourra
  **jamais partager le device wgpu** de Tuile.
- **burn-onnx est le seul chemin vraiment zéro copie en Rust.** Il **génère du
  code Rust** à partir du modèle ONNX (160 des 201 opérateurs canoniques). Ce
  code tourne sur burn-wgpu/CubeCL, en natif comme dans le navigateur. Il
  **accepte un device wgpu existant** (`WgpuDevice::Existing`, `init_device`).
  **Condition : la même version majeure de wgpu**, vérifiée sur crates.io :

  | Version de Burn | CubeCL | wgpu |
  |---|---|---|
  | 0.21 | 0.10 | **29** (celle de Tuile) |
  | 0.22 | 0.11 | **30** |

  Le partage marche aujourd'hui avec Burn 0.21. Ensuite, Tuile et Burn doivent
  avancer au même rythme.
- **`ort` (pyke) reste en 2.0-rc** depuis environ 2,5 ans (rc.14, 12 octobre
  2026). Le support wasm a été sorti dans un crate séparé, `ort-web`, qui lie
  probablement onnxruntime-web en JS. Utile en lot hors ligne (CUDA, TensorRT),
  peu adapté au chemin wasm de Tuile (règle 8).
- **wonnx (ONNX → WGSL sur wgpu) est archivé depuis mai 2025** : inspiration
  seulement. **tract** et **RTen** sont des runtimes CPU mûrs, compilables en
  wasm : de bonnes références et des replis CPU.
- **ONNX → WGSL à l'avance** (tinygrad, TVM), embarqué dans Tuile : viable pour
  de petits modèles à formes figées (super-résolution, segmentation, normales de
  terrain). Ce n'est un produit supporté nulle part.

---

## 1. Le format ONNX : ce qui compte sur GPU

- **Opsets.** ONNX 1.20 a modifié Cast, Reshape, Transpose, Pad,
  Quantize/DequantizeLinear et ajouté des types 2 bits. ONNX 1.21 est revenu sur
  l'opset 25 et passe à 26. Chaque runtime ne couvre qu'une fenêtre d'opsets :
  **figer l'opset à l'export**. Le livre de Burn recommande ≥ 16 ; PyTorch 2.9
  conseille 18 pour les runtimes anciens. —
  https://newreleases.io/project/pypi/onnx/release/1.20.0 ·
  https://newreleases.io/project/pypi/onnx/release/1.21.0
- **Opérateurs contrib `com.microsoft`.** MatMulNBits, GroupQueryAttention et
  les attentions fusionnées sont propres à ONNX Runtime. Un modèle passé par
  l'optimiseur transformer d'ORT ou par onnxruntime-genai **ne se chargera pas**
  dans burn, tract, candle ou wonnx. **Garder un export « `ai.onnx` pur »** pour
  les runtimes Rust. —
  https://instagit.com/microsoft/onnxruntime/what-quantization-formats-does-onnx-runtime-support-for-inference-optimization
- **Données externes.** Au-delà de 2 Go (limite protobuf), les tenseurs vont
  dans des fichiers à part (emplacement, offset, longueur). ORT-Web les reçoit
  via `externalData`. C'est cohérent avec un *fetcher* de poids, comme Tuile en a
  pour les tuiles. — https://onnx.ai/onnx/repo-docs/ExternalData.html ·
  https://onnxruntime.ai/docs/tutorials/web/large-models.html
- **Formes dynamiques ou statiques.** `dim_param` est permis, mais la capture de
  graphe d'ORT-Web et les exports WGSL à l'avance exigent des formes statiques.
  **Pour Tuile, figer la taille du patch** (256² ou 512²) : c'est naturel avec des
  tuiles. — https://onnxruntime.ai/docs/tutorials/web/ep-webgpu.html
- **Quantification QDQ contre QOperator.** QDQ garde des opérateurs flottants
  entourés de paires Quantize/Dequantize : c'est le format portable. QOperator
  substitue des noyaux entiers. En 4 bits, le quantificateur d'ORT émet DQ +
  MatMul, que la session fusionne en MatMulNBits. Attention au point zéro par
  défaut : 8 en MatMulNBits, 0 en DQ. —
  https://instagit.com/microsoft/onnxruntime/what-quantization-formats-does-onnx-runtime-support-for-inference-optimization
- **Conversion fp16** (onnxconverter-common, `keep_io_types`, liste
  d'exclusion ; *non vérifié*). Pièges :
  - l'export fp16 peut empêcher le repliement de constantes, laissant des îlots
    CPU (ConstantOfShape) ;
  - débordements, par exemple dans une branche FFT de LaMa ;
  - écart fp16/fp32 rapporté d'environ 2e-3 (BiRefNet).

  — https://huggingface.co/runes/birefnet-lite-webgpu
- **Optimisation hors ligne.** ORT peut sauvegarder son graphe optimisé
  (`optimized_model_filepath`) ; le format ORT vise les builds minimaux. onnxsim
  fait le repliement de constantes. Immich enchaîne les deux mais saute onnxsim
  sur les gros modèles. —
  https://onnxruntime.ai/docs/performance/model-optimizations/ort-format-model-runtime-optimization.html
  ·
  https://demo.gitea.com/ashipov/immich/src/commit/506f9f6fb970bf2954a516d63c8b3637710dba56/machine-learning/export/models/optimize.py
- **Export depuis PyTorch.** PyTorch 2.9 passe `torch.onnx.export` en
  `dynamo=True` par défaut, sur `torch.export`. TorchScript est en fin de vie et
  l'opset par défaut est 20. **Revalider les opérateurs personnalisés et les axes
  dynamiques.** — https://newreleases.io/project/pypi/torch/release/2.9.0 ·
  https://github.com/pytorch-labs/torch-release-notes/pull/17 ·
  https://docs.pytorch.org/docs/stable/onnx

---

## 2. ONNX Runtime sur WebGPU

### 2.1 Dans le navigateur (onnxruntime-web)

- **Historique.** Le fournisseur WebGPU est lancé dans ORT 1.17 (février 2024),
  via JSEP, une couche de noyaux en JavaScript. —
  https://opensource.microsoft.com/blog/2024/02/29/onnx-runtime-web-unleashes-generative-ai-in-the-browser-using-webgpu/
- **Migration de JSEP vers le fournisseur WebGPU C++ compilé en wasm (1.27 →
  1.29, 2026).** `import 'onnxruntime-web/webgpu'` utilise désormais le
  fournisseur C++. 1.27 livrait encore les deux et refusait de les activer
  ensemble. **Les notes de 1.29 annoncent l'obsolescence de WebGL et de JSEP**
  (la version qui les retire n'a pas été trouvée). **Conséquence : le
  comportement des noyaux peut changer entre versions mineures.** —
  https://newreleases.io/project/github/microsoft/onnxruntime/release/v1.29.0 ·
  https://cdn.jsdelivr.net/npm/onnxruntime-web@1.27.0/lib/index.ts
- **Sorties sur le GPU.** `preferredOutputLocation: 'gpu-buffer'`, global ou par
  sortie. Les sorties non pré-allouées se libèrent par `dispose()` : risque de
  fuite. Des tenseurs d'entrée peuvent être construits à partir de GPUBuffer
  existants (nom d'API *non vérifié*). —
  https://onnxruntime.ai/docs/tutorials/web/ep-webgpu.html
- **Capture de graphe** (`enableGraphCapture`). Exige des formes statiques et
  tous les nœuds sur WebGPU. C'est **le principal levier contre le coût par
  dispatch** (voir l'étude générale).
- **Partage du GPUDevice : impossible avec wgpu 29.** ORT-Web accepte un
  `GPUDevice` fourni (`env.webgpu.device`, avant la première session) et des
  entrées `Tensor.fromGpuBuffer` (f32, **f16**, entiers). Mais
  `Tensor.fromTexture` n'accepte qu'une `WebGLTexture`, et **aucune GPUTexture
  en entrée**.

  Côté Tuile, j'ai lu la source de wgpu 29.0.3 :
  - `mod backend;` est privé (`src/lib.rs:114`) ;
  - le `GPUDevice` du navigateur est dans `WebDevice.inner`, en `pub(crate)`
    (`src/backend/webgpu.rs:1202`) ;
  - seul `backend::custom` est public.

  **Avec l'API publique, wgpu ne peut ni exposer son `GPUDevice` JS ni en
  adopter un.** Pas de zéro copie entre le device de Tuile et ORT-Web, TF.js ou
  LiteRT.js, à moins de patcher wgpu ou d'écrire un backend `custom` complet. —
  https://onnxruntime.ai/docs/tutorials/web/ep-webgpu.html ·
  https://vgpu.labs.vercel.dev/docs/ml/browser
- **fp16.** Noyaux fp16, avec `shader-f16`. Le typage strict de WGSL a causé des
  échecs de compilation (correctif QuickGelu : littéraux f32 → f16).
- **Taille du paquet.** Des dizaines de Mo pour le binaire wasm WebGPU *(non
  vérifié)*. Un build à jeu d'opérateurs réduit est documenté. —
  https://ONNXRuntime.ai/docs/build/web.html

### 2.2 En natif (fournisseur WebGPU C++ sur Dawn)

- **ORT 1.25 → 1.31.** Le fournisseur tourne sur Dawn (D3D12, Vulkan, Metal) et
  est livré en **plugin** : Python `onnxruntime-ep-webgpu`, .NET
  `Microsoft.ML.OnnxRuntime.EP.WebGpu`. 1.30 ajoute Linux AArch64,
  PagedAttention et le cache KV en INT8. —
  https://onnxruntime.ai/docs/execution-providers/WebGPU-ExecutionProvider.html
  · https://newreleases.io/project/github/microsoft/onnxruntime/release/v1.30.0 ·
  https://newreleases.io/project/github/microsoft/onnxruntime/release/v1.31.0
- **Dawn n'est pas wgpu.** Impossible de lui passer le `Device` wgpu 29 de Tuile.
  Les données traversent le CPU, ou une interopérabilité de mémoire externe
  propre à chaque plateforme.
- **onnxruntime-genai + WebGPU.** Vise les LLM, peu pertinent pour Tuile. —
  https://www.mintlify.com/microsoft/onnxruntime-genai/acceleration/webgpu

---

## 3. Les chemins Rust

| Crate | Version (date) | Exécution | wasm | Partage du device wgpu | Maturité | Licence |
|---|---|---|---|---|---|---|
| **burn-onnx** (ex-burn-import) + onnx-ir | 0.21.0 (14 mai 2026) | génère du Rust → n'importe quel backend Burn, dont **burn-wgpu/CubeCL** | **oui** (WebGPU) | **oui** (`WgpuDevice::Existing`, `init_device`) si même wgpu majeur | 160/201 opérateurs ; 472/472 vérifications opset 1–24 ; **0.x** | MIT/Apache-2.0 |
| **ort** (pyke) | 2.0.0-rc.14 (12 oct. 2026), embarque ORT 1.31 | ORT natif (CUDA, TensorRT, DirectML, WebGPU sur Dawn…) ; backends `ort-tract`, `ort-candle` | wasm retiré depuis rc.5 ; **`ort-web`** séparé (WebGPU, WebGL, WebNN) | non (Dawn, ou ORT-Web en JS) | **release candidate** depuis environ 2,5 ans | MIT/Apache *(non vérifié)* ; ORT MIT |
| **tract-onnx** (Sonos) | 0.23.8 (21 sept. 2026) | CPU | **oui** | — | **mûr** | MIT/Apache *(non vérifié)* |
| **RTen** | 0.26.0 | CPU | **oui** | — | actif ; charge `.onnx` depuis 0.23 | MIT/Apache *(non vérifié)* |
| **candle-onnx** (Hugging Face) | 0.11.0 (juin 2026) | CPU, CUDA, Metal ; **pas de backend wgpu** (fonctionnalités de candle-core 0.11 : accelerate, cuda, cudnn, metal, mkl, nccl, ug) | partiel | — | `protoc` au build ; liste d'opérateurs non publiée | MIT/Apache |
| **wonnx** | **archivé (7 mai 2025)** | WGSL sur wgpu | oui | oui (wgpu) | pas d'int64 ; MatMul/Gemm en dimensions paires ; pas de RNN/LSTM/Loop/Scan | MIT/Apache |

Liens :
- burn-onnx : https://burn.dev/blog/release-burn-onnx-0.21.0/ ·
  https://burn.dev/books/burn/import/onnx-model.html ·
  https://burn.dev/docs/burn/backend/wgpu/enum.WgpuDevice.html ·
  https://docs.rs/cubecl-wgpu/latest/
- ort : https://github.com/pykeio/ort/releases ·
  https://newreleases.io/project/github/pykeio/ort/release/v2.0.0-rc.13 ·
  https://docs.rs/ort-web
- tract : https://docs.rs/crate/tract-onnx/latest ·
  https://fosdem.org/2026/events/attachments/YJJQTD-tract-and-torch-to-nnef/slides/266716/tract-and_l9xkrho.pdf
- RTen : https://docs.rs/crate/rten · https://lib.rs/crates/rten
- candle-onnx : https://docs.rs/candle-onnx/
- wonnx : https://github.com/webonnx/wonnx

**Versions de wgpu, vérifiées sur crates.io le 2026-10-14 :**
- `cubecl-wgpu` 0.10.0 dépend de `wgpu ^29` ;
- `cubecl-wgpu` 0.11.0 dépend de `wgpu ^30` ;
- `burn-wgpu` 0.22.0 (6 octobre 2026) dépend de `cubecl ^0.11`.

Burn 0.21 partage donc un device avec Tuile aujourd'hui. Burn 0.22 demande de
passer Tuile en wgpu 30.

**Règles du dépôt.** L'inférence vit dans son propre crate, derrière un trait,
jamais dans `tuile-core` (règle 1). Dans le chemin wasm, burn et ort-web sont en
0.x ou en RC : il faut **une justification écrite dans la PR** (règle 8).

---

## 4. Compilateurs qui consomment ONNX et émettent du WGSL

- **Apache TVM (Relax)** — v0.19 à v0.23 (2025–26). Le frontend ONNX → Relax est
  maintenu. La génération WGSL date de 2020 et alimente MLC/WebLLM. La sortie
  exige le runtime web de TVM (JS + wasm), qui demande 1 Go de binding de
  stockage et retombe à 128 Mo. Apache-2.0. —
  https://newreleases.io/project/github/apache/tvm/release/v0.23.0 ·
  https://tvm.apache.org/2020/05/14/compiling-machine-learning-to-webassembly-and-webgpu
  · https://apache.googlesource.com/tvm/+/HEAD/web/src/webgpu.ts
- **IREE** — import ONNX documenté (`iree-import-onnx`, via torch-mlir). Le **HAL
  WebGPU est expérimental**. Un commit récent ajoute pilote, cible WGSL et suite
  de conformité, mais il vise l'API WebGPU du navigateur ou de Node, pas wgpu. —
  https://iree.dev/guides/ml-frameworks/onnx/ ·
  https://iree.dev/guides/deployment-configurations/ ·
  https://opensecura.googlesource.com/3p/openxla/iree/+/d9a3dd15a552cdded3bda4fcfa65f1341d2b5f92
- **tinygrad** — un exécuteur ONNX et un exportateur WebGPU qui émet des noyaux
  WGSL à formes fixes, plus un harnais JS. Exemple : GPT-2 124M en WGSL, sortie
  identique au natif token par token. MIT. —
  https://huggingface.co/jik-wtf/tinygpt-webgpu

**ONNX → WGSL à l'avance, rejoué par Tuile.** C'est techniquement viable pour de
**petits modèles à formes statiques**. Un exportateur (tinygrad ou TVM) émet
une liste de noyaux WGSL avec leurs tailles de dispatch et un plan de buffers.
Un petit répartiteur Rust les rejoue sur *le device wgpu de Tuile*.

- **On gagne** : le zéro copie, aucune dépendance d'exécution, la parité
  native/wasm.
- **On paie** :
  - pas de formes dynamiques, donc un export par forme ;
  - le plan de buffers et les limites à gérer soi-même ;
  - naga doit accepter le WGSL produit (sous-groupes et f16 compris) ;
  - le mouvement des chaînes d'outils.

burn-onnx atteint presque le même résultat, en Rust et de façon moins fragile.

---

## 5. Pièges rapportés

- **Opérateurs repliés sur le CPU, et les transferts qui suivent.** ORT place
  délibérément les opérateurs de forme sur le CPU. Un noyau manquant coupe le
  graphe et force des copies GPU ↔ CPU. C'est pire en int8, où certains
  DequantizeLinear tournent sur le CPU. **Vérifier le placement des nœuds**
  (journal verbeux). — https://bugzilla.mozilla.org/show_bug.cgi?id=1939943
- **Précision fp16.** Gemma 3 270M est inutilisable en WebGPU fp16 alors que
  fp32 et wasm vont bien (débordement suspecté). **Toujours comparer fp16 à fp32
  sur la même entrée, en CI.** — https://github.com/microsoft/onnxruntime/issues/26732
- **Latence du premier lancement.** Compilation des shaders et téléversement
  des poids : environ 300 ms au premier passage contre 100 à 120 ms ensuite.
  **Chauffer au chargement, pas sur la première frame visible** (règle « jamais
  de noir »). — https://huggingface.co/LunarOilRig/comic-text-onnx-fp16
- **`maxStorageBufferBindingSize`.** 128 Mio par défaut : demander les limites
  de l'adaptateur. ORT découpe les grosses entrées (#25962). Les Concat/Split
  larges dépassent `maxStorageBuffersPerShaderStage` (minimum 8) ; l'export
  RMBG-2.0 les a mis en arbre. — https://developer.mozilla.org/en-US/docs/Web/API/GPUSupportedLimits
  · https://huggingface.co/yamura4/RMBG-2.0-WebGPU
- **Gros modèles dans le navigateur.** wasm32 plafonne à 4 Go, et ORT-Web ne
  charge pas de modèle de plus de 4 Go. Chrome limite un ArrayBuffer à environ
  2 Go. Un chargeur de données externes permet de téléverser les poids
  directement sur le GPU. — https://onnxruntime.ai/docs/tutorials/web/large-models.html
- **Différences entre navigateurs et appareils.** Les limites varient (Linux
  notamment). Le WebGPU de Firefox produit encore des rapports de bogue propres
  à ORT. — https://web3dsurvey.com/webgpu/limits/maxStorageBufferBindingSize/platform/Linux

---

## 6. Recommandation

| Cible | Choix | Repli |
|---|---|---|
| **Natif, sur le device wgpu de Tuile** | **burn-onnx** → code Rust sur burn-wgpu, initialisé sur *le* device de Tuile. Seul chemin zéro copie. Burn 0.21 tant que Tuile est en wgpu 29. | `ort` (CUDA, ou WebGPU sur Dawn) sur un device séparé, avec relecture CPU : acceptable hors boucle de rendu. tract pour le CPU. |
| **Navigateur** | **burn-wgpu sur WebGPU** : le même code qu'en natif, dans le chemin Rust/wasm, et **le seul à partager le device de Tuile** (cf. §2.1). Si un opérateur manque : onnxruntime-web via `ort-web`, **sur un device séparé avec copies**, hors chemin par frame. Runtime auto-hébergé et télémétrie coupée : par défaut, `ort-web` télécharge le runtime sur cdn.pyke.io et envoie de la télémétrie. | tract ou RTen en wasm sur CPU pour de petits modèles. |
| **Lot hors ligne (ferme, serveurs L4)** | **`ort` + CUDA ou TensorRT** (ou Python ORT). WebGPU n'apporte rien sur ces machines. | plugin WebGPU natif d'ORT pour du matériel non NVIDIA. |

**Hygiène d'export, pour tous les cas :**
- opset figé entre 16 et le maximum du runtime cible ;
- **`ai.onnx` pur**, sans contrib ;
- formes statiques à la taille de tuile ;
- fp32 de référence gardé à côté de toute variante fp16 ou quantifiée, avec un
  test d'écart en CI ;
- poids en données externes, servis comme des tuiles (cache, URL immuables).

**Risques principaux :**
1. **Le pas de version wgpu entre Burn/CubeCL et Tuile.** C'est le risque n° 1
   du partage de device.
2. **ORT natif repose sur Dawn**, donc ne partagera jamais le device wgpu.
3. **ORT-Web est en migration** (JSEP obsolète) et **`ort` 2.0 est encore en
   RC**.
4. **Les opérateurs contrib** enferment le modèle dans ORT.
5. **Le fp16 et les limites WebGPU** : 128 Mio par binding, 8 buffers de
   stockage par étage, 4 Go de mémoire wasm.

---

## 7. Premiers essais proposés

1. **Burn 0.21 sur le device de Tuile** (1 à 2 jours). Un petit modèle ONNX
   (super-résolution ×2 ou segmentation légère), via burn-onnx, sur le
   `wgpu::Device` du viewer. La sortie, une texture, est consommée par une passe
   de rendu sans relecture. Mesures : temps par tuile, parité natif/wasm, écart
   fp16/fp32.
2. **Coût du chemin ORT-Web séparé en wasm** (2 à 3 jours). Le partage de device
   étant impossible avec l'API publique de wgpu 29 (§2.1), on mesure
   l'aller-retour par tuile : lecture de la texture wgpu, copie vers ORT-Web,
   inférence, retour en texture. On en tire le seuil au-delà duquel ce chemin
   reste acceptable hors de la boucle de rendu.
3. **ONNX → WGSL à l'avance** (1 semaine). Export tinygrad d'un modèle à forme
   fixe, rejoué par un répartiteur Rust. Mesures : validation par naga, parité
   avec burn-onnx, nombre de dispatchs.
