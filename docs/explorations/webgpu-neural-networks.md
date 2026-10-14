<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Réseaux de neurones sur WebGPU et wgpu — étude

Note d'exploration, rédigée le 2026-10-14. **Rien n'est implémenté.** Elle fait
le point sur l'exécution de réseaux de neurones par WebGPU, dans le navigateur
et en natif via wgpu ou Dawn :

- les frameworks et runtimes ;
- les types de réseaux qui tournent réellement, avec les essais et bancs publiés ;
- les contraintes de bas niveau : limites, noyaux, numérique, plateformes ;
- ce qu'il faut en retenir pour un moteur Rust/wgpu comme Tuile.

Le cas ONNX est traité à part, en détail, dans
[`onnx-webgpu-wgpu.md`](onnx-webgpu-wgpu.md). Les usages envisagés pour Tuile
sont dans [`neural-texture-representations.md`](neural-texture-representations.md)
et [`imagery-tile-enhancement.md`](imagery-tile-enhancement.md).

**Méthode et limites.** Environ 120 recherches web sur quatre axes, plus :
- versions et dates lues sur les API de crates.io et npm le 2026-10-14 ;
- vérification directe dans les sources publiées de `wgpu` 29.0.3,
  `cubecl-wgpu` 0.10.0 et 0.11.0, et `onnxruntime-common` 1.30.0 (lecture du
  code, rien n'a été compilé) ;
- documentation de wgpu 29 via Context7.

Arxiv, Hugging Face et developer.chrome.com ne résolvaient pas. Les chiffres de
performance viennent des résumés, des pages projet et de blogs ; *(non vérifié)*
marque ce qui n'a pas été recoupé. **Aucun essai n'a été lancé ici** : la section
« essais » rapporte ceux des autres, et le §8 propose les nôtres.

---

## 0. En une page

- **WebGPU est devenu une vraie cible d'inférence** :
  - un LLM 8B en 4 bits décode à 41 tokens/s dans Chrome sur M3 Max, soit **71 à
    80 % du natif** (WebLLM) ;
  - un backend WebGPU de llama.cpp (2026) décode 45 à 69 % plus vite que les
    frameworks existants ;
  - des noyaux WebGPU ouverts (Hugging Face, 2026) sont 2,57× plus rapides
    qu'ONNX Runtime WebGPU en moyenne.
- **Le goulot n'est pas le calcul mais le dispatch.** Chaque dispatch WebGPU
  coûte 24 à 71 µs, plus environ 13 µs par soumission. Un réseau de 150 à 300
  opérateurs non fusionnés coûte plusieurs millisecondes avant le moindre calcul,
  soit une grosse part d'une frame de 16 ms. **Fusionner les opérateurs est la
  règle n° 1.**
- **Le support des navigateurs est large, sauf sous Linux :**
  - Chrome, Edge et Safari 26 (macOS, iOS) ;
  - Firefox sous Windows (141) et macOS (145–147) ;
  - environ trois quarts des utilisateurs web ;
  - **Linux : 9 à 16 %**, et Firefox Linux n'a pas encore livré.

  Le repli sans réseau de neurones doit donc être le chemin *normal*, pas
  l'exception.
- **Les accélérateurs du navigateur :**
  - `shader-f16` : 1,3 à 3× ;
  - sous-groupes (Chrome 134) : 2,3 à 2,9× sur du matrice-vecteur ;
  - produit scalaire int8 (`dot4I8Packed`).

  Les **matrices coopératives n'existent pas sur le web** : l'extension
  expérimentale de Chromium est disponible sur 0 % des machines. Dans wgpu 29,
  elles sont **natives seulement** (Metal, Vulkan).
- **Les frameworks JavaScript :**
  - **ONNX Runtime Web** (1.30) est le plus complet, en migration vers son
    fournisseur WebGPU C++ ;
  - **Transformers.js v4** repose dessus ;
  - **LiteRT.js** (Google, juillet 2026) remplace TF.js, qui est **en sommeil**,
    et MediaPipe LLM, **en maintenance seule** ;
  - **WebLLM** (TVM) sert aux LLM ;
  - **WebNN** est en essai d'origine (Chrome 147–149) : à surveiller, pas encore
    à utiliser.
- **Pour un moteur Rust/wgpu, un fait décisif :** l'API publique de wgpu 29
  **n'expose ni n'adopte de `GPUDevice` JS** (vérifié dans la source). Aucun
  runtime JS (ORT-Web, LiteRT.js, TF.js) ne peut partager le device de Tuile
  sans copies. **Le seul chemin même-device est natif à wgpu** : Burn/CubeCL, ou
  du WGSL écrit à la main.
- **Burn/CubeCL** accepte un device wgpu existant (`init_device(WgpuSetup)`,
  `WgpuDevice::Existing`). Les tenseurs sont de simples `wgpu::Buffer`. **Burn
  0.21 / CubeCL 0.10 sont en wgpu 29, comme Tuile** ; Burn 0.22 / CubeCL 0.11
  (6 octobre 2026) sont passés en wgpu 30. Les deux sont **0.x** : la règle 8
  exige une justification écrite pour le chemin wasm.
- **Ce qui est réaliste côté client en 2026, par taille de réseau :**

  | Paramètres | Usage réaliste |
  |---|---|
  | moins de 10 M | **à chaque frame**, dans la boucle de rendu, si fusionné |
  | 10 à 100 M | interactif, mais pas à chaque frame en pleine résolution |
  | 100 M à 1 G | à la demande, en secondes |
  | plus de 1 G | GPU dédié ou Apple Silicon seulement |

  Pour Tuile, la cible naturelle est **un petit réseau, de quelques dizaines de
  milliers à quelques millions de paramètres, sur le device du rendu**, dont la
  sortie reste sur le GPU. C'est le modèle de MobileNeRF, Anime4K et NTC, pas
  celui des LLM.

---

## 1. Plateformes et support (octobre 2026)

| Plateforme | Statut WebGPU |
|---|---|
| **Chrome/Edge** | Mac, Windows x64, ChromeOS depuis 113 ; Android 12+ depuis 121 (GPU Imagination depuis 139 sur Android 16+) ; **Linux : Intel Gen12+ depuis 144, NVIDIA (pilote 535.183.01+, Wayland) depuis 147**, le reste derrière drapeaux ; Windows ARM64 derrière drapeau |
| **Firefox** (implémenté sur **wgpu**) | Windows depuis 141 ; macOS 26+ depuis 145, tous macOS depuis 147 ; **Linux en Nightly seulement**, stable attendu en 2026 ; Android derrière drapeau |
| **Safari 26** | macOS, iOS, iPadOS, visionOS, activé par défaut (iPhone 11 ou ultérieur, non vérifié) |
| **Mode compatibilité** (Chrome 146) | sous-ensemble de WebGPU sur OpenGL ES 3.1 / D3D11, d'abord Android, limites réduites |
| **Node** | paquet npm `webgpu` (Dawn), en cours : validation toujours complète, donc lent. Pour la CI, pas pour les bancs |

**Adoption** (web3dsurvey, deux chiffres non datés) : 74 à 81 % des
utilisateurs web, avec Linux entre 9 et 16 %.

Sources :
- https://github.com/gpuweb/gpuweb/wiki/Implementation-Status (2 octobre 2026,
  la source la plus fiable)
