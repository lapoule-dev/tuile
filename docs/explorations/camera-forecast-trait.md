<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Le forecast de caméra que l'hôte expose à tuile : conception

Statut : **proposé, 2026-11-28, conception seule** (aucun code de production), pour la
validation de Laurent. Branche `feat/camera-forecast-trait`, sur `main` 40a1ee16.

Demande de Laurent (2026-11-28) : « le besoin à t+L est normalement un trait implémenté par
le système de caméra, et en tout cas il est EXPOSÉ à tuile afin de lui permettre d'être plus
rapide… faut voir ça comme une estimation de la caméra à afficher dans les N prochaines
secondes… c'est normalement une prédiction ultra rapide qui doit aider tuile à prendre des
décisions qui ne seront pas en retard. Mais ça ne change absolument rien à nos plans sur le
préchargement et le budget. »

**Ce que ce document définit.** Une interface, le trait `CameraForecast`. Par elle, le
système de caméra de l'hôte dit à tuile quelle caméra sera affichée à des instants donnés
des prochaines secondes. tuile s'en sert pour décider à temps : c'est la compensation du
retard.

**Ce qu'il ne définit pas.** Le préchargement par valeur attendue (P1–P12) et le besoin à
`t + L` (§2.5) de `budget-preload-by-expected-value.md` (branche
`feat/budget-preload-by-expected-value`, a74799bc et ba6e5a17) ne changent pas. Ce document
dit seulement d'où viennent les entrées que ces plans lisent déjà : la pose `p_L` du besoin
décalé, et les moyennes `μ_k` et écarts types `σ_k` des instants `τ_k > L`. Il ne touche ni
au score, ni à la garde, ni aux quotas.

Légende des appuis, celle du document du budget : **[code]** vérifié dans l'arbre à
40a1ee16 ; **[spec]** déjà écrit et validé ; **[lit.]** une référence publiée ; **[dérivé]**
une conséquence des termes précédents ; **[supposé]** une hypothèse à mesurer avant de s'y
fier.

---

## A. Critères d'acceptation (objectifs chiffrés de Laurent)

Ces critères s'appliquent à toute PR de ce chantier. La suite de conformité (§4.6) et le
démonstrateur (§10) les mesurent.

**A1. Le temps de décision de tuile reste sous 2 ms par pas, et le forecast ne l'alourdit
pas.** Ce sont deux coûts distincts, chiffrés à part (§1.5 et §1.7) :

- **Exposer** : le coût de l'API côté hôte. Elle **ne fait aucune traversée**, ni culling,
  ni SSE. Elle lit une pose ou une trajectoire que la caméra a déjà calculée, la copie et
  la convertit de repère. Elle se compte en microsecondes, sur le fil de l'hôte, **hors**
  des 2 ms.
- **Consommer** : le coût côté tuile. On valide et on convertit les échantillons reçus, on
  calcule la clé de conflation, puis, seulement si la conflation le demande, on traverse
  l'arbre sur la pose prévue. Ce coût-là compte **dans** les 2 ms. La conflation (§1.7)
  évite de refaire la traversée tant que la pose prévue ne change pas assez.

Budget par poste, p99, en natif. Les cibles sont **[supposé]** et seront confirmées par la
mesure (§10).

| Poste | Où | Moyenne visée | Pire pas visé | Dans les 2 ms ? |
|---|---|---|---|---|
| Appel de l'API (`generation`, puis `forecast` si la génération change) | hôte | ≤ 2 µs (lecture de la génération seule) | ≤ 10 µs natif, ≤ 30 µs wasm | non |
| Réception : décodage, validation, conversion | runtime | ≤ 5 µs | ≤ 10 µs | oui |
| Clé de conflation (tuile sous l'œil prévu, niveau voulu, par échantillon) | runtime | ≤ 5 µs | ≤ 10 µs | oui |
| Traversée sur les poses prévues | runtime, **seulement si la clé change** | ≈ 0 au repos et en vol connu | ≤ 0,5 ms (un quart des 2 ms) | oui |
| Quantification dans le `Need` (`AheadUse`, puis `Need.ahead[K]`) | runtime | O(Δ) entrées | O(\|C\|·K), une fois par génération | oui |
| `decide` | tuile-budget | inchangé par le forecast | inchangé | oui |

La part du forecast dans les 2 ms vaut donc au plus 20 µs en moyenne (1 %). Au pire pas,
celui où la conflation relance la traversée, elle vaut au plus 0,5 ms (25 %).

**Ce que le forecast doit éviter pour tenir** : toute allocation par image ; toute
traversée que la conflation n'a pas demandée ; tout flottant dans `decide`, où les valeurs
arrivent quantifiées une seule fois ; tout appel de l'hôte depuis le runtime ou le
décideur ; toute relecture des échantillons quand la génération n'a pas changé.

**Mesures de référence existantes** **[code]** : `tuile_budget_decide_seconds` et
`tuile_budget_decide_peak_seconds` (`observe.rs:458-461`), le bench `budget_decide`
(`still`, `still-full`, `motion`, `pressure`, et `diff` pour le côté hôte du delta),
`traversal_seconds` (`metrics.rs:347`) et le `work` par pas. **Constat préalable** : le
dernier relevé publié, sur ce Mac, donne `decide` à 1,1 ms de moyenne et 1,4 ms p99 vue
immobile, mais 2,3 ms de moyenne et **6,9 ms p99 en zoom**
(`graphics-budget-architecture.md:486`). L'objectif de 2 ms n'est donc pas tenu
aujourd'hui en mouvement, indépendamment du forecast. Ce chantier n'a pas à le corriger,
mais il ne doit pas l'aggraver : chaque PR rapporte les quatre benchs avant/après. De
plus, aujourd'hui, **chaque message `Ahead` relance la passe sur les vues futures**
(`ahead_moved = true` à chaque réception, `runtime.rs:1258` et `1976-1992`). Comme le viewer
envoie ce message à chaque image en mouvement, la passe tourne à chaque pas. La conflation
supprime ce coût, qui est le principal poste du forecast aujourd'hui.

**A2. Une API de caméra qui sait exposer où elle va.** Le trait rend la **trajectoire**
prévue, et pas seulement une pose extrapolée. Un film, un Glide, une bande ou la glissade
d'un lancer donnent leur trajectoire exacte, en un seul calcul pour tout le vol. Le
`CameraController` lui-même connaît sa destination, la cible vers laquelle il amortit
(§1.8). En interaction, l'API rend une estimation avec son incertitude `σ`. L'hôte déclare
son état (au repos, en vol connu, en interaction) **dans le forecast lui-même**, champ
`motion` (§1.2). Cet état voyage dans le message et il est enregistré : tuile le connaît
sans état caché. Aucune décision ne branche dessus, puisque les règles de conflation
découlent des données (§1.7). Il sert aux mesures et au démonstrateur.

---

## 0. Ce qui existe aujourd'hui : le constat

Tout ce qui suit est lu dans le code **[code]**, sans exécution.

1. **La prédiction appartient au viewer, pas à l'hôte.** `tuile_camera::predict::Predictor`
   (`predict.rs:783-928`) est appelé par le viewer (`app/predicting.rs:110-194`). Le viewer
   choisit lui-même, dans `views_ahead` (`predicting.rs:219-257`), entre quatre futurs : les
   vues annoncées par un hôte, un rejeu de bande, un Glide, une glissade après un lancer.
   S'il n'y en a aucun, il prend le filtre de Kalman. Un hôte qui possède sa caméra
   (simulateur, moteur de jeu) n'a qu'une porte : `PreloadView::ahead` avec
   `Priority::Ahead { in_us }` (`frame_host.rs:130-175`). Il y pousse des poses sans écart
   type, et le viewer les rapproche des horizons au plus proche, à ±25 % près
   (`predicting.rs:231-236`).
2. **Les horizons appartiennent à la caméra.** `PredictConfig::horizons_s` (0,25 / 0,5 / 1 /
   2 s, `predict.rs:275`) est dans la configuration du prédicteur. Or c'est tuile qui sait
   à quels instants il doit décider : `L` pour le besoin, les `τ_k` pour le préchargement.
3. **L'incertitude est jetée.** `MotionModel::predict` rend `σ` par canal
   (`predict.rs:600-620`), mais `Ahead` ne porte que la pose et `p = e^(−τ/λ)`
   (`predict.rs:882-917`). C'est le constat 3 du document du budget.
4. **Le temps est relatif à l'envoi.** `AheadView.in_us` (`protocol.rs:141-148`) compte à
   partir de l'instant où le viewer construit le message. Le runtime le reçoit plus tard,
   après la file du worker ou le réseau, et il n'a aucun moyen de savoir de combien. Toutes
   les échéances sont décalées de ce retard, et un message périmé ne se reconnaît pas. Le
   viewer lit `web_time::Instant` (`predicting.rs:112`), pas l'horloge de la session
   (`runtime::Clock`, `runtime.rs:341`). Sur wasm, l'horloge par défaut de la session
   compte 16 ms par pas (`runtime.rs:352-356`) : elle n'est pas du tout comparable.
5. **Le film ment sur ses instants.** `tuile-bake` envoie ses vues futures avec
   `AheadView::certain(v, order)` : la vue `i` est due `i` microsecondes plus tard
   (`tuile-bake/src/session.rs:942-948`). Son ordre est juste, ses échéances sont fausses.
   Or un film connaît exactement l'instant de chacune de ses images.
6. **Bevy n'a pas la porte.** `tuile_bevy::camera::Priority` n'a que `Now` et `Later`
   (`tuile-bevy/src/camera.rs:124-131`). Un hôte Bevy ne peut rien annoncer de son futur.
7. **Ce qui est enregistré.** Le pas enregistre l'empreinte des nombres du prédicteur
   (`BudgetEvent::Predictor`, `runtime.rs:1245-1248`) et, par tuile, l'usage prévu quantifié
   (`Need.ahead: AheadUse`, `need.rs:551-575`). Le rejeu de `decide` est donc exact.
   Je n'ai pas trouvé d'enregistrement des vues futures elles-mêmes (grep sur `tuile-record`
   et `tuile-tape`). Le rejeu du **runtime**, qui calcule le `Need` à partir des vues, ne
   peut donc pas les redonner. C'est à vérifier dans l'enregistreur MCAP de la télémétrie
   avant la PR 0 (§6).
8. **Le coût n'est pas borné.** `Predictor::frame` alloue un `Vec<Ahead>` à chaque image et
   clone les horizons (`predict.rs:880-888`). `views_ahead` alloue aussi, pour les listes
   de la glissade et du rejeu. C'est petit, mais ce n'est pas « sans allocation ».

---

## 1. Le contrat

### 1.1 La question posée

tuile pose une seule question, à chaque image : **« Quelle caméra sera affichée à chacun de
ces instants ? »** Les instants sont choisis par tuile, sur l'horloge de la session.

Il y avait trois formes possibles :

| Forme | Pour | Contre |
|---|---|---|
| une pose à `t + τ`, pour un `τ` donné | minimale | un appel par horizon ; l'incertitude manque |
| une trajectoire échantillonnée par l'hôte | naturelle pour un film | tuile ne choisit pas ses instants et doit rééchantillonner (le « plus proche à ±25 % » d'aujourd'hui) |
| **une pose et sa dispersion, à chacun des instants demandés par tuile** | couvre les deux autres ; un appel par image ; tuile garde le choix de `L` et des `τ_k` | un tableau à remplir |

**Choix : la troisième, étendue à une trajectoire et à plusieurs hypothèses.** tuile
indique les instants dont il a besoin et l'horizon jusqu'où il en veut. L'hôte rend une ou
plusieurs **hypothèses**, chacune une trajectoire échantillonnée avec un poids et une
dispersion par échantillon :

- **un hôte exact** (film, Glide, bande) rend une seule hypothèse, sur ses propres instants
  clés, jusqu'à la fin connue de son vol ou jusqu'à l'horizon demandé. tuile interpole
  entre ces instants (position linéaire en ECEF f64, orientation par slerp, la même règle
  que `CameraController::update`, `lib.rs:1060-1066` **[code]**). Il n'y a aucun
  rééchantillonnage « au plus proche » ;
- **un hôte interactif** rend une hypothèse aux instants demandés : la moyenne et `σ` de son
  filtre ;
- **un hôte qui hésite** rend plusieurs hypothèses pondérées. Par exemple, un Glide qu'une
  main peut interrompre : « le vol continue » pèse `e^(−τ/T_i)`, et « la caméra s'arrête
  où elle est » pèse le reste. Autre exemple, une caméra de jeu qui peut suivre l'un de deux
  personnages. C'est un mélange, et le budget en tire `P_k` par somme pondérée (§3.2).

Un hôte qui ne rend qu'une hypothèse de poids 1 a le contrat de départ. Plusieurs
hypothèses ne coûtent que des échantillons de plus, dans un tampon de capacité fixe.

### 1.2 Les types

Esquisse ; les noms sont à discuter en PR, pas la forme. Tout est dans `tuile-core`
(module `forecast`), sans aucune dépendance nouvelle : `glam` y est déjà **[code]**. Aucun
type de `tuile-camera`, `nalgebra` ni `bevy` n'apparaît à la frontière.

```rust
/// One instant of the forecast: the camera that will be displayed at `at`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ForecastSample {
    /// The instant this pose is displayed at, on the session's clock.
    pub at: Micros,
    /// The eye, ECEF metres, f64. For a stereo rig, the rig's origin (the head).
    pub position: DVec3,
    /// Unit forward and up, ECEF axes.
    pub direction: DVec3,
    pub up: DVec3,
    /// The lens: symmetric (vertical fov) or four angles (OpenXR's convention).
    pub lens: Lens,
    /// One standard deviation of the pose, per channel (below). All zero: exact.
    pub spread: Spread,
    /// This hypothesis's weight at `at`, thousandths. Over the hypotheses the
    /// weights at one instant sum to at most 1000; what is left is "unknown",
    /// the reachable floor's (P8). 1000 for a film, a replay, or an estimator
    /// whose uncertainty is all in `spread`.
    pub weight_permille: u16,
}

/// The host's state, as it declares it: recorded, read by the measures only.
pub enum Motion { Still, Known, Interactive }

/// What tuile asks: the instants it decides for, and how far ahead it wants
/// the trajectory. Plus the tolerance under which a change is not worth a new
/// generation (§1.7), in the host's own units.
pub struct ForecastAsk<'a> {
    pub now: Micros,
    pub at: &'a [Micros],   // ascending, ≤ MAX_INSTANTS (16)
    pub until: Micros,      // now + H (+ the conflation's lead, §1.7)
    pub tolerance: Tolerance,
}

/// What the host answers into: a buffer tuile owns and reuses, fixed capacity.
pub struct Forecast {
    /// Bumped by the host only when its trajectory or destination moved past
    /// `ask.tolerance` (§1.7). Unchanged: tuile skips the read and the walk.
    pub generation: u64,
    pub motion: Motion,
    /// ≤ MAX_HYPOTHESES (4), each ≤ MAX_SAMPLES (64) samples, ascending `at`.
    pub hypotheses: Hypotheses,
}

/// One standard deviation per channel, in the frame of the present pose:
/// ground east and north (m), log of the height above the ellipsoid, heading
/// and pitch (rad). The motion model's five channels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Spread {
    pub east_m: f32,
    pub north_m: f32,
    pub log_height: f32,
    pub heading_rad: f32,
    pub pitch_rad: f32,
}

/// What a host's camera system tells tuile of where the camera goes. It reads
/// what the camera already computed: it walks no tree, culls nothing, prices
/// no screen-space error — that is all tuile's, the consumer's.
pub trait CameraForecast {
    /// The generation of the forecast `forecast` would write now: O(1). tuile
    /// calls it every frame and `forecast` only when it changed.
    fn generation(&self, ask: &ForecastAsk<'_>) -> u64;

    /// Writes the forecast into `out`, its hypotheses' samples covering
    /// `ask.at` up to `ask.until` or the forecast's known end, whichever is
    /// first. No hypothesis: the host knows nothing. No allocation, no I/O,
    /// no lock, no blocking.
    fn forecast(&mut self, ask: &ForecastAsk<'_>, out: &mut Forecast);
}
```

`Hypotheses` est un tableau de capacité fixe (`[Hypothesis; 4]` et une longueur), sans
dépendance nouvelle. Taille au pire : 4 × 64 × 152 o ≈ 39 Kio, copiés **seulement quand la
génération change**. Un hôte interactif typique en remplit 1 × 16, soit 2,4 Kio.

`Lens` reprend la lentille de `ViewStateParams` (`fovy_rad` ou `Fov`, `traversal.rs:68` et
`101-113`) **[code]**. La fenêtre et l'échelle n'en font pas partie : tuile les a déjà. Il
reconstruit la `ViewState` de chaque échantillon avec la fenêtre de la vue présente, comme
`predicting.rs:171-181` le fait aujourd'hui.

`Envelope` étend l'actuel `Reach` (`protocol.rs:165-171`). Ce sont les limites que la
caméra déclare d'elle-même : panoramique maximal au sol, en hauteurs par seconde ; zoom
arrière maximal ; vitesse de rotation maximale ; altitudes min et max ; amortissement ;
glissade. Elle voyage avec le forecast, et elle est la source unique de ces nombres pour le
budget (§9.2).

### 1.3 Unités et repères

- **Positions et axes en ECEF, f64** (règle 7). C'est le type de caméra que tuile utilise
  déjà partout (`GlobeCamera`, `frame_host::Pose`) **[code]**. Le rebasage vers un repère
  local ne se fait que dans le rendu, jamais dans le forecast.
- **Dispersion en f32**, dans le repère local de la pose **présente** : est et nord en
  mètres, `ln h`, cap et tangage en radians. Ce sont exactement les canaux du filtre
  (`predict.rs:440-445`) et le `σ_k` que le budget attend dans `AheadView` (§2.1 du
  document du budget). Un écart type n'est pas une coordonnée : f32 suffit, et la règle 7
  ne s'applique pas.
- **Diagonale** : un écart type par canal, sans covariances. C'est l'hypothèse du filtre
  d'aujourd'hui (canaux indépendants) et celle du calcul de `P_k` du budget (T1, Monte Carlo
  par canal) **[supposé]**. À revoir si la mesure montre des erreurs corrélées entre cap et
  sol, ce qui est plausible dans un virage.
- **Stéréo** : le forecast porte la pose de la tête, c'est-à-dire l'origine de la
  `Rig` (`app/eyes.rs`). Les yeux en sont tirés comme pour l'image présente. Un casque
  dont les yeux bougent par rapport à la tête (IPD dynamique) relève d'une lentille qui
  change, pas du forecast.

### 1.4 Le temps

- **Une seule horloge : celle de la session.** `runtime::Clock` est « microsecondes depuis
  une origine que l'hôte choisit » **[code]**. Les `at` demandés et rendus sont sur cette
  horloge-là. L'hôte qui implémente le trait la reçoit (`now`) et convertit vers la sienne
  s'il le faut : un `Instant`, `performance.now()`, le `XrTime` d'OpenXR, le temps d'un
  film.
- **« Affichée », pas « calculée ».** L'échantillon à `at` est la pose de l'image que
  l'utilisateur **voit** à `at`. La latence de présentation (une à trois images dans la
  chaîne d'échange, le `predictedDisplayTime` d'un casque) est donc l'affaire de l'hôte, qui
  la connaît. tuile n'a pas à la modéliser. `L` (§2.5) reste le délai entre la décision et
  l'arrivée **dessinable**, mesuré par tuile (M1 du budget), et l'instant demandé pour le
  besoin est simplement `now + L`.
