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
   avant F0 (§8).
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

**Choix : la troisième.** C'est une trajectoire échantillonnée aux instants de tuile, chaque
échantillon avec sa dispersion. Un hôte exact y répond sans erreur, par l'évaluation de sa
fonction du temps. Un hôte interactif y répond par sa forme fermée (Kalman), un casque par
le runtime OpenXR. Aucun ne rééchantillonne.

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
    /// How likely the camera is still on this forecast at `at`, thousandths:
    /// the chance nothing interrupts a known path. 1000 for a film or a replay,
    /// and for an estimator whose uncertainty is all in `spread`.
    pub survival_permille: u16,
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

/// What a host's camera system tells tuile of the camera to come.
pub trait CameraForecast {
    /// Fills `out[i]` with the camera displayed at `at[i]`, for a prefix of
    /// `at` (ascending, all after `now`), and returns how many were filled:
    /// fewer when the forecast ends sooner (a film's last frame), 0 when it
    /// knows nothing. No allocation, no I/O, no lock, no blocking.
    fn forecast(&mut self, now: Micros, at: &[Micros], out: &mut [ForecastSample]) -> usize;

    /// Where the forecast comes from, for the measures only: no decision
    /// reads it.
    fn source(&self) -> ForecastSource {
        ForecastSource::Estimated
    }
}

pub enum ForecastSource { Still, Exact, Interruptible, Estimated, Device }
```

`Lens` reprend la lentille de `ViewStateParams` (`fovy_rad` ou `Fov`, `traversal.rs:68` et
`101-113`) **[code]**. La fenêtre et l'échelle n'en font pas partie : tuile les a déjà. Il
reconstruit la `ViewState` de chaque échantillon avec la fenêtre de la vue présente, comme
`predicting.rs:171-181` le fait aujourd'hui.

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
- **Cible : ≤ 10 µs pour `K = 16` en natif, ≤ 30 µs en wasm.** C'est **[supposé]** : le
  filtre fait 5 canaux × K produits 2×2, soit quelques centaines de flops, et un chemin
  exact évalue une interpolation. La suite de conformité le mesure (§4.6). Un hôte qui
  dépasse n'est pas refusé ; il est signalé par la métrique `tuile_forecast_call_us`.
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

---

## 2. Qui l'implémente

### 2.1 Le principe : l'hôte, avec des défauts fournis par tuile

Le trait est **implémenté par le système de caméra de l'hôte** et **exposé** à tuile. C'est
le sens de la demande : celui qui possède la caméra est le mieux placé pour dire où elle va.
Un film connaît sa trajectoire, un simulateur son modèle, un casque son capteur. Un moteur
de jeu sait que sa caméra suit un personnage dont il connaît la vitesse.

tuile fournit trois implémentations, pour qu'aucun hôte n'ait à écrire de filtre :

| Implémentation | Où | Répond | Dispersion | Survie |
|---|---|---|---|---|
| `Still` | `tuile-core` | la pose présente, à tous les instants | **inconnue** : rend 0 échantillon dès qu'il n'y a pas de pose à extrapoler, sinon la pose présente avec `σ` de repos (§2.4) | 1000 |
| `Path<F: Fn(Micros) -> Option<Pose>>` | `tuile-core` | `F(at)` | 0 (exact) | 1000, ou `e^(−(at−now)/T_i)` si le chemin est interruptible |
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
  forecast **exact**, à dispersion nulle. Le film et le rejeu ont une survie de 1000 :
  rien ne les interrompt (aujourd'hui `Future::Certain`, `predict.rs:647`). Le Glide et la
  glissade ont une survie de `e^(−τ/T_i)` : une main peut les interrompre (aujourd'hui
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
| `PredictConfig::horizons_s` | **déplacé** vers la spécification du budget (`L` et les `τ_k`) | c'est tuile qui sait quand il doit décider ; **décision D-F1** |
| `p = e^(−τ/λ)`, `PredictConfig::laplace_s` | **disparaît**, dans la PR B2 du budget et pas avant | l'incertitude passe par `σ` ; c'est déjà la mutation de T2 du budget |
| `interrupt_s` | **reste**, comme paramètre de `Path` interruptible | c'est un taux de survie, pas une dispersion ; **décision D-F2** |
| `AheadView { view, in_us, p_permille }` | `issued_at` + échantillons `(at − issued_at, pose, lens, σ, survie)` | instants vérifiables, `σ` qui voyage |
| `Predictor` : modèle + erreurs mesurées + source | `MotionForecast` (le modèle) d'un côté ; **la mesure des erreurs devient générique**, appliquée à tout forecast (§3.4) | un hôte qui fournit son forecast doit être mesuré comme le filtre |
| `Reach` (`protocol.rs:165-171`) | **inchangé** | c'est l'enveloppe des gestes (P8 du budget), pas un forecast |
| `AheadView::certain(v, order)` de `tuile-bake` | `Path` sur les instants réels des images du film | les échéances deviennent vraies (constat 5) |
| `Vec` alloué à chaque image | tableau fourni par tuile, réutilisé | §1.5 |

Le filtre (`Channel`, `MotionModel`, ses `q` et `r`, le modèle de Singer) ne change pas d'une
ligne. Les messages envoyés ne changent que par leurs instants. La PR F1 (§7) le prouve par
des condensés inchangés, à l'exception documentée des échéances.

### 2.4 La dispersion « de repos »

Une caméra immobile a-t-elle `σ = 0` ? Non : une main peut toujours bouger. Mais ce risque
est couvert par le **plancher atteignable** (P8 du budget, `Reach`), pas par le forecast.
`Still` rend donc la pose présente avec `σ = 0` et une survie de 1000 au premier instant.
Le plancher fait le reste. C'est exactement le partage voulu par le budget : « ce qu'aucune
prédiction ne voit venir » appartient à `C_plancher`. Si on gonflait `σ` au repos, on
compterait deux fois le même risque.

---

## 3. Comment tuile le consomme, sans changer le plan préchargement et budget

### 3.1 Le chemin

```
hôte (CameraForecast)            liaison (viewer, web, bevy, bake)          runtime (tuile-core)            décideur (tuile-budget)
  forecast(now, at, out)  ──►  at = [now+L, now+τ_1 … now+τ_K]  ──►  ClientMessage::Ahead {          ──►  Need (quantifié, enregistré)
                                une fois par image, avant la caméra      issued_at, samples, reach, model }
                                                                        enregistré tel que reçu
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
  définition du besoin. La survie et `σ` n'entrent pas dans le besoin.