- https://simonwillison.net/2025/Jul/16/webgpu-firefox
- https://discuss.privacyguides.net/t/news-from-wwdc25-webkit-in-safari-26-beta/28226
- https://app.cinevva.com/news/2026-03-10-chrome-146-webgpu-compatibility
- https://www.khronos.org/developers/linkto/webgl-and-webgpu-sigg26
- https://web3dsurvey.com/ · https://web3dsurvey.com/webgpu
- https://dawn.googlesource.com/dawn/+show/456e40bec8478da182e09b4a1a89d5fcf1b14cbb/src/dawn/node/README.md

---

## 2. Frameworks et runtimes

### 2.1 JavaScript, dans le navigateur

| Runtime | Version (date) | Ce qu'il fait sur WebGPU | État | Licence |
|---|---|---|---|---|
| **ONNX Runtime Web** (Microsoft) | 1.30.0 (14 sept. 2026) | fournisseur WebGPU ; `fromGpuBuffer` (f32/**f16**/entiers), sorties `gpu-buffer`, capture de graphe, `env.webgpu.device` (device fourni) ; `fromTexture` = **WebGL seulement** | **le plus mûr** ; JSEP et WebGL obsolètes depuis 1.29 | MIT |
| **Transformers.js v4** (Hugging Face) | 4.3.1 (6 oct. 2026) | pipelines et zoo de modèles ; runtime WebGPU C++ co-écrit avec ORT ; Safari 26+ depuis 4.3 | actif | Apache-2.0 |
| **WebLLM / MLC** | 0.2.86 (11 oct. 2026) | LLM compilés à l'avance par TVM en WGSL ; API de type OpenAI | actif, 0.x | Apache-2.0 |
| **LiteRT.js** (Google) | 2.5.3 (17 juil. 2026) | runtime LiteRT en wasm ; GPU via **ML Drift** sur WebGPU, CPU via XNNPACK, NPU via WebNN (expérimental) ; délégation tout ou rien par modèle | nouveau ; successeur annoncé de TF.js pour `.tflite` | Apache-2.0 |
| **TensorFlow.js WebGPU** | 4.22.0 (oct. 2024) | `zeroCopy` sur GPUBuffer, `dataToGPU()` | **en sommeil** | Apache-2.0 |
| **MediaPipe Tasks** | vision 1.1.0 ; genai 0.10.29 | tâches clés en main ; LLM Inference exige WebGPU | LLM Inference **en maintenance seule** | Apache-2.0 |
| **WebNN** (W3C) | Candidate Recommendation 22 janv. 2026 | API d'inférence native (CPU, GPU, NPU) ; interop WebGPU (`MLTensor` ↔ GPUBuffer) en conception | **essai d'origine Chrome 147–149** ; Edge suit ; Firefox prévoit | — |

Sources :
- ORT : https://onnxruntime.ai/docs/tutorials/web/ep-webgpu.html ·
  https://newreleases.io/project/github/microsoft/onnxruntime/release/v1.29.0
- Transformers.js : https://github.com/huggingface/transformers.js/releases ·
  https://newreleases.io/project/github/huggingface/transformers.js/release/4.3.0
- WebLLM : https://arxiv.org/html/2412.15803v1 ·
  https://blog.mlc.ai/2024/06/13/webllm-a-high-performance-in-browser-llm-inference-engine
- LiteRT.js : https://developers.googleblog.com/en/litertjs-googles-high-performance-web-ai-inference/
  · https://betterstack.com/community/guides/ai/litert-js-browser-ai/
- TF.js : https://js.tensorflow.org/api/latest ·
  https://mcp.depscope.dev/pkg/npm/@tensorflow/tfjs-backend-webgpu
- MediaPipe : https://developers.google.com/edge/mediapipe/solutions/genai/llm_inference/web_js
- WebNN : https://groups.google.com/a/chromium.org/g/blink-dev/c/5CWKSChYo98 ·
  https://cadeproject.org/updates/updated-candidate-recommendation-web-neural-network-webnn-api-published-by-the-world-wide-web-consortium/
  · https://github.com/webmachinelearning/webnn/issues/482

### 2.2 Rust (le plus important pour Tuile)