- **Sur le fil.** Le message porte `issued_at` (la valeur de `now` passée au trait) et,
  pour chaque échantillon, `at − issued_at` en µs. Quand le serveur partage l'horloge de
  l'hôte (en processus, ou dans un worker auquel l'hôte a passé sa propre horloge),
  `issued_at` est exact : les échéances le sont aussi, et l'âge d'un message se mesure.
  Quand le serveur ne la partage pas (`tuile-server` par WebSocket), il prend l'instant de
  réception comme `issued_at`. L'erreur vaut alors le délai aller, qu'on suit par la
  moitié du RTT déjà mesuré. C'est **[supposé]** suffisant devant `L` ≈ 200 ms.
- **Sur wasm**, l'horloge par défaut de la session ne convient pas (16 ms par pas). Un hôte
  web doit passer la sienne (`GeometryServer::with_clock`, `performance.now()`). C'est déjà
  la consigne du code **[code]**. Le forecast la rend obligatoire : sans elle, les instants
  n'ont aucun sens.

### 1.5 Le coût garanti

- **Appelé une fois par image, par la liaison, avant l'envoi de la caméra.** Ni le décideur
  ni le runtime ne l'appellent : ils ne lisent que les échantillons reçus (§1.6). Le trait
  n'est donc jamais sur le chemin de `decide`, et ne peut pas l'arrêter.
- **`O(K)`, sans allocation, sans E/S, sans verrou.** `K ≤ 16` instants : c'est déjà la
  borne de `PredictConfig::set` (`predict.rs:235`) **[code]**. Le tableau `out` est fourni
  par tuile et réutilisé d'une image à l'autre.
- **Cible du coût d'exposer : ≤ 2 µs pour `generation`, ≤ 10 µs pour `forecast` à
  `K = 16` en natif, ≤ 30 µs en wasm.** À 60 Hz, une image dure 16,7 ms ; à 90 Hz, 11,1 ms.
  10 µs en représentent 0,06 % et 0,09 %. C'est **[supposé]** : le filtre fait 5 canaux ×
  K produits 2×2, soit quelques centaines de flops, et un chemin exact évalue une
  interpolation. La suite de conformité mesure le **temps d'appel de l'API, jamais une
  traversée**, et fait échouer l'hôte lent (§4.6). La métrique
  `tuile_forecast_call_seconds` le suit en production.
- **Le coût de consommer** est celui de tuile, chiffré au §A et au §1.7.
- **Pas d'état caché côté tuile.** `&mut self` laisse l'hôte faire avancer son propre état
  (le filtre observe la pose présente). Tout ce que tuile en tire passe par les
  échantillons rendus.

### 1.6 Déterminisme et rejeu : le forecast est une entrée enregistrée

- Le trait **n'a pas à être déterministe**. Une main ne l'est pas, ni un runtime OpenXR.
- **Ce qu'il rend l'est par construction** : les échantillons vont dans le message
  (`ClientMessage::Ahead`, étendu, §3.1). Le runtime les reçoit et les applique au pas
  suivant. Ils sont enregistrés tels que reçus, avec l'instant du pas qui les applique,
  sur un canal MCAP du même enregistrement que les pas (règle « traces en MCAP »).
- **Le rejeu ne rappelle jamais le trait.** Il relit les échantillons. Le runtime en tire le
  même besoin, puis il quantifie une seule fois : `AheadUse` aujourd'hui, `Need.ahead[K]`
  après la PR B2 du budget. Le rejeu de `decide` reste exact, comme aujourd'hui (G4 du
  budget).