- **Le préchargement par valeur attendue** lit les échantillons à `τ_k > L` comme
  `μ_k, σ_k` (§2.1–§2.3 du budget). Il les multiplie par la survie : `P_k = P^vis_k ×
  P^niv_k × survie_k`. Pour un estimateur, la survie vaut 1000 ; pour un chemin exact,
  `σ = 0`. Aucune incertitude n'est donc comptée deux fois **[dérivé]**. C'est le seul
  ajout à la formule du budget, et il est neutre pour `MotionForecast`.
- **La garde ne bouge pas.** Le dessin et la protection `Π` restent à la pose **présente**
  (la caméra que l'hôte fait dessiner à cette image). `Π` s'agrandit de la protection du
  besoin à `p_L`, sans jamais rétrécir (§2.5.3 du budget). Le forecast ne retire rien de
  protégé. Une erreur de forecast coûte des chargements, jamais un trou (G1 du budget).
- **La conflation** (§2.5.2 du budget) reste dans le runtime : la traversée du besoin décalé
  n'est refaite que si `p_L` change la tuile sous l'œil prévu ou le niveau voulu. Un
  forecast qui tremble ne coûte donc pas une traversée par image.

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
par source (`ForecastSource`) et par horizon :

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
  survie (u16). Un `Float64Array` de `21 K` cases traverse `wasm-bindgen` tel quel. Seuls
  `wasm-bindgen` et `js-sys` sont en jeu (règle 8).
- **Un hôte JS qui possède sa caméra** (la façade THREE.js) fournit une fonction
  `forecast(nowUs, atUs: Float64Array, out: Float64Array) => number`. L'adaptateur Rust
  l'appelle sur le fil principal, de façon synchrone, une fois par image, et l'enveloppe
  dans le trait. Le worker ne voit que le message. Aucun appel ne traverse le worker
  (le serveur de géométrie y tourne, `tuile-web`).
- **L'encodage du fil** (`tuile-web/src/wire.rs:104-120`) **[code]** gagne `issued_at`, `σ`
  et la survie par vue. C'est la même passe que celle que demande B2 du budget pour `σ`.
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

- **Film** : `Path` sur la trajectoire du film. Le forecast est exact (`σ = 0`, survie 1000),
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

1. **Forme** : `n ≤ at.len()` ; `out[..n].at == at[..n]` ; rien n'est écrit au-delà de `n`.
2. **Valeurs** : positions finies et à une distance plausible du centre de la Terre ;
   `direction` et `up` unitaires à 1e-9 près et non colinéaires ; lentille dans ses bornes ;
   `σ ≥ 0` et finis ; survie ≤ 1000.
3. **Sans allocation** : un allocateur de comptage, en test, compte zéro allocation sur
   1 000 appels après le premier.
4. **Coût** : le p99 de 10 000 appels à `K = 16`, rapporté et comparé à la cible (§1.5). Ce
   test est étiqueté lent ; il rapporte toujours et n'échoue qu'au-delà de 10 × la cible
   **[supposé]**, pour ne pas dépendre de la machine.
5. **Continuité à l'instant présent** : pour `at` → `now`, l'échantillon tend vers la pose
   affichée de l'image présente.
6. **Exactitude** (forecast de source `Exact`) : sur le scénario, l'erreur à l'échéance est
   nulle à l'interpolation près, et `σ = 0`.
7. **Calibration** (forecast de source `Estimated`) : sur un corpus de mouvements
   scénarisés et bruités à graine (celui de T2 du budget), la couverture à `1σ` et `2σ` est
   dans ±5 points de 68 % et 95 %, et le NEES moyen dans l'intervalle de confiance à 95 %
   **[lit.]**. Le contrôle échoue si l'hôte rend `σ = 0` pour un mouvement imprévu.
8. **Rejeu** : les échantillons d'une session, enregistrés puis relus, redonnent le même
   `Need` au condensé près, sans rappeler le trait.

Les implémentations de tuile (`Still`, `Path`, `MotionForecast`, `First`) sont les premières
à passer cette suite. L'hôte de démonstration de chaque coquille (viewer, web, bevy) la
passe en CI.

---

## 5. Références

### 5.1 Prédiction de pose et compensation du retard

- Azuma & Bishop, *Improving Static and Dynamic Registration in an Optical See-through
  HMD*, SIGGRAPH 1994 : la prédiction de la pose de la tête pour compenser la latence de
  bout en bout. Elle réduit l'erreur dynamique d'un facteur de l'ordre de 5 à 10 sur des
  horizons d'environ 100 ms **[lit.]**.
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

---

## 7. Découpage en PR

Une PR de comportement à la fois, chacune testée à la main par Laurent sur le viewer avant
la suivante (mémoire « un merge, puis Laurent teste »). Chaque test rouge est écrit d'abord
et vu échouer contre la mutation indiquée. Les PR « mesure » ne changent aucune décision, et
des condensés inchangés le prouvent.

| PR | Nature | Contenu | Rouges (et mutation) | Avant / après |
|---|---|---|---|---|
| **F0** | mesure | `issued_at` dans `ClientMessage::Ahead` ; enregistrement MCAP des vues futures reçues ; mesure de l'âge des messages à l'application, par coquille (natif, worker web) | `ahead_messages_are_recorded_and_replayed` (mutation : ne pas enregistrer) ; `message_age_is_measured` | goldens et `recorded.txt` inchangés, sauf le format du fil ; âge p50 et p99 par coquille |
| **F1** | refonte, sans changement de décision | trait `CameraForecast`, `Still`, `Path`, `First` dans `tuile-core` ; `MotionForecast` dans `tuile-camera` ; la chaîne `views_ahead` réécrite en `First(...)` ; `FrameHost::forecast` par défaut vide ; suite de conformité, passée par les quatre implémentations ; plus d'allocation par image | conformité 1 à 6 et 8 (mutations : écrire au-delà de `n`, allouer, `σ ≠ 0` pour un rejeu, instants décalés de 16 ms) ; les tests de `predicting.rs` gardés tels quels | condensés de `budget_sim` **inchangés** : mêmes vues, mêmes `p` ; coût par image avant et après |
| **F2** | comportement (échéances) | les instants deviennent absolus, sur l'horloge de la session ; le bake passe aux vrais instants du film ; l'horloge web devient obligatoire | `bake_ahead_instants_are_the_films` (mutation : `order`) ; `stale_forecast_is_absent` (mutation : ignorer `A_max`) | `budget_sim` re-blessé, chaque condensé qui bouge expliqué ; un rendu de film ouvert avant et après |
| **F3** | parité | ressource `Forecast` dans `tuile-bevy` ; adaptateur JS pour la façade web ; la suite passée par l'hôte de démonstration de chaque coquille | conformité dans la CI de chaque coquille | même scénario en natif, web et bevy : mêmes échantillons au condensé près |
| *B2 du budget* | *comportement, déjà prévu* | *`σ` et la survie voyagent et sont lus par `P_k` ; `laplace_s` disparaît* | *T1, T2, T9 du budget* | *celui du budget* |
| *N1 du budget* | *comportement, déjà prévu* | *l'instant `now + L` est demandé ; le besoin lit `p_L`* | *ceux de N1* | *celui du budget* |

F0 et F1 ne changent aucune décision et peuvent partir dès que le document est validé. F2 est
la seule PR de comportement propre à ce chantier, et elle corrige un défaut mesurable (les
échéances). B2 et N1 appartiennent au plan du budget : ils lisent ce forecast et n'en sont
pas changés. Leur ordre reste la décision D1 du budget.

La calibration par source (§3.4) arrive avec F1, comme mesure : elle alimente directement M1
du budget (diagramme de fiabilité, NIS par canal).

---

## 8. Décisions demandées à Laurent

- **D-F1. Les horizons appartiennent à tuile, pas à la caméra.** Les instants demandés
  (`L` et les `τ_k`) quittent `PredictConfig` et entrent dans la spécification du budget,
  par contexte d'usage. La caméra répond aux instants qu'on lui donne. C'est conforme au
  partage `[0, L]` et `]L, H]` du budget, mais cela déplace un réglage de la route
  `tuile://budget?horizons=` vers la grammaire de la spécification. **Je recommande oui.**
- **D-F2. La survie à part de la dispersion.** Un chemin connu mais interruptible (Glide,
  glissade) porte `σ = 0` et une survie `e^(−τ/T_i)`. Un estimateur porte `σ` et une survie
  de 1. La survie multiplie `P_k` dans le budget : c'est le seul ajout à sa formule, neutre
  pour le filtre. L'autre voie serait de tout replier dans `σ` (un Glide interruptible
  aurait un `σ` qui gonfle), mais un Glide qui continue est exact, et un `σ` gonflé
  disperserait ses préchargements. **Je recommande la survie à part.**
- **D-F3. Le trait vit dans `tuile-core`.** Les types et le trait n'utilisent que `glam`,
  déjà là, et ils compilent pour wasm. Ainsi `tuile-bevy`, `tuile-web`, `tuile-bake` et un
  hôte Hydra l'implémentent sans dépendre de `tuile-camera`, et le filtre reste dans
  `tuile-camera`. L'autre voie est de tout mettre dans `tuile-camera`, et chaque coquille en
  dépendrait. **Je recommande `tuile-core`.**