| Crate | Version (date) | Ce qu'il fait | Device wgpu partagé ? | wasm | Licence | Verdict |
|---|---|---|---|---|---|---|
| **Burn** (Tracel) | **0.22.0 (6 oct. 2026)** ; 0.21.0 (7 mai 2026) ; 0.20.0 (janv. 2026, noyaux CubeK, refonte de l'import ONNX) | framework complet ; backend **burn-wgpu** sur CubeCL ; import ONNX en code Rust (burn-onnx) | **oui** : `init_device(WgpuSetup{instance, adapter, device, queue})` → `WgpuDevice::Existing`, prévu pour partager avec Bevy ou egui | **oui** | MIT/Apache-2.0 | **à prototyper** ; 0.x, ruptures à chaque mineure |
| **CubeCL** (Tracel) | 0.11.0 (6 oct. 2026) ; 0.10.0 (7 mai 2026) | fonctions `#[cube]` en Rust → WGSL / SPIR-V / MSL / CUDA / HIP / CPU, à la volée, avec autotuning ; pas de tensor cores sur WebGPU (doc 0.10) ; stockage = `wgpu::Buffer` + offset | oui (via Burn ou directement) | oui | MIT/Apache-2.0 | noyaux maison sur le device de Tuile |
| **web-rwkv** | 0.10.20 (31 mars 2026) | RWKV seulement, WebGPU pur, wasm ; **wgpu ^29** | non vérifié | oui | MIT/Apache-2.0 | exemple de WGSL d'inférence écrit à la main sur *notre* version de wgpu |
| **tract** (Sonos) | tract-onnx 0.23.8 (21 sept. 2026) | ONNX/NNEF sur CPU, Rust pur | — | oui *(non vérifié)* | MIT/Apache-2.0 | repli CPU sûr |
| **ort / ort-web** (pyke) | ort 2.0.0-rc.14 ; ort-web 0.4.0+1.30 (12 oct. 2026) | ORT natif (fonctionnalité `webgpu` = fournisseur Dawn, non vérifié) ; `ort-web` = onnxruntime-web derrière l'API `ort`, dans un contexte wasm séparé | non | via ort-web, avec copies | MIT/Apache-2.0 | serveur et lot hors ligne ; `ort-web` **télécharge le runtime sur cdn.pyke.io et envoie de la télémétrie par défaut** (désactivable) |
| **Candle** (Hugging Face) | candle-core 0.11.0 (juin 2026) | CPU, CUDA, Metal… **pas de backend wgpu ni WebGPU** | — | CPU seulement | MIT/Apache-2.0 | hors sujet pour le GPU |
| **wonnx** | 0.5.1 (sept. 2023), **archivé** | ONNX sur wgpu, ancien wgpu | — | — | MIT/Apache-2.0 | référence de conception seulement |
| **Ratchet** (Hugging Face) | aucune release | moteur WebGPU d'abord (Whisper, Phi, Moondream) | — | — | MIT | non adoptable |
| **wgml** (Dimforge) | 0.2.0 (nov. 2024), **wgpu ^23** | noyaux WGSL composables (GPT-2, Llama-2, GGUF) | — | — | MIT/Apache-2.0 | obsolète |
| **Luminal** | 0.2.0 (2024) | CUDA et Metal ; **pas de WebGPU** | — | — | MIT/Apache-2.0 | hors sujet |
| **Kalosm** | 0.4.0 (2025) | API haut niveau sur Candle | — | — | MIT/Apache-2.0 | hors sujet |
| **forge-ml** | 0.3.0 | GPT-2 sur wgpu/WebGPU | — | — | **AGPL-3.0** | **incompatible** avec MIT/Apache |

**Versions de wgpu, vérifiées sur crates.io le 2026-10-14 :** `cubecl-wgpu`
0.10.0 → `wgpu ^29` ; 0.11.0 → `wgpu ^30` ; `burn-wgpu` 0.22.0 → `cubecl ^0.11`.

Sources :
- Burn : https://burn.dev/blog/release-0.21.0/ ·
  https://burn.dev/docs/burn/backend/wgpu/enum.WgpuDevice.html ·
  https://www.phoronix.com/news/Burn-0.20-Released ·
  https://burn.dev/blog/release-0.17.0 · https://docs.rs/crate/burn/0.22.0
- CubeCL : https://github.com/tracel-ai/cubecl/releases ·
  https://docs.rs/crate/cubecl/latest
- web-rwkv : https://docs.rs/web-rwkv · tract : https://docs.rs/crate/tract-onnx/latest
- ort-web : https://docs.rs/ort-web · https://lib.rs/crates/ort-web ·
  https://github.com/pykeio/ort/releases
- Candle : https://crates.io/api/v1/crates/candle-core
- wonnx : https://packages.ecosyste.ms/registries/crates.io/packages/wonnx
- Ratchet : https://github.com/huggingface/ratchet · Luminal :
  https://docs.rs/crate/luminal
- wgml : https://docs.rs/crate/wgml/latest · forge-ml :
  https://docs.rs/crate/forge-ml/latest · Kalosm :
  https://gitblind.noratr.app/floneum/kalosm
- Burn dans le navigateur : localharness (Gemma 3 270M) compile pour wasm32,
  mais sa passe avant n'est **pas encore validée**. —
  https://docs.rs/crate/localharness/0.60.3/source/src/backends/local/mod.rs

### 2.3 Compilateurs qui émettent du WGSL

- **Apache TVM** (v0.24, mai 2026) — Relax + TensorIR → WGSL + runtime wasm ;
  c'est le moteur de WebLLM. Le runtime a son propre device. —
  https://tvm.apache.org/2020/05/14/compiling-machine-learning-to-webassembly-and-webgpu
  · https://whimsy-vm9.apache.org/board/minutes/TVM.html
- **IREE** — HAL WebGPU et cible WGSL ajoutés en 2026, « largement complets ».
  Il vise l'API WebGPU du navigateur via JS ; le pilote natif Dawn est un
  squelette. Expérimental. —
  https://opensecura.googlesource.com/3p/openxla/iree/+/d9a3dd15a552cdded3bda4fcfa65f1341d2b5f92
- **ExecuTorch WebGPU** (PyTorch) — **expérimental**. Réutilise le partitionneur
  Vulkan et passe par Emscripten + emdawnwebgpu ; plus de 100 opérateurs, dont
  SDPA et cache KV. — https://docs.pytorch.org/executorch/main/backends/webgpu/webgpu-overview.html
- **tinygrad** — exportateur WGSL (GPT-2 124M dans le navigateur ; un vieux
  port de Stable Diffusion). Pas de documentation officielle. —
  https://huggingface.co/jik-wtf/tinygpt-webgpu ·
  https://github.com/softwiredtech/stable-diffusion-webgpu
- **Slang → WGSL** — cible **expérimentale**. Différentiation automatique
  intégrée et pont SlangPy/PyTorch : une voie possible pour des *shaders
  neuronaux* compilés en WGSL pour wgpu. —
  https://gamefromscratch.com/khronos-group-launch-slang-shader-language/
- **Triton → WGSL** : aucun projet trouvé. L'analogue le plus proche est le
  backend WebGPU de Futhark (mémoire de master), dont certains programmes cassent
  sur les restrictions de WebGPU. —
  https://futhark-lang.org/student-projects/sebastian-msc-thesis.pdf

### 2.4 Interopérabilité et zéro copie

- **ORT-Web** accepte un device fourni (`env.webgpu.device`) ou expose le sien.
  Entrées par `fromGpuBuffer`, sorties en `gpu-buffer`. **Les entrées doivent
  être des buffers** : une texture ou une image vidéo passe d'abord par une passe
  de calcul.
- **TF.js** : `tf.tensor({buffer, zeroCopy})` et `dataToGPU()`, sur le même
  device seulement.
- **wgpu 29 en wasm** (lu dans la source 29.0.3) :
  - `mod backend;` est privé (`src/lib.rs:114`) ;
  - le `GPUDevice` est dans `WebDevice.inner`, en `pub(crate)`
    (`src/backend/webgpu.rs:1202`) ;
  - seul `backend::custom` est public.

  **Pas d'extraction ni d'adoption d'un device JS par l'API publique**, donc pas
  de zéro copie avec un runtime JS sans patcher wgpu.
- **Textures externes** (vidéo) : wgpu 29 a un type de texture externe dans son
  backend WebGPU, mais aucun runtime ML ne l'accepte. Il faut toujours une passe
  d'échantillonnage vers un buffer, qui reste sur le GPU et coûte peu.
- **Relecture** : `mapAsync` ajoute au moins une frame de latence. Entre deux
  modules wasm (ort-web), il faut aussi copier entre mémoires. Le partage WebNN ↔
  WebGPU n'est sans copie que sur le même GPU. —
  https://github.com/webmachinelearning/webnn/issues/482

---

## 3. Types de réseaux qui tournent, avec chiffres

### 3.1 LLM

- **WebLLM** — Ruan et al. (MLC/CMU), décembre 2024. 4 bits, MacBook Pro M3 Max,
  WebGPU contre MLC natif sur Metal :
  - Llama-3.1-8B : 41,1 contre 57,7 tokens/s (**71 %**) ;
  - Phi-3.5-mini : 71,1 contre 89,3 tokens/s (**80 %**) ;
  - un modèle 3B tourne à environ 90 tokens/s.

  — https://arxiv.org/pdf/2412.15803
- **LlamaWeb** (llama.cpp sur WebGPU) — UC Santa Cruz, mai 2026. Testé sur RTX
  5080, M4 Pro, RX 7900 XT et Arc B580. Décodage 45 à 69 % plus rapide que les
  frameworks web existants, mémoire de crête −29 à −33 % grâce à une arène
  préallouée. Le prefill est plus faible. Petits modèles sur mobile : 4 à 17
  tokens/s *(non vérifié)*. — https://www.alphaxiv.org/abs/2605.20706 ·
  https://www.youngju.dev/blog/2026-07-16-llama-cpp-webgpu-browser-inference.en
- **Transformers.js** :
  - Llama-3.2-1B q4f16 (1,24 Go), Chrome seulement en 2024 ;
  - all-MiniLM-L6-v2 en WebGPU contre wasm : 11 à 45× en lot 1, 28 à 61× en
    lot 32 (mesures de la communauté).

  — https://simonwillison.net/2024/Sep/30/llama-32-webgpu ·
  https://huggingface.co/spaces/Xenova/webgpu-embedding-benchmark/discussions/1
- **Dispatch overhead** (Maczan, 2026) — WebLLM q4f16 : Chrome/Metal à **46,4
  tokens/s, 97 % du MPS natif f16**. Safari fait 30 à 42 tokens/s, Firefox 9,1 à
  9,6 tokens/s. — https://arxiv.org/abs/2604.02344
- **MediaPipe LLM** (Gemma 2B, Phi-2…) exige WebGPU. **Gemini Nano dans Chrome**
  passe par son propre runtime, pas par le WebGPU visible de la page. Il demande
  plus de 4 Go de VRAM et 22 Go de disque, et ne tourne pas sur mobile. —
  https://developers.googleblog.com/en/large-language-models-on-device-with-mediapipe-and-tensorflow-lite/
  · https://developer.chrome.com/docs/ai/get-started

### 3.2 Vision

- **YOLO (v8n, 11n) avec ORT-Web** : aucun banc WebGPU publié trouvé. —
  https://pyimagesearch.com/2025/07/28/run-yolo-model-in-the-browser-with-onnx-webassembly-and-next-js/
- **SAM 2 dans le navigateur** : il faut convertir au format ORT (le graphe brut
  fait planter l'optimiseur). Le schéma courant : l'encodeur lourd une fois par
  image, puis le petit décodeur à chaque clic. Encodeurs fp16 publiés pour
  WebGPU. — https://xevion.dev/projects/websam ·
  https://labelbox.com/blog/bringing-ai-to-the-browser-sam2-for-interactive-image-segmentation/
- **SAM 3, EdgeTAM, Cellpose-SAM** (M1 Max, WebGPU) : encodeur SAM 3 environ 2,5 s
  par image en q4 ; EdgeTAM environ 40 ms *(non vérifié)* ; Cellpose-SAM 277 ms
  par tuile 256². — https://huggingface.co/jax-image-tools/sam3-video-onnx ·
  https://huggingface.co/jax-image-tools/cellpose-sam-onnx
- **Depth Anything V2 Small** (Transformers.js, q4f16) : profondeur en temps réel
  sur webcam (démo), aucun FPS publié. Les contraintes citées sont le
  téléchargement, la mémoire et l'initialisation. —
  https://huggingface.co/spaces/Xenova/webgpu-realtime-depth-estimation/blob/main/index.html
  · https://dev.to/martindelophy/building-cinematic-depth-in-the-browser-with-depth-anything-v2-small-and-webgpu-1k07
- **CLIP / SigLIP / MobileCLIP** : démos WebGPU, pas de latences publiées. Sous
  environ 100 M de paramètres, wasm peut suffire. Démarrage à froid de 1 à 5 s
  *(non vérifié)*. —
  https://www.huggingface.co/spaces/webml-community/mobileclip-webgpu/commit/787389e9e63e419d393512e59b4c43ccf5b8a883
- **Anime4K-WebGPU** (Université de Pennsylvanie) : super-résolution ×2 à ×4,
  débruitage et défloutage **entièrement en shaders de calcul WebGPU, en temps
  réel sur vidéo**. Real-ESRGAN serait plus net mais environ 1 000× plus lent. —
  https://cdn.jsdelivr.net/npm/anime4k-webgpu@1.0.0/README.md
- **Real-ESRGAN dans le navigateur** (TF.js WebGPU/WebGL) : images fixes,
  modèles mis en cache dans IndexedDB. — https://free.upscaler.video/technical/architecture/
- **Détourage RMBG-1.4** : moins de 1 à 3 s par image sur M1 ; wasm 3 à 5× plus
  lent ; plantages WebGPU rapportés sur certaines machines. —
  https://cdn.jsdelivr.net/npm/rembg-webgpu@0.2.1/README.md

### 3.3 Génératif et audio

- **SD-Turbo via ORT-Web** : environ 1 image/s sur RTX 4090 (chiffre
  fournisseur, ORT 1.17). **web-txt2img** : 512², environ 2,3 Go à télécharger.
  — https://opensource.microsoft.com/blog/2024/02/29/onnx-runtime-web-unleashes-generative-ai-in-the-browser-using-webgpu/
  · https://cdn.jsdelivr.net/npm/web-txt2img@0.3.1/README.md
- **Web Stable Diffusion** (MLC, 2023) : 389 s par image sur un M1 iMac dans les
  premières versions. — https://websd.mlc.ai/ ·
  https://gigazine.net/gsc_news/en/20230317-web-stable-diffusion
- **Whisper WebGPU** (base 73M q4, environ 200 Mo mis en cache). —
  https://blog.logrocket.com/how-to-build-a-real-time-voice-ai-agent-in-the-browser
- **Kokoro-82M** (synthèse vocale) : facteur temps réel de 0,2 à 0,4 sur M2 Air
  en WebGPU fp32, contre environ 1,1 en wasm mono-thread. **L'int8 casse le
  décodeur audio : rester en fp32.** —
  https://offlinetts.com/blog/browser-tts-showdown-kokoro-piper-kitten/

### 3.4 Réseaux proches du rendu

- **MobileNeRF** (Google, CVPR 2023) : polygones texturés et petit MLP dans le
  **fragment shader** ; en **WebGL**, il tourne partout, téléphones compris. —
  https://arxiv.org/pdf/2208.00277v4
- **SMERF** (Google, 2023) : grandes scènes explorables en 6 degrés de liberté,
  temps réel sur téléphone, **contenu streamé selon le point de vue**, comme des
  tuiles. — https://arxiv.org/abs/2312.07541v3
- **City-on-Web** (ECCV 2024) : rendu neuronal à l'échelle d'une ville sur le
  web, environ 32 images/s en 1080p sur RTX 3060. —
  https://arxiv.org/html/2312.16457v1
- **Re-ReND** (ICCV 2023), **MixRT** (2023) : des NeRF cuits en maillage plus une
  couleur factorisée, évalués dans un fragment shader standard. —
  https://openaccess.thecvf.com/content/ICCV2023/papers/Rojas_Re-ReND_Real-Time_Rendering_of_NeRFs_across_Devices_ICCV_2023_paper.pdf
- **Compression neuronale de textures** : **aucune implémentation WGSL trouvée**.
  AMD MiniDXNN (D3D12) a une démo interactive ; les MLP sont assez petits pour
  un portage WGSL. —
  https://gpuopen.com/learn/minidxnn-v040-interactive-neural-texture-compression/
  · décodage efficace : https://arxiv.org/pdf/2608.27888
- **Débruiteurs et upscalers neuronaux pour le rendu** : aucun en WebGPU. Un path
  tracer WebGPU à 60 images/s se contente d'accumulation temporelle et d'un
  filtre à-trous. — https://www.jamesdrandall.com/posts/building-a-real-time-path-tracer-in-webgpu/
- **Gaussian splatting avec composante neuronale en WebGPU** : rien trouvé.

### 3.5 Bancs et études

- **Anatomizing Deep Learning Inference in Web Browsers** — Wang et al. (Pékin),
  ACM TOSEM 2024. Neuf modèles, 50 PC et 20 mobiles, en wasm et WebGL (WebGPU
  exclu) :
  - ralentissement par rapport au natif : PC 16,9× sur CPU et 4,9× sur GPU ;
    mobile 15,8× et 7,8× ;
  - mémoire jusqu'à 334× la taille du modèle ;
  - **le temps de rendu de la page augmente de 67 % pendant l'inférence.** Un
    moteur de carte qui partage le GPU est directement concerné.

  — https://arxiv.org/html/2402.05981v2
- **nnJIT** — Jia et al. (Microsoft Research), MobiSys 2024. Génération de noyaux
  à la volée pour wasm et WebGPU : en moyenne 4,44× plus rapide que TF.js, ORT-Web
  ou AutoTVM, coût de compilation environ 100× moindre. —
  https://arxiv.org/pdf/2309.08978
- **Characterizing WebGPU Dispatch Overhead for LLM Inference** — Maczan, février
  2026 ; suite en août 2026 :
  - **24 à 36 µs par dispatch sur Vulkan, 32 à 71 µs sur Metal** ;
  - les microbancs à un seul opérateur surestiment d'environ 20× ;
  - la fusion (876 → 564 dispatchs) donne +53 % ;
  - la soumission représente environ 40 % du temps CPU (12,9 µs par dispatch) ;
  - un capture-and-replay (comme les CUDA Graphs) demanderait de changer la spec.

  — https://arxiv.org/html/2604.02344v1 · https://arxiv.org/pdf/2608.08730
- **@huggingface/kernels** — septembre 2026. **207 noyaux WebGPU** Apache-2.0,
  chacun versionné, testé et mesuré ; **2,57× en moyenne géométrique sur ORT
  WebGPU** sur 809 cas (Apple M4). Une référence algorithmique : on s'en inspire,
  on ne les porte pas (règle 4). — https://huggingface.co/blog/webgpu-kernels
- **WPIA** (2024, époque WebGL) : l'échauffement est très long à cause de la
  compilation des shaders ; remède, précompiler côté serveur. —
  https://alphagalileo.org/fr-fr/Item-Display-fr-FR/ItemId/254362
- **Mobile** : pas de banc LLM WebGPU sur Chrome Android trouvé. En natif, les
  implémentations n'utilisent que 5 à 20 % des unités arithmétiques du GPU mobile
  (étude 2024). — https://arxiv.org/html/2410.03613v1
- **Énergie** : aucune mesure trouvée.

---

## 4. Contraintes de bas niveau

### 4.1 Fonctionnalités et limites

| Élément | Valeur, statut | Conséquence |
|---|---|---|
| `shader-f16` | Chrome 120 ; `enable f16;` ; à demander au device | 2,1× au prefill et 1,3× au décodage (WebLLM, Llama 3 8B) ; 3× sur un banc d'embeddings ; plage et précision réduites |
| **Sous-groupes** | livrés Chrome 134 ; `subgroup_id` (144), `subgroup_uniformity` (145), `subgroup-size-control` (151–152) | 2,3 à 2,9× en matrice-vecteur (Google Meet) ; **taille variable selon le fabricant**, ne jamais la coder en dur |
| **Matrices de sous-groupe** | `chromium-experimental-subgroup-matrix`, **0 % de disponibilité** | rien sur le web ouvert ; Dawn natif seulement (ORT) |
| **Matrices coopératives wgpu** | `EXPERIMENTAL_COOPERATIVE_MATRIX`, `enable wgpu_cooperative_matrix;` ; Metal (Apple7/Mac2, MSL 2.3+) et Vulkan (`VK_KHR_cooperative_matrix`) ; tailles par `cooperative_matrix_properties()` | **natif seulement**, instable ; 8×8 sur Metal, 16×16 f16 sur AMD Vulkan |
| **Produit scalaire int8** | `packed_4x8_integer_dot_product` (Chrome 123) ; `dot4I8Packed`, `pack4xI8`… | int8 : jusqu'à −75 % de mémoire et de bande passante contre f32 |
| `timestamp-query` | optionnel ; **arrondi à 100 µs dans Chrome** | suffit pour budgéter une frame, pas pour profiler un noyau |
| `maxStorageBufferBindingSize` | **128 Mio par défaut** | découper les poids, ou demander plus si l'adaptateur le permet (TVM demande 1 Go et retombe à 128 Mio) |
| `maxBufferSize` | **256 Mio par défaut** | idem |
| `maxComputeWorkgroupStorageSize` | **16 Kio** par défaut, et minimum WGSL | tuiles de mémoire partagée limitées (environ deux tuiles 64×32 en f32) |
| `maxStorageBuffersPerShaderStage` | minimum 8 | les Concat/Split larges débordent |
| **Pas de f64** en WGSL | — | cohérent avec la règle 7 : le f64 reste sur le CPU, rebasement local avant envoi |
| **wasm memory64** | Firefox 134, Chrome 133 ; pas Safari ; **10 à plus de 100 % plus lent** | rester en wasm32 et **garder les poids hors de la mémoire linéaire**, en les écrivant directement dans des buffers GPU |
| **Relecture** | copie vers un buffer `MAP_READ`, puis attente du GPU ; sous Safari, `mapAsync` attend des command buffers sans rapport | modèle d'Unity : relecture asynchrone, sans blocage, avec quelques frames de latence |

Sources :
- f16 : https://developer.chrome.com/blog/new-in-webgpu-120?hl=de ·
  https://developer.chrome.com/blog/io24-webassembly-webgpu-2
- sous-groupes : https://developer.chrome.com/blog/new-in-webgpu-134 ·
  https://developer.chrome.com/blog/new-in-webgpu-144?hl=vi ·
  https://developer.chrome.com/blog/new-in-webgpu-151-152?hl=hi
- matrices de sous-groupe :
  https://web3dsurvey.com/webgpu/features/chromium-experimental-subgroup-matrix ·
  https://dawn.googlesource.com/dawn/+/43f5e8e6046c8191150068e6f6c54f6238af8b0f
- matrices coopératives wgpu :
  https://wgpu.rs/doc/wgpu/documentation/extensions/cooperative_matrices/index.html
- int8 : https://developer.chrome.com/blog/new-in-webgpu-123
- limites : https://developer.mozilla.org/en-US/docs/Web/API/GPUSupportedLimits ·
  https://apache.googlesource.com/tvm/+/HEAD/web/src/webgpu.ts ·
  https://web3dsurvey.com/webgpu/limits/maxStorageBufferBindingSize
- memory64 : https://spidermonkey.dev/blog/2025/01/15/is-memory64-actually-worth-using.html
- relecture : https://bugs.webkit.org/show_bug.cgi?id=272804 ·
  https://docs.unity3d.com/2018.4/Documentation/ScriptReference/Rendering.AsyncGPUReadback.html

### 4.2 Écrire des noyaux en WGSL

- **Matmul** :
  - pas de BLAS fournisseur en WebGPU, d'où la compilation (TVM) ou les noyaux
    maison ;
  - une matmul en tuiles 16×16 via wgpu/Vulkan atteint **1,2 à 2,1 TFLOP/s sur
    RTX 5090** aux tailles réelles d'un MLP, contre 30 GFLOP/s en 256³ ;
  - sur Apple Silicon, un blocage en registres vec4 approche 1 TFLOPS f32, et la
    mémoire partagée y était *plus lente* : **prévoir des variantes par
    fabricant** ;
  - le journal pas à pas de Simon Boehm (CUDA) reste le guide classique.

  — https://arxiv.org/abs/2604.02344 · https://github.com/jbingen/tabgpt ·
  https://gwern.net/doc/www/siboehm.com/d4b3e9d6de0655f490fe6369f4d004936248a833.html
  · https://github.com/Rust-GPU/rust-gpu.github.io/pull/53.diff
- **Quantification** (LlamaWeb) : les formats quantifiés sont des buffers `u32`
  plats, avec un déquantification choisi à la compilation. En q2_k, la
  déquantification annule le gain de bande passante. —
  https://www.alphaxiv.org/abs/2605.20706
- **Les compilateurs WebGPU optimisent peu** : dérouler les boucles à la main
  paie (nnJIT). — https://arxiv.org/pdf/2309.08978
- **Attention** : FlashAttention en gabarits WGSL dans ORT ; un chemin paged
  attention fp16 environ 2× plus rapide au décodage ; dispatch indirect pour les
  longueurs variables. WebInfer porte l'API FlashInfer en WebGPU *(non
  vérifié)*. — https://app.semanticdiff.com/gh/microsoft/onnxruntime/pull/24400/overview
  · https://cdn.jsdelivr.net/npm/webinfer@0.0.5/README.md
- **Convolution** (ORT 1.30) : im2col + matmul avec activations fusionnées et
  poids préempaquetés. — https://newreleases.io/project/github/microsoft/onnxruntime/release/v1.30.0
- **Création de pipelines** : `createComputePipelineAsync` pour éviter les
  blocages. Le cache de shaders de Dawn saute Tint et le compilateur du backend
  quand le shader est déjà connu. — https://developer.mozilla.org/docs/Web/API/GPUDevice/createComputePipelineAsync
  · https://dawn.googlesource.com/dawn/+/e9cfa262db36400a579cac3f2720dd51233c8a8e
- **Contrôle des bornes** : Tint propose les modes clamp, predicate et ignore.
  Sauter la vérification sur D3D12 rapporte environ 11 %. naga en injecte aussi
  *(non vérifié)*. — https://dawn.googlesource.com/dawn/+/455e4b80f6b8d065660ced359bf836f72c656f33

### 4.3 Numérique et déterminisme

- **fp16** :
  - Gemma-3-270M q4f16 est inutilisable en WebGPU (débordement suspecté) ;
  - un débordement peut rendre toute la passe NaN, ce qu'on corrige en
    remettant à l'échelle quelques canaux aberrants (HEAL, 2026).

  — https://github.com/microsoft/onnxruntime/issues/26732 ·
  https://arxiv.org/pdf/2606.21023
- **Non-déterminisme** :
  - les causes : non-associativité du flottant, choix de noyau selon le matériel,
    unités matricielles propres à chaque fabricant ;
  - dans une étude, les atomiques flottants étaient la seule source de
    non-déterminisme d'une exécution à l'autre ;
  - des GEMM à ordre fixe donnent des sorties identiques au bit entre
    générations NVIDIA, pour un coût de 30 à 60 % *(non vérifié)*.

  — https://arxiv.org/pdf/2603.20421 · https://arxiv.org/pdf/2511.20621 ·
  https://www.emergentmind.com/papers/2609.25624
- **Aucune étude de reproductibilité entre backends WebGPU trouvée.** Il faut
  s'attendre à quelques ULP d'écart entre D3D12, Vulkan et Metal, et **tester
  avec des tolérances**, pas au bit près. Pour les films de ferme (même pack →
  mêmes octets), soit on fige le GPU et le pilote, soit on **stocke la sortie**.

### 4.4 Déploiement

- **Mise en cache des modèles** : l'API Cache par défaut, ou OPFS ou IndexedDB ;
  URL immuables et longues durées de cache. —
  https://developer.chrome.com/docs/ai/cache-models
- **Requêtes partielles et safetensors** : aucune source trouvée. L'en-tête de
  safetensors donne les offsets de chaque tenseur, qui se prêtent naturellement
  aux requêtes HTTP partielles (déduction). C'est le même schéma que les tuiles
  de Tuile.
- **Une seule file, pas de calcul asynchrone parallèle** : WebGPU v1 n'a qu'une
  file, et l'inférence passe à la suite du rendu. —
  https://github.com/gpuweb/gpuweb/wiki/GPU-Web-2020-09-28
- **Perte de device** : la promesse `GPUDevice.lost`, puis tout recréer, avec un
  nombre d'essais borné. Environ la moitié des applications WebGL ne
  récupéraient pas d'une perte de contexte. —
  https://developer.mozilla.org/docs/Web/API/GPUDevice/lost ·
  https://github.com/gpuweb/gpuweb/wiki/Minutes-2021-04-26

---

## 5. Ce qu'il faut en retenir pour un moteur Rust/wgpu

1. **Rester sur un seul device.** Le seul chemin zéro copie est **natif à
   wgpu** : Burn/CubeCL sur le `wgpu::Device` de Tuile (`init_device(WgpuSetup)`),
   ou du WGSL écrit à la main.
   - Les runtimes JS imposent un device séparé et des copies, car wgpu 29
     n'expose ni n'adopte de `GPUDevice` JS.
   - Garder **Burn 0.21 tant que Tuile est en wgpu 29**, puis avancer au même
     rythme.
2. **Compter les dispatchs avant les FLOPs.**
   - Fusionner biais, activation et normalisation dans l'épilogue de la matmul.
   - Enregistrer les dispatchs ML dans l'encodeur de la frame, avec une
     soumission par frame.
   - Mesurer en dispatchs successifs, pas opérateur par opérateur.
3. **Découper les poids et les garder hors de la mémoire wasm.**
   - Chaque tenseur sous 128 Mio ;
   - chargement en flux (`fetch` ou requêtes partielles) directement dans des
     buffers GPU (`queue.write_buffer`) ;
   - cache sous URL immuables, comme des tuiles.

   100 M de paramètres font environ 200 Mo en f16, 100 Mo en int8, 50 Mo en int4.
4. **Des variantes de shaders par palier, choisies à la création du device :**

   | Palier | Contenu | Disponibilité |
   |---|---|---|
   | 0 | f32, au plus 16 Kio de mémoire partagée, sans sous-groupes | partout |
   | 1 | stockage f16 avec accumulation f32, sous-groupes pour les réductions | selon navigateur et GPU |
   | 2 | int8 en `dot4I8Packed` | selon navigateur et GPU |
   | 3 | matrices coopératives wgpu | **natif seulement** |

   Créer tous les pipelines au chargement, de façon asynchrone, et les chauffer.
5. **Étaler l'inférence sur plusieurs frames, dans l'unique file.**
   - Un lot borné de dispatchs par frame, réglé d'après `timestamp-query` (exact
     en natif, arrondi à 100 µs dans Chrome) ou le temps CPU de frame.
   - Des dispatchs courts, pour éviter les délais de garde du GPU (environ 2 s
     sous Windows, non vérifié).
   - Les sorties restent sur le GPU, consommées par les passes de rendu.