- Les écarts flottants entre cibles (natif, wasm) restent possibles **avant** la
  quantification, comme le dit G4 du budget. Ils n'atteignent pas le rejeu.
- La génération et `motion` sont enregistrés avec les échantillons. Un rejeu sait donc
  quand la conflation a sauté une lecture ou une traversée, et il la saute de même.

### 1.7 La conflation de la caméra : ne recalculer que si cela sert

**Définition.** La conflation de la caméra consiste à ne refaire un calcul sur la pose
prévue que si son résultat peut changer. Elle a deux étages, un de chaque côté de l'API.

**Côté hôte : le compteur de génération.** L'hôte incrémente `generation` seulement quand sa
trajectoire ou sa destination prévue s'écarte de la dernière publiée au-delà de
`ask.tolerance`. tuile compare la génération à la dernière lue. Si elle n'a pas changé, il ne
rappelle pas `forecast`, ne recopie rien et ne renvoie aucun message. C'est ce qui rend
l'exposition quasi gratuite au repos et en vol connu.

- La tolérance est **donnée par tuile, dans les unités de l'hôte**, parce que l'hôte ne
  connaît pas l'arbre. tuile la dérive de son besoin présent : au sol, `κ · s_n` (la
  fraction `κ` du côté `s_n` d'une tuile au niveau voulu sous l'œil) ; en angle,
  `κ · s_n / d` (`d` la distance à ce sol) ; en hauteur, `ln(1 + m)` avec
  `m = level_margin_permille` (15 %, la marge entre niveaux de la spécification,
  `spec.rs:358-361` **[code]**). En deçà, un écart ne peut ni changer la tuile sous l'œil ni
  faire passer un niveau sa marge **[dérivé]**. `κ = 1/4` est **[supposé]**, et c'est le seul
  réglage. Il est dans la spécification et le démonstrateur le balaie (§10).
- Pour un hôte exact, la trajectoire est la même d'une image à l'autre : la génération ne
  change qu'au début d'un vol, à son interruption ou à un nouveau `goto`.
- Pour un hôte interactif, une trajectoire exprimée en **instants absolus** ne change pas
  tant que la vitesse estimée ne change pas : un panoramique régulier garde sa génération.
  Ce qui la fait changer, ce sont une accélération, un virage, un arrêt.

**Côté tuile : le critère de refaire la traversée.** À chaque nouvelle génération, le runtime
calcule une **clé** par échantillon utile : la tuile de sol sous l'œil prévu et le niveau
voulu sous lui. C'est la clé de `Neighbourhood::update` aujourd'hui **[code]**, en O(K), sans
traversée. Il ne traverse l'arbre sur les poses prévues que si une clé a changé. Le critère
porte sur **l'empreinte en tuiles et le niveau voulu**, jamais sur un seuil de pose brute :
deux poses distantes de 100 m au-dessus de l'océan à 10 000 km ont la même empreinte, deux
poses à 1 m l'une de l'autre au ras du sol ne l'ont pas.

**Règles.**

1. **Au repos, rien n'est recalculé.** La génération ne bouge pas, aucun message ne part,
   aucune traversée n'a lieu. La passe tous les `AHEAD_EVERY = 4` pas d'aujourd'hui
   (`runtime.rs:371`) ne sert qu'à suivre la résidence ; elle disparaît, et une arrivée
   de tuile met à jour la sélection prévue au même titre que la présente (I6, delta).
2. **Le dernier forecast gagne.** Une génération nouvelle remplace l'ancienne en entier.
   Les voulus en file qui n'y sont plus sont retirés, et les chargements en vol vont à leur
   terme, comme au §2.5.2 du budget.
3. **Cadence maximale en mouvement.** Au plus une traversée prévue toutes les `N_c` images
   (`N_c = 2` proposé, **[supposé]**, à 60 Hz : 30 traversées par seconde au plus). Une
   génération qui arrive plus vite est gardée, et la plus récente est traversée à la
   cadence suivante.
4. **En vol connu, un seul calcul pour toute la trajectoire.** L'hôte exact rend son vol
   entier, ou jusqu'à `until`. tuile le traverse une fois, sur des poses espacées par la
   clé : une nouvelle pose n'est retenue que si sa clé diffère de la précédente. Il en
   garde la liste des tuiles avec leur instant de premier usage. À chaque pas, il ne fait
   que faire glisser la fenêtre `[now + L, now + H]` sur cette liste, en O(Δ). Le vol n'est
   retraversé que si la génération change.
5. **L'avance.** `until = now + H + H/2` : l'hôte interactif publie la trajectoire une
   demi-horizon plus loin que nécessaire, pour que la fenêtre glisse sans qu'il faille
   republier à chaque image. Même à vitesse constante, il y a donc au plus une génération
   toutes les `H/2`.

**Effet sur les 2 ms.** Le coût **moyen** du forecast devient quasi nul au repos et en vol
connu : une lecture de génération côté hôte, rien côté tuile. Le coût du **pire pas**, celui
où la clé change, est une traversée sur les poses prévues, bornée par le §A (≤ 0,5 ms) et
par la cadence. Aujourd'hui, cette traversée a lieu à chaque pas en mouvement (§A). Le
démonstrateur mesure les deux, par scénario (§10).

**Test de conformité** (§4.6, n° 9) : un forecast dont la génération ne change pas ne coûte
aucune traversée. On le vérifie par le compteur de passes du runtime, sur 1 000 pas au repos
et pendant un Glide.

### 1.8 Rapide et exact en interactif

**Ce qui rend l'interactif difficile.**

- **L'intention n'est pas observable.** Un filtre ne voit que la pose, jamais le doigt.
  Le début d'un geste est donc vu en retard : `T_détection`, les quelques images qu'il faut
  à `MotionModel::moving()` pour passer à vrai (M1 du budget).
- **Les rotations brusques et les demi-tours** violent le modèle à vitesse constante. Après
  le retournement, l'erreur croît comme la vitesse fois l'horizon, jusqu'à ce que le
  filtre ait vu assez d'images (Azuma & Bishop 1995) **[lit.]**.
- **Le retard des capteurs et des entrées.** Un événement de pointeur arrive une à deux
  images après le geste ; un casque échantillonne à haute fréquence mais livre avec un
  délai. Le forecast ne connaît que l'instant où l'hôte a reçu la pose.
- **L'inertie aide.** Après un lancer, la glissade suit une loi connue (`Inertia`,
  `coast_s = 0,35 s`, `inertia.rs:32` **[code]**) : c'est un vol connu, exact.

**Ce que le contrôleur sait déjà, et que le filtre ne devrait pas réestimer.** Le
`CameraController` n'est pas une boîte noire : il **amortit vers une cible connue**
(`update(1 − e^(−dt/SETTLE_SECONDS))`, position interpolée et orientation par slerp,
`lib.rs:1039-1066` **[code]**). Tant qu'aucune entrée nouvelle n'arrive, la trajectoire
jusqu'à la cible est **exacte** et calculable en forme fermée : c'est une décroissance
exponentielle vers `target`. La destination est la cible. La meilleure implémentation
interactive est donc une composition :

```
First( Glide | bande | glissade              (exact, σ = 0)
     , amortissement vers la cible           (exact jusqu'à la prochaine entrée)
       ⊕ incertitude du geste en cours       (σ du filtre sur le taux de la cible)
     , MotionModel                           (σ du filtre, repli) )
```

Le filtre n'estime plus que ce qui est inconnu : **le taux du geste**, appliqué à la cible,
et non plus la pose amortie, que le contrôleur connaît. C'est le principal gain de
précision attendu ; il est **[supposé]** jusqu'à la mesure.

**Estimateurs établis, sur des crates établis.** Le filtre existant (Kalman à vitesse
constante par canal, taux de Singer, `nalgebra` 2×2) reste la référence. Deux variantes sont
à **mesurer** avant toute adoption, toutes deux sur `nalgebra` ou `glam`, sans noyau maison :
le lissage exponentiel double de LaViola (2003), deux EMA, moins cher et comparable sur
100 ms **[lit.]** ; et un modèle de Singer à accélération corrélée, qui est déjà le
`rate_decay_s`. Aucun n'est adopté sans battre la référence sur le corpus du §10.

**Ce que le filtre affirme aujourd'hui** **[dérivé]** des paramètres par défaut
(`q = 0,5`, terme de processus seul, `σ(τ) = √(q τ³/3)`), avant toute mesure :

| Horizon | σ au sol (en hauteurs `h`) | σ en cap et tangage |
|---|---|---|
| 0,2 s | 0,04 h | 2,1° |
| 0,5 s | 0,14 h | 8,3° |
| 1 s | 0,41 h | 23° |
| 2 s | 1,15 h | 66° |

Ce sont des **affirmations** du filtre, pas des précisions. **La précision attendue par
horizon n'est pas connue sur un globe** (§6). Elle se mesure ainsi :

- **Corpus.** On rejoue des sessions réelles enregistrées en MCAP (bandes `tuile-tape` et
  pas enregistrés), en mode « comme une main ». Ce mode rejoue les poses sans exposer leur
  futur au forecast (le §5.4 du budget le demande déjà). S'y ajoutent les scénarios de
  gestes scriptés du §10.
- **Métriques par horizon et par canal** : l'erreur médiane et p90 (déjà dans
  `PredictionErrors`) ; la couverture à 1σ et 2σ, le NEES (§3.4) ; et surtout le **gain
  sur la caméra immobile**, le rapport d'erreur forecast/immobile. L'horizon où ce gain
  passe sous 1 fixe `H` (§8 du budget).
- **Acceptation** : à chaque horizon `≤ H`, l'erreur p50 est inférieure à celle de
  « immobile », la couverture à 2σ vaut 95 % ± 5 points, et le coût d'exposition respecte
  le §1.5. Un estimateur plus exact mais trop lent échoue.

**Comment `σ` est exposé pour que tuile s'en serve.** Par échantillon, 5 × f32 dans le
repère de la pose présente (§1.3). tuile en tire l'élargissement des vues prévues
(`z · σ_ang`) et les `P_k` du budget (§2.2 a et §2.3 du budget), sans rien y changer.

---

## 2. Qui l'implémente

### 2.1 Le principe : l'hôte, avec des défauts fournis par tuile

Le trait est **implémenté par le système de caméra de l'hôte** et **exposé** à tuile. C'est
le sens de la demande : celui qui possède la caméra est le mieux placé pour dire où elle va.
Un film connaît sa trajectoire, un simulateur son modèle, un casque son capteur. Un moteur
de jeu sait que sa caméra suit un personnage dont il connaît la vitesse.

tuile fournit trois implémentations, pour qu'aucun hôte n'ait à écrire de filtre :

| Implémentation | Où | Répond | Dispersion | Poids |
|---|---|---|---|---|
| `Still` | `tuile-core` | la pose présente, à tous les instants | 0 (§2.4) ; aucune hypothèse avant la première pose | 1000 |
| `Path<F: Fn(Micros) -> Option<Pose>>` | `tuile-core` | `F(at)` | 0 (exact) | 1000 ; interruptible : deux hypothèses, `e^(−(at−now)/T_i)` et le reste |
| `MotionForecast` | `tuile-camera` | la moyenne du filtre (`MotionModel::predict`, inchangé) | `σ(τ)` du filtre | 1000 (l'incertitude est dans `σ`) |

Et une composition, qui n'est pas un interrupteur mais une règle sur les données :
`First(a, b)` prend les échantillons de `a`, puis complète par `b` les instants que `a` ne
couvre pas. C'est exactement la chaîne de priorité de `views_ahead` d'aujourd'hui, écrite
une seule fois au lieu d'être cachée dans le viewer.

### 2.2 Les trois cas de la demande

- **Hôte sans prédicteur.** Il n'implémente rien, et tuile prend `MotionForecast` nourri
  de la pose présente : c'est le comportement d'aujourd'hui. `Still` ne sert que lorsque
  même la pose n'est pas connue (première image) : il rend la pose présente, ce qui ramène
  le besoin décalé au besoin présent (`L` effectif = 0).
- **Hôte à trajectoire connue** : film, Glide, rejeu de bande, glissade. `Path` donne un
  forecast **exact**, à dispersion nulle. Le film et le rejeu ont un poids de 1000 :
  rien ne les interrompt (aujourd'hui `Future::Certain`, `predict.rs:647`). Le Glide et la
  glissade ont deux hypothèses : « le vol continue », de poids `e^(−τ/T_i)`, et « la caméra s'arrête où elle est », du reste. Une main peut les
  interrompre (aujourd'hui
  `Future::Known`, `p = decay(τ, interrupt)`, `predict.rs:902`). Une fois interrompus, ils
  ne disent plus rien, et le `First` passe la main au filtre.
- **Hôte interactif** : inertie, Kalman. `MotionForecast` rend la moyenne et `σ`. Un hôte
  qui a mieux (le contrôleur de gestes d'un moteur de jeu, la prédiction de pointeur de la
  plateforme, §5) l'implémente lui-même et rend son propre `σ`.

### 2.3 Ce qui bouge par rapport à #282, et ce qui disparaît

Le principe : **rien de nouveau dans le filtre**. On déplace des responsabilités et on
retire des chemins parallèles.

| Aujourd'hui (#282, 99d90958) | Devient | Pourquoi |
|---|---|---|
| `Future { Known, Certain, Interactive }` (`predict.rs:642-650`) | **disparaît** : `Path` interruptible, `Path` sûr, `MotionForecast` | trois implémentations du même trait |
| `views_ahead` et `Futures` du viewer (`predicting.rs:204-257`) | **disparaît** : un `First(hôte, First(rejeu, First(glide, First(glissade, filtre))))` construit une fois | la chaîne de priorité devient une donnée |
| `PreloadView::ahead`, `Priority::Ahead` (`frame_host.rs:136-175`) et le rapprochement à ±25 % | **remplacés** : `FrameHost` gagne une méthode `forecast` par défaut vide (0 échantillon = « rien de mieux que le viewer ») | l'hôte répond aux instants de tuile, sans rééchantillonnage |
| `PredictConfig::horizons_s` | **déplacé** vers la spécification du budget (`L` et les `τ_k`) | c'est tuile qui sait quand il doit décider ; **décision D-F2** |
| `p = e^(−τ/λ)`, `PredictConfig::laplace_s` | **disparaît**, dans la PR B2 du budget et pas avant | l'incertitude passe par `σ` ; c'est déjà la mutation de T2 du budget |
| `interrupt_s` | **reste**, comme paramètre de `Path` interruptible | c'est le poids d'une hypothèse, pas une dispersion |
| `AheadView { view, in_us, p_permille }` | `issued_at` + échantillons `(at − issued_at, pose, lens, σ, poids)` | instants vérifiables, `σ` qui voyage |
| `Predictor` : modèle + erreurs mesurées + source | `MotionForecast` (le modèle) d'un côté ; **la mesure des erreurs devient générique**, appliquée à tout forecast (§3.4) | un hôte qui fournit son forecast doit être mesuré comme le filtre |
| `Reach` (`protocol.rs:165-171`) | **inchangé** | c'est l'enveloppe des gestes (P8 du budget), pas un forecast |
| `AheadView::certain(v, order)` de `tuile-bake` | `Path` sur les instants réels des images du film | les échéances deviennent vraies (constat 5) |
| `Vec` alloué à chaque image | tableau fourni par tuile, réutilisé | §1.5 |

Le filtre (`Channel`, `MotionModel`, ses `q` et `r`, le modèle de Singer) ne change pas d'une
ligne. Les messages envoyés ne changent que par leurs instants. La PR 0 (§7) le prouve par des
condensés inchangés ; les échéances ne changent qu'avec C2.

### 2.4 La dispersion « de repos »

Une caméra immobile a-t-elle `σ = 0` ? Non : une main peut toujours bouger. Mais ce risque
est couvert par le **plancher atteignable** (P8 du budget, `Reach`), pas par le forecast.
`Still` rend donc la pose présente avec `σ = 0` et un poids de 1000.
Le plancher fait le reste. C'est exactement le partage voulu par le budget : « ce qu'aucune
prédiction ne voit venir » appartient à `C_plancher`. Si on gonflait `σ` au repos, on
compterait deux fois le même risque.

---

## 3. Comment tuile le consomme, sans changer le plan préchargement et budget

### 3.1 Le chemin

```
hôte (CameraForecast)            liaison (viewer, web, bevy, bake)          runtime (tuile-core)            décideur (tuile-budget)
  generation(ask)        ──►  ask.at = [now+L, now+τ_1 … now+τ_K]   ──►  ClientMessage::Forecast {      ──►  Need (quantifié, enregistré)
  forecast(ask, out)          une fois par image ; forecast() et         issued_at, generation, motion,
  (si la génération change)   envoi seulement si la génération change    hypotheses, reach, model }
                                                                       enregistré tel que reçu ;
                                                                       clé de conflation → traversée ?
```

- Les instants demandés sont `now + L` (le besoin décalé, §2.5 du budget) et `now + τ_k`
  pour `τ_k > L` (le préchargement, §2.1 du budget). C'est le partage du temps du §2.5.4 du
  budget : `[0, L]` au besoin, `]L, H]` au préchargement. Aucun instant n'est compté deux
  fois.
- Tant que la PR N1 du budget n'est pas livrée, `L = 0` : on ne demande que les `τ_k`. Le
  message est alors le même qu'aujourd'hui, au format des instants près.

### 3.2 Les rôles restent ceux du budget

- **Le besoin à `t + L`** lit l'échantillon à `now + L` comme `p_L` (§2.5.1 du budget). Il
  n'y lit que la moyenne : ses voulus sont certains à leur échéance (`P = 1`), par
  définition du besoin. Le besoin prend l'hypothèse de plus grand poids à `now + L` ; ni `σ` ni le poids n'entrent dans le besoin.
- **Le préchargement par valeur attendue** lit les échantillons à `τ_k > L` comme
  `μ_k, σ_k` (§2.1–§2.3 du budget). Avec plusieurs hypothèses, il somme : `P_k = Σ_j w_jk × P^vis_jk × P^niv_jk`. Pour un estimateur seul, `w = 1` ; pour un chemin exact,
  `σ = 0`. Aucune incertitude n'est donc comptée deux fois **[dérivé]**. C'est le seul
  ajout à la formule du budget, et il est neutre pour `MotionForecast`.
- **La garde ne bouge pas.** Le dessin et la protection `Π` restent à la pose **présente**
  (la caméra que l'hôte fait dessiner à cette image). `Π` s'agrandit de la protection du
  besoin à `p_L`, sans jamais rétrécir (§2.5.3 du budget). Le forecast ne retire rien de
  protégé. Une erreur de forecast coûte des chargements, jamais un trou (G1 du budget).
- **La conflation** (§2.5.2 du budget) est celle de la caméra, au §1.7 : génération côté
  hôte, clé d'empreinte côté tuile. La traversée du besoin décalé n'est refaite que si `p_L`
  change la tuile sous l'œil prévu ou le niveau voulu. Un forecast qui tremble ne coûte
  donc pas une traversée par image.

### 3.3 Absent, périmé, faux

| Cas | Détection (runtime, au pas) | Effet sur le besoin à `t + L` | Effet sur le préchargement |
|---|---|---|---|
| **Absent** (0 échantillon, ou aucun message) | rien reçu pour cet instant | `p_L` = pose présente (`L` effectif = 0) : le besoin d'aujourd'hui | aucun `C_prévu` ; il reste `C_plancher` (P8) |
| **Partiel** (le film finit, le Glide se termine) | moins d'échantillons que d'instants | idem si `now + L` manque | seuls les instants couverts comptent |
| **Périmé** | `at ≤ now_pas` pour un échantillon ; ou `now_pas − issued_at > A_max` pour le message | l'échantillon passé est ignoré ; un message trop vieux compte comme absent | idem |
| **Invalide** (NaN, infini, axes non unitaires, `up ∥ direction`, `at` non croissants) | validation à la réception, comme `Specify` (`protocol.rs:112-117`) **[code]** | le message est refusé en entier, celui d'avant reste jusqu'à `A_max` | idem |
| **Faux** (demi-tour brusque, hôte mal calibré) | pas au pas : mesuré à l'échéance (§3.4) | des chargements à côté, un raffinement en retard d'environ `L`, **jamais un trou** (§2.5.5 du budget) | des chargements de réservoir gaspillés (G1, G2 du budget) |

`A_max`, l'âge maximal d'un message, est un nombre de la spécification. Je propose deux
périodes d'image mesurées, `2 Δ` (`Δ` est déjà enregistré, §2.1 du budget) **[supposé]** :
au-delà, la caméra a reçu au moins une image de plus que le forecast ne connaît. Ce n'est
pas un interrupteur : un message âgé n'est pas éteint, il est traité comme absent, ce qui
est son sens.

Aucune correction du forecast par tuile. Un forecast faux se mesure ; il n'est ni redressé ni
gonflé en silence, ce qui cacherait un défaut de l'hôte. Le seul recalage envisagé est celui
du `q` du filtre de tuile, d'après le NIS mesuré (risque 1 du budget), et il est fait par une
PR, pas à chaud.

### 3.4 La mesure, la même pour tous les forecasts

La mesure des erreurs de #282 (`PredictionErrors`, `predict.rs:741-778`) est sortie du
`Predictor` et appliquée à **tout** forecast, quel qu'en soit l'auteur. Chaque échantillon
est gardé jusqu'à son instant, puis comparé à la pose effectivement affichée. On rapporte
par état déclaré (`motion`), par hôte et par horizon :

- les erreurs d'aujourd'hui (sol, cap, tangage, rapport de hauteur), en histogrammes ;
- **la couverture** : la part des poses affichées qui tombent à moins de `1σ` et de `2σ`
  de la moyenne, canal par canal. Une gaussienne calibrée donne 68 % et 95 %. C'est le
  diagramme de fiabilité demandé par M1 du budget ;
- **le NEES** (erreur normalisée par `σ`), dont la moyenne doit valoir 5 (le nombre de
  canaux) pour un forecast cohérent, d'après le test de cohérence de Bar-Shalom **[lit.]** ;
- pour un forecast à `σ = 0`, l'erreur doit être nulle à la précision de l'interpolation
  près. Un écart y est un défaut, pas une incertitude : c'est ce qui trouverait le décalage
  d'instants du constat 4.

Ces mesures vont sur le board et sur Prometheus (`tuile_forecast_*`), et elles n'entrent
dans aucune décision.

### 3.5 Ce qui change dans les types : une prévision ou plusieurs

Le contrat de tuile évolue légèrement. Aujourd'hui, il accepte une liste de vues à des
délais, chacune avec une probabilité. Demain, il accepte **une ou plusieurs hypothèses**,
chacune une trajectoire aux instants absolus avec `σ` et poids par échantillon, sous un
numéro de génération. Une seule hypothèse de poids 1000 à `σ = 0` exprime exactement ce
qu'exprime l'`Ahead` d'aujourd'hui.

**Le message.**

```rust
// tuile-core/src/protocol.rs
ClientMessage::Forecast {
    issued_at: Micros,           // the host's `now`, session clock
    generation: u64,             // the host's; unchanged ⇒ never sent again
    motion: Motion,              // declared, recorded, read by measures only
    hypotheses: Vec<Hypothesis>, // ≤ 4; each ≤ 64 ForecastSample, ascending `at`
    envelope: Envelope,          // `Reach` extended: the camera's declared limits (§9.2)
    model: u64,                  // unchanged (the predictor's numbers' digest)
}
```

Le `Vec` du message est alloué par la liaison **une fois par génération**, et non à chaque
image. En processus, on pourrait recycler le tampon par le canal de retour ; on le mesure
avant de compliquer (§6).

**Compatibilité descendante.** Un hôte sans prévision **ne fait rien** et continue de
marcher. La liaison du viewer, de bevy ou du web lui fournit le `MotionForecast` de tuile,
nourri de la pose présente : c'est exactement le comportement d'aujourd'hui. Un hôte qui
n'envoie aucun message `Forecast` a le besoin présent et le plancher, comme un hôte sans
`Ahead` aujourd'hui. Aucun drapeau, aucun mode : l'absence de forecast est un cas des
données (§3.3).

**Types et fichiers touchés.**

| Fichier | Type ou élément | Changement | Cassant ? |
|---|---|---|---|
| `tuile-core/src/forecast.rs` (nouveau) | `CameraForecast`, `ForecastAsk`, `Forecast`, `Hypothesis`, `ForecastSample`, `Spread`, `Lens`, `Motion`, `Tolerance`, `Still`, `Path`, `First`, `conformance` | ajout | non |
| `tuile-core/src/protocol.rs:107-171` | `ClientMessage::Ahead`, `AheadView`, `Reach` | **remplacés** par `ClientMessage::Forecast` ; `Reach` devient `Envelope`, qui l'étend | **oui** pour qui construit le message à la main (bake, tests, hôtes en aval) |
| `tuile-core/src/runtime.rs:1112-1131`, `1240-1259` | champs `ahead`, `ahead_when`, `ahead_moved`, `AHEAD_EVERY` | remplacés par un état de forecast préalloué : génération, hypothèses, clés, pas de la dernière traversée | non (privé) |
| `tuile-core/src/runtime.rs:1672-1737` | l'anneau et `AheadUse` | lus depuis les hypothèses ; `AheadUse` **inchangé** jusqu'à B2 du budget | non |
| `tuile-budget/src/need.rs:551-575` | `Need.ahead: Option<AheadUse>` | **inchangé** ici (B2 le change en `[AheadSample; K]`) | non |
| `tuile-budget/src/spec.rs` | `Params` | ajout de `forecast_tolerance_permille` (`κ`), `forecast_walk_every` (`N_c`), `forecast_max_age` (`A_max`), puis des instants `L` et `τ_k` (§9) | non (ajout ; le condensé de spécification change) |
| `tuile-web/src/wire.rs:95-120`, `216+` | tag 7 | nouvel encodage ; les deux moitiés sont le même module wasm, donc sans version (`wire.rs:16-17`) | non (privé) |
| `tuile-camera/src/predict.rs:642-928` | `Future`, `Ahead`, `Source`, `Predictor::frame` | `MotionForecast: CameraForecast` ; `Predictor` garde la mesure des erreurs ; `Future` et `Ahead` disparaissent | **oui** (API publique de `tuile-camera`) |
| `tuile-viewer/src/frame_host.rs:130-175` | `Priority::Ahead`, `PreloadView::ahead` | `FrameHost::forecast`, vide par défaut ; `PreloadView::ahead` gardé une version mineure comme adaptateur `#[deprecated]` vers un `Path` | oui, à la version suivante |
| `tuile-viewer/src/app/predicting.rs` | `Futures`, `views_ahead` | la composition `First(...)` | non (privé) |
| `tuile-bevy/src/camera.rs:124-154` | ressource `Forecast` | ajout | non |
| `tuile-bake/src/session.rs:938-956` | `Session::ahead` | un `Path` sur les instants du film | non (même signature) |

**Versionnement.** tuile est en 0.x : un changement cassant de l'API publique prend la
version mineure suivante, avec une entrée au journal des changements qui donne la
correspondance `AheadView` → `ForecastSample`. Le fil du worker n'a pas de version et n'en a
pas besoin, puisque ses deux moitiés sortent d'une seule compilation. Le futur `WsStream`
(planifié, `01-architecture.md:182`) devra négocier une version de protocole à l'ouverture
de session, parce que ses deux bouts ne sont pas compilés ensemble. C'est à inscrire dans
D-T1 (`12-crate-server.md`) ; ce n'est pas propre au forecast.

**Les liaisons.**

- **En processus** (`InProcessStream`) : le message traverse le canal sans sérialisation.
  Le seul coût est l'allocation par génération.
- **Worker web** : l'encodage binaire passe de 9 nombres par vue à 21 par échantillon, plus
  `issued_at`, la génération et `motion`. Un message de 16 échantillons pèse environ
  2,7 Kio, envoyé une fois par génération et non à chaque image.
- **WebSocket (glb sur le fil)** : la géométrie va du serveur au client, le forecast du
  client au serveur. Ce sont deux sens différents, et le glb n'est pas touché. Le message
  est le même que dans le worker, avec la conversion d'horloge du §1.4.
- **HTTP pull statique** : le `GeometryServer` tourne chez le consommateur
  (`01-architecture.md:181`), c'est donc le cas « en processus ». Le forecast n'atteint
  jamais le CDN.
- **wasm** : rien d'exotique à la frontière ; un `Float64Array` par hypothèse (§4.2).

---

## 4. Compatibilité

### 4.1 Natif

Le viewer construit sa composition (`First(...)`, §2.3) une fois par session. Un hôte
embarqué (`FrameHost`) qui implémente `forecast` passe devant. C'est la même porte pour
`tuile-wgpu-viewer`, le viewer bundle et un hôte de rejeu.

### 4.2 wasm et web

- **Aucun type exotique à la frontière.** Un échantillon tient en 21 nombres : `at` (u64,
  passé en f64 de µs, exact jusqu'à 2^53 µs, soit 285 ans), la position, la direction et
  `up` (9 × f64), la lentille (5 × f64, `fovy` ou les quatre angles), `σ` (5 × f32) et la
  poids (u16). Un `Float64Array` de `21` cases par échantillon traverse `wasm-bindgen` tel quel. Seuls
  `wasm-bindgen` et `js-sys` sont en jeu (règle 8).
- **Un hôte JS qui possède sa caméra** (la façade THREE.js) fournit une fonction
  `forecast(nowUs, atUs: Float64Array, out: Float64Array) => number`. L'adaptateur Rust
  l'appelle sur le fil principal, de façon synchrone, une fois par image, et l'enveloppe
  dans le trait. Le worker ne voit que le message. Aucun appel ne traverse le worker
  (le serveur de géométrie y tourne, `tuile-web`).
- **L'encodage du fil** (`tuile-web/src/wire.rs:104-120`) **[code]** gagne `issued_at`, `σ`
  le poids, la génération et `motion`. C'est la même passe que celle que demande B2 du budget pour `σ`.
- **La même chose qu'en natif** : le code de composition est celui de `tuile-camera` et de
  `tuile-core`, compilé pour les deux (règle « web = desktop »).

### 4.3 Bevy

`tuile-bevy` gagne une ressource `Forecast(Box<dyn CameraForecast + Send + Sync>)`, que le
système de caméra du jeu remplit ou remplace. Sans elle, c'est `MotionForecast` sur la
`TuileCamera`. Cela comble le constat 6 sans ajouter de `Priority::Ahead` : le forecast
remplace la porte manquante plutôt que d'en recopier une.

### 4.4 Casque et stéréo

OpenXR fait déjà exactement cela. `xrWaitFrame` donne le `predictedDisplayTime` de l'image à
venir, et `xrLocateViews` ou `xrLocateSpace` rendent la pose prévue **pour un instant
donné** **[lit.]**. Un hôte casque implémente donc le trait en convertissant `at` vers
`XrTime` et en localisant l'espace `VIEW`. C'est la même interface, et la pose affichée de
l'image présente (`at = predictedDisplayTime`) en est un cas particulier.

Deux limites, **[supposé]** et à mesurer :

- les runtimes ne garantissent la prédiction que sur quelques dizaines de millisecondes.
  Au-delà, l'hôte compose avec `MotionForecast` (`First(openxr_jusqu_a_50ms, filtre)`) ;
- OpenXR ne rend pas d'écart type, seulement des drapeaux de validité. L'hôte casque rend
  `σ = 0` dans la fenêtre valide, puis le `σ` du filtre au-delà.

La tête porte le forecast ; les yeux sont tirés par la `Rig` (§1.3).

### 4.5 Film, ferme, serveur

- **Film** : `Path` sur la trajectoire du film. Le forecast est exact (`σ = 0`, poids 1000),
  sur l'horloge du film, que l'hôte choisit pour la session. Le bake passe des faux
  instants (`order`) aux vrais (constat 5). Le bake converge en plusieurs pas par image :
  si l'horloge du film reste figée pendant la convergence, les `τ` restent constants et
  l'ordre est celui d'aujourd'hui. **À vérifier** dans `tuile-bake` avant de changer les
  instants (§8).
- **`tuile-server`** : un consommateur distant implémente le trait chez lui et envoie le même
  message par WebSocket. Le serveur ne voit que des données, et la règle 6 (pas d'USD
  dans le serveur) n'est pas concernée. Le format du fil est le même qu'en worker.
- **Hydra et USD** : la caméra USD d'un rendu est une fonction du temps (`UsdGeomCamera`
  aux codes de temps). C'est un `Path`, exact, par la même interface.

### 4.6 La suite de conformité

`tuile_core::forecast::conformance` est un module de tests que **tout hôte exécute** sur son
implémentation (`fn check(f: &mut impl CameraForecast, scenario: &Scenario) -> Report`).
L'hôte le lance dans ses propres tests, natif et wasm. Les contrôles :

1. **Forme** : au plus 4 hypothèses de 64 échantillons ; `at` strictement croissants dans
   chaque hypothèse, après `now` et avant `until` ; les instants demandés sont couverts
   jusqu'à la fin connue du vol.
2. **Valeurs** : positions finies et à une distance plausible du centre de la Terre ;
   `direction` et `up` unitaires à 1e-9 près et non colinéaires ; lentille dans ses bornes ;
   `σ ≥ 0` et finis ; à chaque instant, la somme des poids ≤ 1000.
3. **Sans allocation** : un allocateur de comptage, en test, compte zéro allocation sur
   1 000 appels après le premier.
4. **Temps d'appel de l'API, et l'hôte lent échoue.** On mesure **l'appel seul**,
   `generation` puis `forecast`, jamais une traversée : la suite ne lie aucun arbre. On fait
   10 000 appels à `K = 16`, en mouvement scripté, après un échauffement. Le test **échoue**
   si le p99 dépasse la cible du §1.5 (10 µs pour `forecast`, 2 µs pour `generation`, en
   natif ; 30 µs en wasm). Pour ne pas dépendre de la machine, la cible est mise à
   l'échelle par un étalon mesuré dans le même processus : le temps de la référence
   `MotionForecast` sur le même scénario. L'hôte échoue s'il est plus lent que
   `max(cible, 4 × étalon)` **[supposé]**. Le rapport donne p50, p99 et max. Le test est
   étiqueté lent, mais il tourne en CI.
5. **Continuité à l'instant présent** : pour `at` → `now`, l'échantillon tend vers la pose
   affichée de l'image présente.
6. **Exactitude** (`motion = Known`) : sur le scénario, l'erreur à l'échéance est nulle à
   l'interpolation près, et `σ = 0`. La trajectoire couvre le vol entier, ou jusqu'à
   `until`, en un seul appel.
7. **Calibration** (`motion = Interactive`) : sur un corpus de mouvements
   scénarisés et bruités à graine (celui de T2 du budget), la couverture à `1σ` et `2σ` est
   dans ±5 points de 68 % et 95 %, et le NEES moyen dans l'intervalle de confiance à 95 %
   **[lit.]**. Le contrôle échoue si l'hôte rend `σ = 0` pour un mouvement imprévu.
8. **Rejeu** : les échantillons d'une session, enregistrés puis relus, redonnent le même
   `Need` au condensé près, sans rappeler le trait.
9. **Conflation côté hôte** : au repos, et pendant un vol connu non interrompu, la
   génération ne change pas sur 1 000 images. Pendant un panoramique à vitesse constante,
   elle change au plus une fois par `H/2` (§1.7, règle 5). Après un demi-tour, elle change
   dans les deux images qui suivent.
10. **Un forecast inchangé ne coûte aucune traversée.** Ce contrôle est le seul qui lie un
    runtime : celui du simulateur, qui compte ses passes. Il vérifie que la génération
    inchangée entraîne zéro appel à `forecast` et zéro traversée prévue sur 1 000 pas, au
    repos et en Glide. Il vérifie aussi qu'une génération nouvelle de même clé entraîne
    zéro traversée.
11. **État déclaré** : `motion` vaut `Still` sans hypothèse, ou une hypothèse constante ;
    `Known` donne `σ = 0` ; `Interactive` donne `σ > 0` au-delà du premier instant.

Les implémentations de tuile (`Still`, `Path`, `MotionForecast`, `First`) sont les premières
à passer cette suite. L'hôte de démonstration de chaque coquille (viewer, web, bevy) la
passe en CI.

---

## 5. Références

### 5.1 Prédiction de pose et compensation du retard

- Azuma & Bishop, *Improving Static and Dynamic Registration in an Optical See-through
  HMD*, SIGGRAPH 1994 : la prédiction de la pose de la tête pour compenser la latence de
  bout en bout. C'est la source principale de la réduction de l'erreur dynamique, sur des
  horizons de l'ordre de 100 ms **[lit.]**. Les facteurs chiffrés sont à relire dans
  l'article : je ne les cite pas de mémoire.
- Azuma & Bishop, *A Frequency-Domain Analysis of Head-Motion Prediction*, SIGGRAPH 1995 :
  l'erreur d'un prédicteur croît avec l'horizon et avec la bande de fréquence du mouvement.
  C'est le fondement de « plus `L` est grand, plus la prédiction est fausse » (§2.5.5 du
  budget) **[lit.]**.
- LaViola, *Double Exponential Smoothing: An Alternative to Kalman Filter-Based Predictive
  Tracking*, EGVE 2003 : à précision comparable, un lissage exponentiel double est bien
  moins coûteux qu'un Kalman étendu sur des horizons de l'ordre de 100 ms **[lit.]**. Ce
  n'est pas une raison de changer de filtre (le nôtre est linéaire, 2×2 par canal, déjà
  bon marché). C'est un repère de coût pour un hôte qui écrirait le sien.
- van Rhijn, van Liere & Mulder, *An Analysis of Orientation Prediction and Filtering
  Methods for VR/AR*, IEEE VR 2005 : une comparaison des prédicteurs d'orientation ; aucun
  ne domine sur tous les mouvements, ce qui justifie que l'hôte puisse fournir le sien
  **[lit.]**.
- Singer, *Estimating Optimal Tracking Filter Performance for Manned Maneuvering Targets*,
  IEEE TAES 1970 : le taux qui décroît (`rate_decay_s`) du filtre d'aujourd'hui **[lit.]**.
- Bar-Shalom, Li & Kirubarajan, *Estimation with Applications to Tracking and Navigation*,
  Wiley 2001 : le modèle à vitesse constante et bruit d'accélération blanc (le `Q(Δt)` de
  `predict.rs`), et les tests de cohérence NEES et NIS (§3.4) **[lit.]**.