6. **Numérique.**
   - Accumulation f32, softmax avec soustraction du maximum.
   - Réductions en arbre à ordre fixe plutôt qu'atomiques flottants.
   - **Tests de référence avec tolérance** sur Vulkan, Metal et D3D12 natifs,
     ainsi que dans Chrome et Safari.
7. **Le ML améliore le rendu, il ne le conditionne jamais.** C'est la règle
   « le noir est un bogue » : toute surface issue d'un réseau se pose sur un repli
   sans réseau toujours résident. En cas de perte de device ou de fonctionnalité
   absente, on dessine d'abord le repli. Sur le web, c'est **courant**, pas rare :
   Linux n'a WebGPU que pour 9 à 16 % des utilisateurs.
8. **Architecture du dépôt.**
   - L'inférence va dans son propre crate, derrière un trait, jamais dans
     `tuile-core` (règle 1).
   - Burn et CubeCL sont en 0.x : **justification écrite dans la PR** pour le
     chemin wasm (règle 8).
   - À éviter :
     - forge-ml (AGPL) ;
     - wonnx et wgml (obsolètes) ;
     - TF.js (en sommeil) ;
     - Candle pour le GPU (pas de wgpu).

---

## 6. Licences en un coup d'œil

| Utilisable | À éviter, ou à surveiller |
|---|---|
| Burn, CubeCL, web-rwkv, tract, ort/ort-web, Candle (MIT/Apache-2.0) | **forge-ml (AGPL-3.0)** |
| ONNX Runtime (MIT), Transformers.js, LiteRT.js, TVM (Apache-2.0) | ort-web : CDN et télémétrie par défaut (à couper) |
| @huggingface/kernels (Apache-2.0) : inspiration algorithmique, pas portage (règle 4) | SDK NVIDIA RTXNTC (non OSI ; cf. la note sur les textures neuronales) |