- Qian et al., *Flare: Practical Viewport-Adaptive 360-Degree Video Streaming for Mobile
  Devices*, MobiCom 2018 : l'origine du `λ = 1,9 s` d'aujourd'hui, mesuré sur des têtes et
  non sur un globe (constat 3 du budget) **[lit.]**.
- Gneiting & Raftery, *Strictly Proper Scoring Rules, Prediction, and Estimation*, JASA
  2007 : pourquoi on juge un forecast probabiliste à sa calibration et à un score propre
  (Brier, CRPS), et non à sa seule erreur moyenne **[lit.]**.

### 5.2 Des plateformes qui exposent déjà leur prédiction aux applications

C'est le précédent direct de la demande : celui qui possède l'entrée publie son futur, et
l'application décide à temps.

- **OpenXR 1.0** : `xrWaitFrame` (`predictedDisplayTime`), `xrLocateViews` et
  `xrLocateSpace` à un instant donné **[lit.]**.
- **W3C Pointer Events, niveau 3** : `PointerEvent.getPredictedEvents()` **[lit.]**.
- **UIKit** : `UIEvent.predictedTouches(for:)` **[lit.]**.
- **Android** : `androidx.input.motionprediction.MotionEventPredictor` **[lit.]**.
- Nancel et al., *Next-Point Prediction for Direct Touch Using Finite-Time Derivative
  Estimation*, UIST 2018 : la prédiction de la position d'un doigt pour compenser la
  latence d'un écran tactile **[lit.]**.

### 5.3 Filtrage : un crate établi, jamais un noyau maison

Le filtre existant est écrit sur `nalgebra` (`Matrix2`, `Vector2`) **[code]**, et il le
reste. Les types de la frontière sont `glam::DVec3` et des scalaires. Un hôte qui veut un
filtre plus riche (un UKF sur l'orientation, par exemple) prend un crate établi, ou
`nalgebra` directement ; tuile n'en écrit pas. Le lissage exponentiel double de LaViola
n'est pas un noyau : c'est deux EMA, et ce choix appartient à l'hôte.

---

## 6. Ce qui est supposé, mesuré, inconnu

**Mesuré ou lu [code]** : tout le constat du §0 ; le filtre et ses `σ` ; le format du fil ;
la chaîne de priorité du viewer ; les faux instants du bake ; l'absence de porte dans Bevy.

**Supposé, à mesurer avant de s'y fier :**

- le coût cible de 10 µs et 30 µs (§1.5) : la suite de conformité le mesure ;
- `A_max = 2 Δ` (§3.3) ;
- l'erreur d'horloge par WebSocket, la moitié du RTT, négligeable devant `L` (§1.4) ;
- la dispersion diagonale (§1.3) ;
- la fenêtre de validité de la prédiction OpenXR, et le fait qu'aucun runtime ne rende de
  covariance (§4.4) ;
- qu'un forecast d'hôte bat le filtre de tuile. Rien ne le garantit : la mesure par source
  (§3.4) le dira, hôte par hôte.

**Inconnu :**

- **la précision réelle du filtre sur un globe**, par horizon. Les erreurs sont collectées
  (`PredictionErrors`) mais je ne les ai pas lues. C'est aussi le premier inconnu du budget
  (§8) ;
- **si le roulis manque.** Le filtre n'a pas de canal de roulis. Sur un globe, le roulis est
  nul, mais un casque roule la tête et déplace les coins du frustum. Il faudra peut-être un
  sixième canal, qui ne coûterait qu'un `f32` de plus dans `Spread` ;
- **la cause du décalage d'instants** (constat 4) : est-elle négligeable en processus, et de
  combien est-elle dans le worker web ? Elle n'est pas mesurée ;
- **si le rejeu du runtime enregistre déjà les vues futures** quelque part (constat 7). Je
  ne l'ai pas trouvé, mais je n'ai pas lu tout l'enregistreur MCAP de `tuile-telemetry` ;
- **le comportement de l'horloge du bake pendant la convergence** (§4.5) ;
- **la part du retard visible que le besoin décalé corrige vraiment**, devant celle qui
  vient de la sélection (#323, #333) : le budget pose la même question.
- **le coût réel de la traversée sur les poses prévues**, par scénario : `traversal_seconds`
  ne sépare pas aujourd'hui la passe présente de la passe prévue. Le budget de 0,5 ms au
  pire pas (§A) n'est appuyé sur aucune mesure ;
- **le `decide` d'aujourd'hui** : le seul relevé publié (1,1 ms moyenne immobile, 6,9 ms p99
  en zoom) date d'octobre, sur ce Mac, avant l'I6 incrémental. Il faut un relevé neuf de
  `budget_decide` avant la PR 0 ;
- **les limites réelles des gestes du contrôleur** (panoramique, zoom, rotation), qui
  remplaceraient les `reach_*` supposés : je ne les ai pas dérivées de `drag`, `zoom` et
  `tilt` (`lib.rs:675-850`) ;
- **qui, en aval, utilise `PreloadView::ahead`** ou construit `ClientMessage::Ahead` à la main :
  c'est ce qui décide de la durée de l'adaptateur `#[deprecated]` (§3.5) ;
- **le seuil de l'hôte lent** (4 × l'étalon, §4.6) et `κ = 1/4`, `N_c = 2` : supposés,
  à balayer avec le démonstrateur.

---

## 7. Découpage en PR

Une PR de comportement à la fois, chacune testée à la main par Laurent sur le viewer avant
la suivante (mémoire « un merge, puis Laurent teste »). Chaque test rouge est écrit d'abord
et vu échouer contre la mutation indiquée. Les PR de mesure ne changent aucune décision, et
des condensés inchangés le prouvent.

| PR | Nature | Contenu | Rouges (et mutation) | Avant / après |
|---|---|---|---|---|
| **PR 0, le démonstrateur** (§10) | **mesure seule** | trait et types dans `tuile-core` (`forecast.rs`), `Still`, `Path`, `First` ; `MotionForecast` dans `tuile-camera` (adaptateur sur `MotionModel`, inchangé) ; la chaîne `views_ahead` réécrite en `First(...)`, qui produit **le même** `ClientMessage::Ahead` qu'aujourd'hui ; `issued_at` et l'âge des messages mesurés ; la conflation **comptée et non appliquée** (« traversées évitables ») ; suite de conformité ; surcouche, route et menu ; scénarios scriptés et headless ; métriques et canal MCAP | conformité 1 à 11 (mutations : allouer, `σ ≠ 0` pour un Glide, instants décalés de 16 ms, génération incrémentée à chaque image, hôte artificiellement lent) ; `views_ahead_through_the_trait_send_the_same_messages` | condensés de `budget_sim` et goldens **inchangés** ; le tableau du §10.4 sur `main` (#282) et sur la PR |
| **C1** | comportement | la conflation **appliquée** : la passe sur les poses prévues n'a lieu que si une clé change, à la cadence `N_c` ; `AHEAD_EVERY` disparaît | `an_unchanged_forecast_walks_nothing` (mutation : remettre `ahead_moved = true` à chaque message) ; `a_glide_is_walked_once` | `decide` et traversée p50/p99 par scénario ; trous = 0 ; tuiles sous le besoin non dégradées |
| **C2** | comportement (échéances) | instants absolus sur l'horloge de la session ; `ClientMessage::Forecast` (hypothèses, `σ`, poids, génération) remplace `Ahead` ; le bake passe aux vrais instants du film ; l'horloge web devient obligatoire | `bake_ahead_instants_are_the_films` (mutation : `order`) ; `stale_forecast_is_absent` (mutation : ignorer `A_max`) | `budget_sim` re-blessé, chaque condensé qui bouge expliqué ; un rendu de film ouvert avant et après |
| **C3** | comportement (interactif) | la composition du §1.8 : l'amortissement vers la cible du contrôleur exposé comme trajectoire exacte, le filtre ramené au taux du geste | gain sur « immobile » et calibration du §1.8 sur le corpus (mutation : `MotionModel` seul) | erreur par horizon avant/après, sur le corpus MCAP et les scénarios |
| **F3** | parité | ressource `Forecast` dans `tuile-bevy` ; adaptateur JS pour la façade web ; la suite passée par l'hôte de démonstration de chaque coquille | conformité dans la CI de chaque coquille | même scénario en natif, web et bevy : mêmes échantillons au condensé près |
| *B2 du budget* | *comportement, déjà prévu* | *`σ` et les poids lus par `P_k` ; `laplace_s` disparaît* | *T1, T2, T9 du budget* | *celui du budget* |
| *N1 du budget* | *comportement, déjà prévu* | *l'instant `now + L` est demandé ; le besoin lit `p_L`* | *ceux de N1* | *celui du budget* |

La PR 0 peut partir dès que le document est validé : elle ne change aucune décision, et le
démonstrateur en sort. C1, C2 et C3 sont les comportements propres à ce chantier, un à la
fois. B2 et N1 appartiennent au plan du budget : ils lisent ce forecast et n'en sont pas
changés. Leur ordre reste la décision D1 du budget.

---

## 8. Ce qui ne change pas

Pour lever toute ambiguïté, voici ce que ce chantier ne touche pas, dans le plan du budget
(`budget-preload-by-expected-value.md`) :

- les lois P1–P12, l'ensemble candidat `C_prévu ∪ C_plancher`, le score `V`, `U`, `L` et sa
  quantification (§1 à §3 du budget) ;
- la définition du besoin à `t + L`, sa garde `Π`, sa conflation et la calibration de `L`
  (§2.5 du budget) : ce document en fournit l'entrée `p_L` et nomme sa conflation, il ne
  la redéfinit pas ;
- la loi 8 par Little, la préemption et l'admission (§3 du budget) ;
- les garanties G1 à G4, les tests T1 à T13, le découpage M1, M2, B1, B2, B3, R, N1 et les
  décisions D1 à D3 du budget.

Le seul point de contact est au §3.2 : la somme pondérée des hypothèses dans `P_k`. Elle est
neutre pour une hypothèse unique de poids 1000, c'est-à-dire pour tout ce que le budget
suppose aujourd'hui.

---

## 9. Le lien entre la spécification du budget et celle de la caméra

Le plan du budget n'est pas modifié. Cette section décrit **qui définit quoi**, pour que
chaque nombre n'ait qu'une source de vérité. Elle ne refait pas la spécification du budget.

### 9.1 L'inventaire : aujourd'hui, des nombres en double ou à l'envers

| Grandeur | Défini aujourd'hui dans | Lu par | Constat **[code]** | Source de vérité proposée |
|---|---|---|---|---|
| L'horizon `H` | `Params::horizon` (`spec.rs:397` : 1 s interactif, 2 s FPV, 8 s chemin connu, 10 s film) **et** le plus grand `PredictConfig::horizons_s` (2 s, `predict.rs:275`) | le budget ; le prédicteur | **en double et incohérent** : en interactif, le prédicteur échantillonne jusqu'à 2 s quand le budget a un horizon de 1 s | le budget (`Params::horizon`) |
| Les instants `τ_k` et `L` | `PredictConfig::horizons_s` ; `L` n'existe pas encore | le prédicteur | dans la caméra alors que c'est tuile qui décide | le budget (spécification), transmis par `ForecastAsk` |
| Les gestes les plus rapides (`reach_*`) | `PredictConfig` (3 h/s, ×4/s), **[supposé]** | P8 du budget, par `Reach` | posés dans le prédicteur, sans lien avec le contrôleur qui produit les gestes | la caméra : ses limites réelles, déclarées |
| La glissade (`coast_s`) | `PredictConfig::coast_s`, **recopié dans** `controller.inertia.coast_s` (`predicting.rs:103`) | le contrôleur, le prédicteur | **à l'envers** : la configuration de la prédiction règle la physique de la caméra | la caméra (`Inertia`) ; le forecast la lit |
| L'amortissement (`SETTLE_SECONDS = 0,08 s`) | constante locale du contrôleur (`lib.rs:1027`) | le contrôleur | invisible au forecast, qui en a besoin pour une trajectoire exacte (§1.8) | la caméra, déclarée |
| L'interruption `T_i` (`interrupt_s`) | `PredictConfig` | le prédicteur | c'est une propriété des usagers, par contexte, à mesurer | la caméra, par contexte |
| `q`, `r`, `rate_decay_s` | `PredictConfig` | le filtre | à sa place | la caméra (le filtre) |
| `laplace_s` | `PredictConfig` | le prédicteur | disparaît avec B2 | — |
| La latence de présentation, la période d'image | implicite (`refresh_hz` du viewer) ; `FrameDeadline::time` par image (`deadline.rs:21-31`) | l'exécuteur | deux lectures du même affichage | l'hôte (son affichage), une fois : le forecast la contient (« affichée à `at` », §1.4), `FrameDeadline` en dérive |
| Les altitudes min et max | `CameraController::min_altitude`, `max_altitude` | le contrôleur | l'enveloppe atteignable en dépend (zoom arrière borné) | la caméra, déclarée |
| Le temps de chargement mesuré `F_c` | `TierSample`, puis l'histogramme de M1 du budget | le budget | la caméra n'en sait rien | le budget ; la caméra **peut** le lire (§9.2) |
| Le contexte d'usage | `SpecKind` (`spec.rs:36`) **et** `PredictConfig::for_context(label: &str)` (`predict.rs:143`), reliés par une **chaîne** (`predicting.rs:95-101`) | les deux | un libellé inconnu donne silencieusement les défauts | `SpecKind`, un seul enum, apparié exhaustivement (§9.3) |

### 9.2 Qui lit quoi : une seule source, deux sens de lecture

- **Le budget lit la caméra, sans la redéfinir.** Les limites de la caméra forment une
  **enveloppe** déclarée par elle : vitesse de panoramique au sol, en hauteurs par seconde ;
  zoom arrière maximal ; vitesse de rotation maximale ; altitudes min et max ;
  amortissement ; glissade. Elle voyage avec le forecast, à la place de l'actuel `Reach`
  qu'elle étend. Le runtime la quantifie une fois et elle est enregistrée. P8 du budget
  (`C_plancher`, `v_pan`) la lit au lieu de ses défauts supposés. C'est déjà ce que le
  budget demande : « il faut les prendre au contrôleur de gestes » (§2.2 b et §8 du budget).
- **La caméra lit le budget, sans le redéfinir.** Par `ForecastAsk`, elle reçoit les
  instants (`L`, `τ_k`), l'horizon `until` et la tolérance de conflation. Ce sont des nombres
  de la spécification du budget. Elle n'a plus d'horizons à elle.
- **Le temps de chargement, en lecture seule.** Un hôte peut vouloir régler un vol sur ce
  que le réseau peut suivre : un Glide plus lent si le sol d'arrivée n'est pas encore là.
  Il lit alors `BudgetSignals` (le q50 et le q95 de `F_c`, publiés). Ce n'est **jamais** une
  entrée de `decide` venue de la caméra, et ce n'est pas dans ce chantier. Je le signale
  parce que c'est le seul sens « budget → comportement de la caméra », et qu'il doit rester
  une lecture de métriques, sans boucle cachée.

### 9.3 Un seul choix de contexte, pour les deux

Le contexte d'usage est choisi **une fois**, par l'hôte, sous la forme d'un `SpecKind`. Il
donne à la fois `Specification::for_kind` (le budget) et `CameraSpec::for_kind` (la
caméra : filtre, `T_i`, enveloppe par défaut). Le second est un `match` exhaustif sur le même
enum, et non une chaîne : ajouter un contexte est une erreur de compilation tant que la
caméra ne l'a pas traité.

| Contexte | Spécification du budget | Forecast | Enveloppe |
|---|---|---|---|
| Viewer, à la main | `interactive` (`H` = 1 s) | `MotionForecast`, composé avec l'amortissement (§1.8) | les limites du `CameraController` |
| Glide, `goto` | `interactive`, ou `known_path` pendant le vol | `Path` exact, interruptible | idem |
| Rejeu de bande | `known_path` | `Path` exact, poids 1000 | celle de la bande enregistrée |
| Film, bake | `film` (`H` = 10 s) | `Path` exact sur la trajectoire du film | aucune (la trajectoire est toute connue) |
| FPV | `fpv` (`H` = 2 s) | `MotionForecast` aux paramètres FPV | celle du simulateur |
| Jeu (Bevy) | `interactive` ou `fpv`, au choix de l'hôte | celui du jeu, ou `MotionForecast` | celle que le jeu déclare |
| Casque | `interactive` | OpenXR, puis `MotionForecast` (§4.4) | celle de la tête (vitesses mesurées) |

Changer de contexte en cours de session (un Glide qui part) reste un changement de
spécification, avec son fondu (loi 12). Rien de nouveau.

### 9.4 Où vit le contrat partagé, sans cycle

```
                 ┌──────────────────────────────────────────────────────────────┐
                 │ tuile-budget  (au fond du graphe, entiers seuls)             │
                 │  Specification, SpecKind, Params                             │
                 │  + instants L, τ_k ; κ, N_c, A_max (nombres du forecast)     │
                 │  Need.ahead (quantifié)        BudgetSignals (publiées)      │
                 └───────────────▲──────────────────────────────┬───────────────┘
                                 │ dépend de                    │ métriques (lecture seule)
                 ┌───────────────┴──────────────────────────────┼───────────────┐
                 │ tuile-core   (réexporte tuile-budget en `budget`)            │
                 │  forecast : CameraForecast, ForecastAsk, Forecast,           │
                 │             Hypothesis, Spread, Motion, Envelope,            │
                 │             Still, Path, First, conformance                  │
                 │  protocol : ClientMessage::Forecast                          │
                 │  runtime  : validation, clés de conflation, traversée,       │
                 │             quantification → Need                            │
                 └───────────────▲──────────────────────────────┼───────────────┘
                                 │ dépend de                    │
                 ┌───────────────┴──────────────────────────────┼───────────────┐
                 │ tuile-camera                                 │               │
                 │  CameraController (enveloppe : sa source)    │               │
                 │  MotionForecast (nalgebra), CameraSpec::for_kind(SpecKind)   │
                 └───────────────▲──────────────────────────────┼───────────────┘
                                 │ dépend de                    ▼
   tuile-viewer · tuile-bevy · tuile-web · tuile-bake · hôtes  (composent First(...), lient, affichent)
```

Les flux de données, à chaque image :

```
 spécification (budget) ── instants, until, tolérance ──► ForecastAsk ──► hôte : CameraForecast
                                                                            │ generation(), forecast()
 enveloppe (caméra) ───────────────────────────────┐                        ▼
                                                   └──► ClientMessage::Forecast (enregistré, MCAP)
                                                                            │
                                                     runtime : clé changée ? ── non ──► rien
                                                                            │ oui (≤ 1 / N_c images)
                                                                            ▼
                                                     traversée sur les poses prévues ──► Need quantifié
                                                                            │
                                                                            ▼
                                                     decide (entiers) ──► BudgetSignals ──► (lecture par l'hôte)
```

- **Aucun cycle.** `tuile-budget` ne voit aucun type de caméra : il reçoit des entiers par
  `Need` et `Params`. `tuile-core` ne dépend pas de `tuile-camera`. `tuile-camera` voit le
  budget par la réexportation `tuile_core::budget`, comme aujourd'hui
  (`predicting.rs:99`) **[code]**.
- **Le trait et l'enveloppe sont dans `tuile-core`** : un hôte Bevy, web ou Hydra les
  implémente sans dépendre de `tuile-camera` (D-F2).
- **Les nombres du forecast côté tuile** (`κ`, `N_c`, `A_max`, les instants) sont des
  `Params` de la spécification du budget. Ils ont donc son condensé, son fondu et son
  enregistrement, et ils passent par la grammaire `tuile://budget?…`.

---

## 10. Démonstrateur

Ce n'est pas seulement de la conception : la PR 0 est un démonstrateur exécutable que Laurent
lance et regarde. Il mesure, et il ne change aucune décision de tuile (§7).

### 10.1 Ce qu'il démontre

1. **Le trait implémenté par la caméra du viewer** en interactif : `MotionForecast` (le
   filtre existant, `nalgebra`), avec son `σ`.
2. **Le trait implémenté par un vol connu** : le Glide (`app/flight.rs`) et le rejeu de bande
   (`app/tape.rs`), en `Path` exact, à `σ = 0`, en un seul appel pour tout le vol.
3. **tuile qui consomme le forecast** : les messages construits depuis le trait, identiques
   aux messages d'aujourd'hui, ce qui prouve que l'interface tient sans rien changer.
4. **La conflation** : le compteur de génération de l'hôte, la clé d'empreinte côté tuile, et
   le nombre de traversées **qu'elle aurait évitées**. Ce nombre est compté, sans être
   appliqué : l'appliquer est C1.
5. **Le temps de décision** : `decide` et la traversée p50/p99 par scénario, face aux 2 ms,
   avec la part du forecast isolée (§A).

### 10.2 Où il vit

- **Le viewer natif** (`tuile-viewer`, lancé par `examples/wgpu-viewer`) est le principal.
  Une **surcouche** y montre la trajectoire prévue (une polyligne sur le globe), `σ` en
  ellipses au sol à chaque instant `τ_k`, l'horizon `H` et `now + L` marqués, la génération du
  forecast, l'état déclaré (`motion`) et le compteur de traversées évitables. La surcouche
  est une couche de `tuile-overlay` posée par l'application (`OverlayLayers`,
  `frame_host.rs:424`) **[code]**. Les chiffres vont dans la fenêtre du budget, à côté de la
  section « Prediction » d'aujourd'hui (`predicting.rs:259-308`).