---

## 7. Lacunes de la littérature et des outils

- Aucune étude publiée sur la **pression mémoire** quand un moteur de rendu et un
  runtime ML partagent un device.
- Aucune mesure de la **latence de première inférence** en WebGPU (compilation
  et téléversement) ; seulement des anecdotes de 300 ms à 5 s.
- Aucune étude de **reproductibilité entre backends** WebGPU.
- Aucun **réseau neuronal intégré au rendu en WGSL** publié (NTC, débruiteur,
  upscaler), hors Anime4K. C'est une place à prendre.
- Aucun banc de **Burn/CubeCL en WebGPU** publié.
- Aucune mesure d'**énergie**.

---

## 8. Essais proposés (les nôtres)

Chacun produit un film ou des captures **regardés**, en plus des chiffres. Le
banc de dispatch sert ensuite à tous les autres.

1. **Banc de dispatch Tuile** (2 jours). Mesurer le coût par dispatch et par
   soumission sur nos cibles : Vulkan (L4 de la ferme), Metal, D3D12, puis
   Chrome, Safari et Firefox. On le compare aux 24–71 µs publiés, pour fixer
   notre budget ML par frame.
2. **Burn 0.21 sur le device du viewer** (2 à 3 jours). Un MLP ou un CNN de
   moins de 1 M de paramètres (super-résolution ×2 d'une tuile) via
   `init_device(WgpuSetup)` :
   - natif et wasm, même code ;
   - sortie copiée dans une texture sans relecture ;
   - mesures : temps par tuile, nombre de dispatchs, écart f16/f32, parité
     natif/wasm.
3. **Le même réseau en WGSL écrit à la main**, fusionné en une ou deux passes
   (2 à 3 jours). Comparaison avec Burn sur les dispatchs, le temps et la taille
   du binaire wasm. C'est ce qui tranche entre framework et noyaux maison.
4. **Paliers de shaders** (1 semaine). Variantes f32, f16 et sous-groupes, puis
   coopératif en natif, sur un même noyau ; gain par plateforme et taux de repli
   réel dans les navigateurs.
5. **Étalement sur plusieurs frames** (1 semaine). Une inférence de 10 à 50 ms
   découpée sur plusieurs frames sans dépasser le budget, sur un film de
   référence, pour vérifier que le rendu ne saccade pas.