- **On l'active** par le menu (une case « Forecast ») ou par `tuile://forecast?show=on|off`,
  une route nouvelle de `steer.rs`. C'est un **affichage** de l'application, et non un
  interrupteur du moteur : le moteur calcule et mesure la même chose que la surcouche soit
  là ou non (règle « pas d'interrupteurs »).
- **Le web** : le même code de viewer compilé en wasm (`examples/web-viewer`), si la PR 0 le
  permet sans retard. Sinon il vient avec F3. La surcouche est la même.
- **Headless** : il n'y a pas d'`examples/headless` dans l'arbre (vérifié). Le scénario
  headless vit donc dans `tuile-budget-sim`, qui dépend déjà de `tuile-camera` **[code]** :
  `tests/forecast.rs` et un binaire `forecast_demo`. Il passe les caméras du catalogue
  (`tour`, `aggressive-sequence`, `catalogue.rs:139-300`) par un `CameraController` piloté
  par des gestes synthétiques, de sorte que le forecast ne voie pas le futur (le mode
  « comme une main »). C'est là que tourne la CI. Il ne fait pas de rendu.

### 10.3 Les scénarios reproductibles

Ce sont des mains scriptées, sur le mécanisme existant `TUILE_TOUCH_SCRIPT`
(`app/script.rs`, aujourd'hui `tour`) **[code]**. Elles passent par le vrai contrôleur, donc
par la vraie inertie et le vrai amortissement. Les mêmes scénarios existent en headless.

| Scénario | Geste | Ce qu'il éprouve |
|---|---|---|
| `zoom` | zoom avant puis arrière, à la molette, sur un relief | `ln h`, le niveau voulu qui change vite, la clé de conflation |
| `orbit` | rotation régulière du cap autour d'un point | une génération stable à vitesse constante (§1.7, règle 5) |
| `u-turn` | panoramique rapide, puis demi-tour brusque | l'erreur après violation du modèle, la récupération, 0 trou |
| `glide` | `tuile://goto?…&seconds=6` Paris → Mont-Blanc | l'exactitude, un seul calcul pour le vol, l'interruption par une main à mi-vol |
| `stop` | lancer, glissade, puis arrêt net d'un clic | la glissade exacte, puis la bascule au repos |
| `aggressive` | la séquence de Laurent (`AGGRESSIVE_PHASES`) | tout ensemble, pour comparer avec le budget |

**Métriques par scénario**, exportées vers Prometheus (`job="tuile-viewer"`, étiquette
`vcs_ref_head_revision`, comme le §5.4 du budget) et dans le MCAP de la session (canal du
forecast et pas enregistrés), **jamais en CSV** :

- l'erreur du forecast par horizon et par canal (p50, p90), la couverture à 1σ et 2σ, et le
  gain sur « immobile » : `tuile_forecast_error_*` ;
- le temps d'appel de l'API : `tuile_forecast_call_seconds` ;
- les générations, et les traversées faites et évitables :
  `tuile_forecast_generations_total`, `tuile_forecast_walks_total{outcome="walked"|"avoidable"}` ;
- `decide` en ms, p50 et p99 (`tuile_budget_decide_seconds`, `_peak_seconds`), et la
  traversée par passe, présente et prévue (`traversal_seconds`, séparée par passe) ;
- les trous, qui doivent rester à 0 (`EyeCoverage::lost`, `observe.rs:118-125`) ;
- les tuiles sous le besoin (`tuile_budget_under_need_tiles`) ;
- l'âge des messages à leur application (`issued_at`).

### 10.4 Le critère de réussite, et la comparaison avec #282

On fait tourner les mêmes scénarios sur `main` (le viewer de #282) et sur la PR 0, puis on
les compare par révision.

| Critère | Seuil |
|---|---|
| Décisions inchangées | condensés `budget_sim` et goldens identiques ; même `under_need`, mêmes trous (0) |
| Coût d'exposer | p99 ≤ 10 µs natif (`forecast`), ≤ 2 µs (`generation`) ; la suite échoue sinon |
| Part du forecast dans la décision | moyenne ≤ 20 µs ; le pire pas mesuré et rapporté face au budget de 0,5 ms |
| `decide` | rapporté face aux 2 ms, par scénario ; la PR 0 n'ajoute pas plus de 2 % au p99 |
| Vol connu | erreur nulle à l'interpolation près ; 1 génération par vol non interrompu |
| Conflation (comptée) | au repos, 100 % des traversées prévues évitables ; Glide ≥ 95 % ; `orbit` ≥ 50 % **[supposé]** |
| Interactif | erreur p50 meilleure que « immobile » à chaque horizon ≤ `H` ; couverture à 2σ : 95 % ± 5 points, **ou l'écart rapporté tel quel** (c'est une mesure, pas un rouge, tant que C3 n'est pas livrée) |

Ce que la comparaison doit montrer avec #282 : les mêmes messages et les mêmes décisions ;
la calibration de `σ`, que #282 calculait sans l'exposer ; l'âge réel des messages, que #282
ne pouvait pas voir ; et le nombre de traversées que #282 fait à chaque pas en mouvement et
que C1 évitera.

**Ouvrir ce qu'on mesure.** Chaque scénario se regarde dans le viewer avec la surcouche,
avant tout chiffre (règle « damier à chaque itération »). Une trajectoire prévue qui part de
travers se voit avant de se compter.

### 10.5 Coût de construction et ce que le Mac supporte

| Crate | Fichiers | Taille estimée |
|---|---|---|
| `tuile-core` | `forecast.rs` (types, `Still`, `Path`, `First`), `forecast/conformance.rs` ; `protocol.rs` (`issued_at`), `runtime.rs` (compteurs, clés) | ≈ 700 lignes, la moitié de tests |
| `tuile-camera` | `predict.rs` : `MotionForecast` sur `MotionModel` | ≈ 150 |
| `tuile-viewer` | `app/predicting.rs` (composition), `app/forecast_overlay.rs` (nouveau), `steer.rs` (route), `menu.rs` (case), `app/script.rs` (scénarios) | ≈ 600 |
| `tuile-telemetry` | métriques `tuile_forecast_*`, canal MCAP | ≈ 150 |
| `tuile-budget-sim` | `tests/forecast.rs`, `bin/forecast_demo.rs` | ≈ 300 |

Ces tailles sont **[supposé]**. Il n'y a ni nouvelle dépendance ni nouveau crate.

**Sur le Mac**, que Laurent utilise pour tester des viewers : rien de lourd. Les tests, les
compilations du viewer et du wasm passent par le builder k8s, dans les créneaux donnés par la
session principale (mémoire « rien en local »). En local, au plus `CARGO_BUILD_JOBS=2 nice
-n 19 cargo check -p tuile-core` (ou `-p tuile-camera`), un seul job à la fois, avec
`uptime` sous 12. Le viewer que Laurent lance vient du builder ou d'une compilation niced
dans un créneau convenu.

**Prototype.** Je n'ai pas prototypé de squelette sur la branche. La machine était chargée
(charge sur 5 et 15 minutes à 18 et 22 au début de la session) et Laurent y teste des viewers.
La forme des types (§1.2) n'utilise que `glam` et des tableaux de taille fixe, déjà présents
dans `tuile-core`. Le risque de compilation est faible, mais il n'est pas vérifié.

---

## 11. Décisions demandées à Laurent

- **D-F1. Plusieurs hypothèses pondérées, un poids par échantillon.** Le contrat accepte
  jusqu'à 4 hypothèses, chacune une trajectoire avec `σ` et un poids par échantillon (en
  ‰, de somme ≤ 1000 à chaque instant, le reste allant au plancher). Ce poids remplace la
  probabilité par vue d'aujourd'hui et la « survie » d'une version antérieure de ce document. Un Glide
  interruptible donne deux hypothèses, « il continue » et « il s'arrête ». L'autre voie,
  une seule hypothèse avec une survie, ne sait pas dire « l'un de deux personnages ».
  **Je recommande plusieurs hypothèses, plafonnées à 4.**
- **D-F2. Une seule source de vérité, réparti ainsi.** La spécification du budget possède
  les instants (`L`, `τ_k`), `H`, `κ`, `N_c` et `A_max`. La caméra possède son enveloppe
  (vitesses, zoom, amortissement, glissade, altitudes), qu'elle déclare dans le message, et
  son filtre (`q`, `r`, `T_i`). `PredictConfig` perd ses horizons, ses `reach_*` et
  `coast_s`. Le contexte est un seul `SpecKind`, apparié exhaustivement des deux côtés. Le
  trait et l'enveloppe vivent dans `tuile-core`. Cela déplace la route
  `tuile://budget?horizons=` vers la grammaire de la spécification. **Je recommande oui.**
- **D-F3. Ce que couvrent les 2 ms, et l'ordre.** Est-ce `decide` seul, ou le pas entier
  (traversées, besoin, diff, `decide`) ? Je propose **le pas entier hors chargements**, avec
  au plus 0,5 ms au pire pas pour la traversée prévue. Aujourd'hui, `decide` seul dépasse
  déjà 2 ms en zoom (6,9 ms p99, relevé d'octobre). Je propose aussi l'ordre suivant : la
  PR 0 (démonstrateur, mesure seule), puis C1 (la conflation appliquée, le gain de temps le
  plus sûr), puis C2, puis C3. **À trancher avant la PR 0**, parce que la PR 0 affiche ce
  critère.
