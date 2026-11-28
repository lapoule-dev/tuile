<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Préchargement par valeur attendue : conception

Statut : **proposé, 2026-11-28, conception seule** (aucun code de production), pour la
validation de Laurent. Branche `feat/budget-preload-by-expected-value`, sur `main` d7ac8133.

Demande de Laurent : « ON DOIT FAIRE LE PRELOADING PAR SCORE ET NON PAR CERCLE
CONCENTRIQUE… on a besoin de rigueur et non d'idées fancy », puis « il faut un score
calculé de manière fiable, pas un truc complètement artisanal… on dirait de la peinture ».

Ce document remplace, pour le préchargement, le §2 et le §4–§5 de
`budget-progressive-coarse-preload.md` (le score `A × D` et le voisinage isotrope). Il
garde intacts les lois absolues de `budget-eviction-prefetch-spec.md` §2.1, la monnaie du
§1b (le flouz) et la mécanique du §1c. Il précise la loi 8 (quotas) et la loi 2b (voisinage
grossier), qui ne sont pas appliquées aujourd'hui.

Légende des appuis : **[code]** vérifié dans l'arbre à d7ac8133 ; **[spec]** déjà écrit et
validé par Laurent ; **[lit.]** une référence publiée (rapports `Bibliographie éviction et
préchargement.md` et `Monnaie unique et approximations rapides.md`) ; **[dérivé]** une
conséquence mathématique des termes précédents ; **[supposé]** une hypothèse qui n'a ni
mesure ni référence, à mesurer avant de s'y fier.

---

## 0. Pourquoi le score actuel est de la « peinture » : le constat

Tout ce qui suit est lu dans le code **[code]**, sans exécution.

1. **L'ensemble candidat ne dépend pas de la vue.** `neighbourhood.rs:139-208` liste, à
   chaque niveau entre le plancher et `n + 1`, les `⌊π(ρ+1)²⌋` tuiles de sol les plus
   proches de l'empreinte `F` (78 par niveau à ρ = 4). Rien ne lit le frustum, la direction
   du regard ni la vitesse. C'est un disque centré sur l'œil, et il produit les cercles.
2. **Le « P » n'est pas une probabilité.** `value.rs:198-238` calcule `A(ℓ, n) × D(d)`.
   `A` divise par deux par niveau d'écart, avec `a0 = 0,8`, `a↓ = 0,2` et un relèvement de
   `0,95 × 2^−(ℓ−3)`. `D = ρ²/(ρ+d)²`. Aucune de ces constantes n'est mesurée ni dérivée.
   Ce sont des formes choisies pour obtenir un ordre. Rien ne vérifie que « P = 0,4 » se
   réalise 4 fois sur 10.
3. **Les vues anticipées (#282) jettent leur incertitude.** Le filtre de Kalman calcule un
   écart type par canal, σ(τ) (`predict.rs:546-557`), mais la vue envoyée ne porte que sa
   moyenne et `p = e^(−τ/λ)` (`predict.rs:882-917`). La valeur `λ = 1,9 s` est recopiée des
   mesures de mouvement de tête en vidéo 360° (Flare). Elle n'a pas été mesurée sur un globe.
   La même probabilité vaut pour toute la vue, qu'une tuile soit au centre ou au bord.
4. **Le décideur ne lit pas la prédiction.** `Need.ahead` (`need.rs:551`, `AheadUse`) est
   enregistré, mais aucun fichier de `tuile-budget/src` ne le lit (vérifié par grep). Le
   message `Ahead.reach` (`runtime.rs:1255`) est stocké et jamais utilisé.
5. **La loi 8 n'est pas appliquée.** `Speculation.slots_permille` (750) et `memory_permille`
   (200) ne sont lus que par l'export des paramètres. Le préchargement peut occuper les 16
   emplacements d'une classe. Seuls `Floor` et `Hole` préemptent (`engine.rs:2868-2910`).
   D'où la photographie « 157 drapés de besoin en file, 0 en vol, 16 préchargements en vol »
   (`reports/anneau-du-budget.md`).
6. **Le gain d'un préchargement est `P × w × (H + T)`** (`value.rs:289-310`). Le terme
   `+ T` (« un niveau lent part plus tôt ») n'est pas dérivé de l'objectif. Il est
   l'inverse du `(H − T)⁺` du besoin présent, sans justification.
7. **La loi 2b n'existe pas dans le code.** `Params::hole_penalty` n'existe pas (grep).
8. **Le test qui le dit est déjà là, ignoré** :
   `tuile-budget-sim/tests/prediction.rs::predicted_views_ahead_leave_fewer_tiles_under_their_need`,
   `#[ignore = "red: … until the anticipation band reads AheadUse"]`. Mesure du 2026-11-23,
   Desktop/Fast : 74 196 tuile·pas sous le besoin avec le modèle, contre 23 271 sans. **Avec
   le score actuel, prédire fait pire que ne rien prédire.**

---

## 1. Les lois, en clair

Chaque loi est numérotée P1–P12 (P pour préchargement), pour ne pas les confondre avec les
lois 1–13 du spec. Les lois absolues du spec restent au-dessus de tout.

- **P1. Une tuile est préchargée parce qu'une vue prévue en aura besoin, pas parce qu'elle
  est proche.** Les candidates sont les tuiles que les vues prévues sur l'horizon
  atteignent, frustum compris. Le disque autour de l'œil disparaît.
- **P2. La probabilité est une vraie probabilité.** `P = 0,3` veut dire que, sur toutes les
  tuiles notées 0,3, trois sur dix sont effectivement dessinées à l'instant prévu. Cela se
  mesure et se teste (calibration), et elle se calcule par tuile, à partir de la pose
  prévue et de son incertitude.
- **P3. Une tuile vaut ce qu'elle ajoute à l'image.** Ce qu'elle apporte, ce sont les niveaux
  gagnés sur ce qui serait dessiné à sa place sans elle (l'ancêtre résident), fois les pixels
  qu'elle couvre, fois la probabilité, fois le temps pendant lequel elle sert. Le tout est
  rapporté à ses octets : c'est le flouz par octet du spec.
- **P4. Une tuile ne vaut que ce qu'elle rapporte une fois arrivée.** Le temps de
  chargement mesuré (sa distribution, pas une constante) décide quelle part de l'horizon
  elle peut encore servir. Rien d'autre ne fait d'escompte : l'incertitude est déjà dans `P`.
- **P5. Le plus urgent part en premier.** L'ordre de départ est la perte qu'on subirait en
  attendant un pas de plus. Elle monte quand l'échéance moins le temps de chargement
  approche de zéro, sans seuil.
- **P6. Le grossier passe d'abord, par le calcul et non par une règle.** Un parent couvre
  quatre fois la surface d'un enfant pour un poids comparable : sa valeur par octet est
  plus grande tant que l'ancêtre résident est plus grossier.
- **P7. Garder et charger se mesurent dans la même monnaie.** La perte d'un relâchement se
  calcule avec les mêmes `P`, niveaux et pixels, pondérés par la probabilité que le
  rechargement arrive trop tard. On ne charge A en relâchant B que si A vaut plus que B,
  avec la marge (loi 7 du spec).
- **P8. Un plancher atteignable, dimensionné par une loi.** Il couvre ce qu'aucune
  prédiction ne voit venir : un geste brusque. Autour de l'œil, on garde les tuiles que le
  geste le plus rapide permis peut découvrir avant qu'un chargement lancé à cet instant
  n'arrive, à au plus `G` niveaux sous leur besoin. Sa taille découle de la vitesse maximale
  des gestes, du temps de chargement mesuré et de `G`. Aucun rayon n'est choisi à la main.
- **P9. Un préchargement ne fait jamais attendre le besoin présent.** Le besoin présent passe
  avant dans la file. Il a des emplacements réservés, dont le nombre suit son débit
  d'arrivée mesuré (loi de Little). S'ils ne suffisent pas, il reprend l'emplacement d'un
  préchargement en vol. À l'arrêt, rien n'est réservé, et le préchargement peut tout prendre.
- **P10. Le sol n'est jamais noir.** Le score ne touche à rien de protégé : tuiles
  dessinées, groupes d'échange, chaîne de repli, plancher épinglé. Ces protections sont
  calculées hors du score. Une mauvaise prédiction coûte des chargements, jamais un trou.
- **P11. Le travail par pas est borné.** Seules sont réévaluées les tuiles dont une entrée a
  changé. Les files sont à seaux et les échanges par pas sont plafonnés.
- **P12. Tout se rejoue.** Les poses prévues et leur incertitude, les `P` par tuile et les
  distributions de latence sont des entrées enregistrées. Le décideur reste en entiers.

---

## 2. Formellement

### 2.1 Notations et entrées enregistrées

- `k = 1..K` : les instants de l'horizon, `τ_k` secondes après maintenant. Ce sont les
  horizons de `PredictConfig` (aujourd'hui 0,25 / 0,5 / 1 / 2 s, `predict.rs:275`) **[code]**.
  `τ_0 = 0` est la vue présente : elle relève du besoin présent et de sa propre bande,
  inchangée. `H = τ_K`.
- `Δt_k` : le poids de l'instant `k` (règle des trapèzes),
  `Δt_k = (τ_{k+1} − τ_{k−1})/2`, avec `τ_{K+1} = τ_K`. On a `Σ_k Δt_k = H − τ_1/2`. C'est
  la discrétisation de l'intégrale de l'objectif `J` du spec §1 **[dérivé]**.
- Pose prévue à `τ_k` : moyenne `μ_k` (5 canaux : est, nord, `ln h`, cap, tangage) et écarts
  types `σ_k` du filtre (`MotionModel::predict`) **[code]**. **Nouveau** : `σ_k` voyage dans
  `AheadView` (5 × `f32`). Aujourd'hui seule la moyenne voyage.
- `F_c(t)` : la fonction de répartition du temps de chargement de la classe `c` depuis le
  niveau de stockage où la ressource se trouve. On la mesure du pas de départ au pas
  d'arrivée, comme `R` aujourd'hui (`budget-progressive-coarse-preload.md` §2) **[code]**.
  **Nouveau** : `TierSample` porte un histogramme à seaux doublants en ms (12 × `u16`,
  les mêmes seaux que les attentes, `waits`) à la place de la seule médiane. L'EMA par seau
  se fait dans `decide`, comme aujourd'hui (spec §4.2).
- `Δ` : la période d'un pas, EMA des `now` successifs, enregistrée.

Tout ce qui est flottant (géométrie, `erf`, projections) se calcule **hors du décideur**,
dans `tuile-core` (runtime, en f64), puis est quantifié une seule fois dans le `Need`.
Le `Need` est enregistré à chaque pas (loi 5) **[code]**.

### 2.2 L'ensemble candidat (P1, P8)

`C = C_prévu ∪ C_plancher`, et rien d'autre.

#### a) `C_prévu` : l'empreinte des vues prévues

Pour chaque `k`, le runtime construit une vue **élargie** `V_k⁺` à la pose moyenne `μ_k`.
Ses demi-angles d'ouverture sont élargis de `z · σ_ang,k`, avec :

- `σ_ang,k² = σ_cap,k² + σ_tangage,k² + (σ_sol,k / h)²` : l'incertitude angulaire,
  translation ramenée à un angle à la distance la plus courte (la hauteur), donc majorée ;
- `z = Φ⁻¹(1 − p_min)`, où `p_min` est la valeur plancher du spec (`floor_value_permille`,
  1 %) : `z = 2,33` pour une gaussienne. **Pourquoi ce `p_min`** : une copie sous 1 % est
  déjà relâchée par la loi de la valeur plancher. Lister une candidate qu'on relâcherait
  aussitôt serait contraire à la loi 7 **[dérivé]**.

Les `K` vues élargies sont traversées **en une passe**, comme le fait déjà la passe des
vues anticipées (`traverse_with(&self.ahead)`, toutes les `AHEAD_EVERY = 4` étapes ou quand
elles bougent, `runtime.rs:1976-1992`) **[code]**. On garde les sélectionnées, les demandées
et leurs ancêtres à contenu. Une tuile dont `max_k P_k < p_min` (§2.3) est retirée.

La traversée connaît déjà l'arbre, le culling d'horizon et l'erreur d'écran. Aucune
géométrie nouvelle n'est écrite côté budget, qui n'en a pas.

#### b) `C_plancher` : le plancher atteignable

Le spec §5.2b (Laurent, 2026-11-21) demande ceci : les tuiles grossières autour du lieu sont
prêtes pour toute éventualité, et `P` vient de l'**ensemble atteignable** par les gestes les
plus rapides, dans toutes les directions **[spec]**. On en fait une loi de taille.

- `T_exp` est le temps pendant lequel un geste non prévu découvre du sol sans que rien ne
  soit encore parti : `T_exp = T_détection + q95(F_réseau)`. `T_détection` est le nombre de
  pas que met `MotionModel::moving()` à passer à vrai, mesuré ; `q95` est le 95ᵉ centile
  mesuré.
- Les gestes les plus rapides valent `v_pan = reach_heights_per_s × h` au sol et
  `reach_zoom_out_per_s` en zoom arrière (`predict.rs:111-114`, défauts 3 h/s et ×4/s). Ils
  sont déjà envoyés dans `Ahead.reach` **[code]**, mais leurs valeurs sont **[supposé]** :
  il faut les prendre au contrôleur de gestes ou au p99 des sessions enregistrées.
- `d_ℓ = e_ℓ · f / τ` est la distance 3D en deçà de laquelle une tuile de niveau `ℓ` est
  voulue. `e_ℓ` est son erreur géométrique, `f` la focale en pixels, `τ` le seuil d'erreur
  d'écran. C'est la règle de raffinement de la traversée **[code]**.
- `G` est la tolérance, en niveaux : de combien le sol peut être plus flou que son besoin
  pendant un geste imprévu. C'est le seul nombre de la loi qui ne soit pas mesuré. Il a un
  sens visible et appartient à la spécification. **Décision D2.**

**Loi.** Pour chaque niveau `m`, du plancher épinglé + 1 jusqu'au besoin `n` sous l'œil :

```
C_plancher(m) = { tuiles de niveau m qui touchent le disque au sol de rayon
                  ρ_m = d_{m+G} + v_pan · T_exp   autour du nadir de l'œil }
```

**Justification [dérivé].** Depuis toute pose atteignable dans `T_exp` (déplacement au sol
≤ `v_pan · T_exp`, cap et tangage quelconques, hauteur quelconque), un lieu `x` dont le
besoin est au moins `m + G` est à moins de `d_{m+G}` de l'œil. Il est donc dans le disque
`ρ_m` autour du nadir actuel. Le zoom arrière ne fait que réduire le besoin. Le zoom avant
le rend plus fin que `n`, ce qui relève de la prédiction. Si `C_plancher` est résident, toute
pose atteignable trouve donc partout une tuile à au plus `G` niveaux sous son besoin.

**Borne [dérivé].** Une tuile de côté `s_m` qui touche un disque de rayon `r` tient dans le
disque `r + √2 s_m` (`budget-progressive-coarse-preload.md` §5). D'où :

```
|C_plancher(m)| ≤ π (ρ_m / s_m + √2)²,      ρ_m / s_m = (e_m f / (τ s_m)) / 2^G + v_pan T_exp / s_m
```

Le premier terme est constant d'un niveau à l'autre, car `e_m ∝ s_m` sur un quadtree. Le
second est divisé par deux à chaque niveau plus grossier. Le total est une somme
géométrique, petite au-dessus du besoin.

*Exemple chiffré, entièrement* **[supposé]** : `f = 935 px` (1080 px, 60°), `τ = 16`,
`e = s/64` (la grille du test du voisinage, à mesurer sur le vrai terrain) ; le premier terme
vaut alors `0,91 / 2^G`. On prend `s_n ≈ 1,1 h`, `v_pan = 3 h/s`, `G = 1`. Avec
`T_exp = 0,5 s`, on obtient environ 33 tuiles au niveau `n`, 20 à `n−1`, 15 à `n−2`, puis
environ 11 par niveau plus grossier. Pour `n = 13`, cela fait environ 180 tuiles, contre 858
aujourd'hui. Avec `T_exp = 8 s` (l'ordre des attentes réelles p50 relevées sur le viewer),
cela fait environ 1 800 tuiles au seul niveau `n`. **La taille du plancher suit le réseau
mesuré. Il faut donc mesurer `F_réseau` avant tout réglage** (§8).

#### c) Ce que devient le disque : supprimé, remplacé par le plancher atteignable

On écarte les deux options du brief telles quelles :

- **(i) Supprimer le disque sans le remplacer.** Le §5.2b de Laurent ne serait plus tenu :
  un geste brusque non prévu, vers le haut ou de côté, découvrirait du sol couvert seulement
  par le plancher épinglé (niveau 2, l'aplat beige). La loi 2b et le test « zoom-in, pan,
  tilt, rotation, zoom-out sans perte » échoueraient par construction.
- **(ii) Un disque réduit de taille choisie.** Ce serait un nombre magique de plus.

**Choix : (ii) au sens de la loi.** `neighbourhood.rs`, `Proximity`, `reach_tiles` / `ρ` et
`per_level` sont **supprimés**. `C_plancher` les remplace. C'est encore un disque, parce que
les rotations sont libres et que l'ensemble atteignable est donc isotrope. Mais son rayon
par niveau sort de la loi, et il ne descend pas sous le besoin `n` : aucun `n + 1` isotrope.
Les tuiles de `C_plancher` entrent **dans la même monnaie** (§2.3), avec `P = 1` à l'instant
`T_exp` comme le demande le §5.2b. Elles ne sont **pas protégées** (loi 2b : « priced, never
exposed », la protection reste le plancher épinglé).

### 2.3 Le score : valeur attendue par octet

Pour une ressource `r` (le maillage d'une tuile `t`, ou un de ses drapés), les termes, leur
unité, leur source et leur calcul :

| Terme | Unité | Source (entrée enregistrée) | Calcul, quantification |
|---|---|---|---|
| `P_k(t)` | sans dimension, unités `CERTAIN` (1/32 000) | `μ_k, σ_k` (AheadView) + géométrie de `t` | `P_k = P^vis_k × P^niv_k`, ci-dessous ; f64 dans le runtime, puis `round(P × 32 000)` |
| `Δq(r)` | niveaux | la résidence (budget) | `ℓ(r) − ℓ(a(r))`, entier ; `a(r)` = l'ancêtre résident le plus fin (maillage), ou le drapé que `best_drape_for` porterait (drapé) |
| `w_k(t)` | pixels | `μ_k` + volume englobant | `min(π (f · R / d_k)², W·H)` ; `R` le rayon de la sphère englobante, `d_k` la distance à la pose moyenne ; quantifié en `u32` px |
| `Δt_k` | s (µs en code) | `PredictConfig.horizons` | trapèzes, §2.1 |
| `F_c` | sans dimension | `Measured` (histogramme) | interpolation linéaire par morceaux entre seaux, en ‰ |
| `bytes(r)` | octets | `Cost::drape_cost` (drapé, connu avant chargement) ; EMA des tailles acquittées par (classe, niveau) (maillage) | `u64`, plancher `ref_bytes/16` comme aujourd'hui |

**`P^vis_k(t)`, la probabilité d'être dans le champ.** On l'obtient comme le produit des
intégrales marginales en lacet et en tangage sur l'intervalle angulaire de la tuile, la
méthode de Zou et al. (arXiv 1906.08575) **[lit.]** :

```
P^vis_k = [Φ((a_ψ − δψ)/s_ψ) − Φ((−a_ψ − δψ)/s_ψ)] · [Φ((a_θ − δθ)/s_θ) − Φ((−a_θ − δθ)/s_θ)] · 1[visible de μ_k au test d'horizon]
```

- `δψ, δθ` : l'écart angulaire du centre de la tuile à l'axe de la vue moyenne.
- `a_ψ = hfov/2 + α`, `a_θ = vfov/2 + α`, où `α = asin(R / d_k)` est le rayon angulaire.
- `s_ψ² = σ_cap² + (σ_⊥ / d_k)²` et `s_θ² = σ_tangage² + (σ_h cos θ / d_k)²`. `σ_⊥` est
  l'écart au sol perpendiculaire à la ligne de visée, `σ_h = h · σ_ln h`. C'est une
  linéarisation au premier ordre, **[dérivé]**.
- `Φ` : la loi de l'erreur. Le filtre la suppose gaussienne. Zou rejette la gaussienne pour
  la tête par Jarque-Bera et prend une Laplace **[lit.]**. **La famille se choisit sur les
  histogrammes d'erreur mesurés** (`PredictionErrors`, déjà collectés par horizon **[code]**).
  Le code accepte les deux ; le choix est une donnée de la spécification.

Hypothèses de ce produit : indépendance des canaux (vraie dans le modèle, les canaux sont
filtrés séparément **[code]**), linéarisation, test d'horizon à la moyenne seulement.
**Elles sont vérifiées contre une référence Monte Carlo (§5, T1). Elles ne sont pas
admises.**

**`P^niv_k(t)`, la probabilité que le niveau de `t` soit voulu.** Le parent `p` de `t` se
raffine quand `e_p f / d > τ`. Avec `ln d` de moyenne `ln d_k` et d'écart `σ_ln d ≈ σ_ln h`
**[supposé]** (vrai près du nadir, à vérifier en vue rasante) :

```
P^niv_k = Φ( (ln(e_p f / τ) − ln d_k) / σ_ln d )          (= 1 pour une tuile plus grossière que le besoin)
```

**Futurs connus.** Pour un vol plané, un coast ou une trajectoire, `σ = 0` et
`P_k = e^(−τ_k / T_i)`, la loi de l'interruption (`interrupt_s`) **[code]**, avec `T_i` à
mesurer. Pour un film : `P ∈ {0, 1}` par le plan (spec §5.4). Un rejeu (`Future::Certain`)
donne `P = 1`.

**Unité du score : le pas de raffinement.** Depuis #324, un raffinement est tout ou rien sur
les frères (`siblings_drawable`, `engine.rs:1446-1450`) **[code]**. Un enfant seul ne change
aucun pixel. La valeur se calcule donc **par groupe de frères absents `S`**, comme le « pas »
du rapport *Monnaie unique* §1 **[lit.]** : gain du groupe = `Σ_{t∈S}` des gains, octets du
groupe = `Σ_{t∈S}` des octets, et chaque frère reçoit la valeur par octet du groupe.

**La valeur de résidence (charger maintenant), en flouz par octet** :

```
V(S) = Σ_{t∈S} Σ_k P_k(t) · Δq(t) · w_k(t) · Δt_k · F_c(τ_k)   /   Σ_{t∈S} bytes(t)
```

**L'urgence (ordre de départ), en flouz par octet par pas d'attente** :

```
U(S) = Σ_{t∈S} Σ_k P_k(t) · Δq(t) · w_k(t) · Δt_k · [F_c(τ_k) − F_c(τ_k − Δ)]   /   Σ bytes
```

`U` est la perte attendue si l'on attend un pas de plus : la part de chaque instant qu'un
départ différé de `Δ` ferait manquer. C'est l'analogue exact de `ΔT_pf` de TIP **[lit.]**,
dérivé au rapport *Monnaie unique* §3. `U` est nulle loin de l'échéance, car `F` y vaut 1 des
deux côtés. Elle est maximale quand `τ_k − Δ` traverse le gros de la distribution de
latence, c'est-à-dire quand l'échéance moins le temps de chargement passe par zéro. Elle
retombe ensuite sans s'annuler pour les instants suivants. C'est l'urgence croissante
demandée, **sans paramètre ajouté** **[dérivé]**.

**La perte d'un relâchement (garder), en flouz par octet** :

```
L(r) = Σ_k P_k · Δq(r) · w_k · Δt_k · (1 − F_{c,tier'}(τ_k))  /  bytes(r)
```

`tier'` est le niveau où la copie retomberait (décodé ou réseau, comme `Valuation::of`
aujourd'hui **[code]**). C'est la qualité perdue aux instants où un rechargement lancé
aussitôt arriverait trop tard.

**Les deux formules du spec en sont des cas limites** **[dérivé]**. Pour un besoin
continu sur `[0, H]`, `∫₀ᴴ F(τ) dτ = H − E[min(T, H)] ≈ (H − T)⁺` : c'est le gain du besoin
présent du spec §1b (`load_gain`, `preload = false`). Et `∫₀^∞ (1 − F(τ)) dτ = E[T]` : c'est
la perte `P · Δq · w · T_restore` du spec §1b (`Flouz::loss`). V et L ne sont donc pas des
formules nouvelles. Ce sont **les mêmes formules, étendues à un besoin futur et à une
latence aléatoire**. Le `H + T` du préchargement d'aujourd'hui n'en sort pas, et il disparaît.

**Pas d'escompte `δ(t)`.** Dans le spec, `δ` portait deux choses : la perte de confiance, qui
est maintenant dans `P`, calibrée ; et le retard d'arrivée, qui est maintenant dans `F`. En
ajouter un troisième compterait deux fois la même chose. **`δ ≡ 1`** **[dérivé]**.

**Pas de `hole_penalty`.** Le plancher épinglé (niveaux 0–2) couvre tout le globe et n'est
jamais relâché **[code]**. Une place sans meilleure tuile porte donc ce plancher, et son
`Δq = n − 2` dépasse déjà tout raffinement d'un niveau (11 niveaux à `n = 13`). La pénalité
constante de la loi 2b n'aurait aucun fondement mesurable. Les vrais trous vus dans le viewer
étaient des trous de **sélection** (sol jamais sélectionné, `lost_tiles` aveugle) : aucun
chargement ne les corrige. Ils relèvent de l'instrument géométrique de la loi 2b (§5, T11).
*À confirmer par Laurent : c'est un écart à la lettre de la loi 2b.*

### 2.4 Comparaison avec `value.rs` : conservé, supprimé

| Élément d'aujourd'hui | Sort | Pourquoi |
|---|---|---|
| `Flouz`, `FlouzPerByte`, `CERTAIN` | **conservé** | la monnaie et son unité sont celles du spec ; `CERTAIN` est l'échelle de `P` |
| `Valuation` (crédit GreedyDual, récence `L`) | **conservé**, crédit recalculé | `L` reste la valeur des copies qu'aucune vue prévue ne voit (fournisseur LRU de TIP) ; le crédit `P·R/bytes` devient `L(r)` ci-dessus |
| `load_gain` du besoin présent `(H − T)⁺` | **conservé** (bande `Value`) | c'est le cas limite de `V` ; la bande du besoin présent n'est pas touchée ici |
| `deficit_due` (§5.3, au repos) | **conservé** | c'est le besoin présent ; hors périmètre |
| `load_gain` avec `preload = true`, `H + T` | **supprimé** | non dérivé ; remplacé par `V` et `U` |
| `progressive`, `progressive_terms`, `product` (`A × D`) | **supprimés** | ce n'est pas une probabilité (§0) ; remplacé par `P_k` |
| `Progressive` (`a0`, `a↓`, `coarse_from`, `coarse_boost`) | **supprimés** | constantes sans mesure ; le grossier d'abord sort de `Δq × w / bytes` (P6) |
| `Anticipation` (`ring_permille` 500, `ancestor_permille` 250) | **supprimés** | « P » plats ; chaque candidate a son `P_k` |
| `Quantised.need_level`, `away`, `Need.proximity`, `Proximity` | **supprimés** | servaient `A × D` |
| `neighbourhood.rs`, `BudgetConfig::reach_tiles`, `Params::rho_tiles` | **supprimés** | remplacés par `C_plancher` (§2.2 b) |
| `Need.ahead: AheadUse { next_use_in, probability }` | **remplacé** | devient `Need.ahead: [AheadSample; K]`, avec `(P_k, w_k)` quantifiés par instant ; `Δq` se lit côté budget (il dépend de la résidence) |
| `Speculation.slots_permille`, `memory_permille` | **remplacés** | par la réserve de Little et l'échange (§3) |
| `Urgency::Refresh` à 100 ‰ | **inchangé, signalé** | sans fondement, hors périmètre |
| `Compress.probability_permille` (lit « le P progressif ») | **adapté** | il lit `max_k P_k` (une copie dessinée a `P = 1`) |

---

## 3. Un seul ordre pour charger et relâcher

1. **Deux prix, une monnaie.** Le spec §1b donne un prix par limite **[spec]**. Ici, il y en a
   deux. Les **octets tenus** ont pour prix `λ_mem` : la plus haute `L` relâchée pour faire
   place, déjà publiée comme `exchange_threshold` **[code]**. Les **emplacements de
   chargement** sont ordonnés par `U`. L'**admission** se lit sur `V` : un départ n'est
   admis que si `V(S) > λ_mem × (1 + m)`, sous la cible de la classe ou contre le moins
   valant des tenus. C'est le *Do No Harm* de Cao et al. **[lit.]** : on ne lance pas ce qui
   serait relâché à l'arrivée. Le relâchement prend les `L` les plus basses. `V` et `L`
   intègrent la même densité `P · Δq · w` : c'est le « même ordre » des deux côtés, à la
   façon des deux estimateurs de TIP dans une même monnaie.
2. **Comparaisons quantifiées sans échange nuisible.** Les valeurs vont dans des seaux
   logarithmiques entiers (`m = 6`, 2 048 seaux, bitmap 64 × 64, `clz` / `ctz`). Un échange
   n'a lieu que si la borne basse du seau de la candidate dépasse la borne haute du seau du
   tenu, fois `(1 + marge)` (*Monnaie unique*, « Quantifier en log ») **[lit./dérivé]**. La
   quantification peut manquer un échange marginal, jamais en faire un mauvais.
   `margin_permille = 250` reste le chiffre en vigueur. Il est **[supposé]** : il doit
   dépasser `2^(1−m)` (3 %), plus l'erreur de calibration de `P` (§5). On le vérifiera par
   le test anti-flap I4.
3. **Échanges bornés par pas.** Un seau à jetons, en entiers et rempli par le temps, borne
   les départs d'anticipation. Son débit est le débit mesuré du réseau divisé par la taille
   moyenne d'une ressource ; sa rafale vaut un huitième de seconde, comme dans le tableau du
   spec §6.3 **[spec]**. `release_visits` borne déjà les relâchements **[code]**, et
   `request_heap = 256` borne la file examinée **[code]**.
4. **La loi 8 enfin appliquée, sans réserve fixe.** La réserve à 75 % a été essayée et
   retirée : elle « starved the views ahead and coarsened a still view »
   (`budget-progressive-coarse-preload.md` §3) **[code/mesure]**. À la place, pour chaque
   classe `c` :

   ```
   r_c = min( ⌈ λ̂_c · T̂_c ⌉ , quota_c − 1 )
   anticipation en vol (réseau) ≤ quota_c − r_c
   ```

   `λ̂_c` est le débit d'arrivée des voulus du besoin présent, en voulus par seconde (EMA,
   enregistré). `T̂_c` est le temps moyen de chargement mesuré. Par la loi de Little, `r_c`
   est le nombre de voulus présents qui arrivent pendant un chargement, donc le nombre
   d'emplacements qu'il faut garder libres pour qu'aucun n'attende un préchargement
   **[lit./dérivé]**. À l'arrêt, `λ̂ → 0` et `r_c = 0` : le préchargement prend tout, ce qui
   corrige le défaut mesuré de la réserve fixe. En mouvement, la réserve suit la demande.
5. **Préemption.** Elle ne joue que si un voulu présent attend et qu'aucun emplacement n'est
   libre. On annule alors le préchargement en vol de la même classe dont `U` est la plus
   basse (le moins pénalisé par un retard), un par voulu présent en attente. Aujourd'hui,
   seuls `Floor` et `Hole` préemptent **[code]**. L'extension e16525e3 a été revert pour du
   churn « caméra immobile » (`reports/anneau-du-budget.md` §2) **[code]**. Ici, la condition
   est « un besoin présent attend » : il n'y en a pas au repos, donc il n'y a pas de
   préemption au repos. **Cette affirmation est à vérifier sur le scénario qui avait
   churné** (§5, T3b). La cause exacte du churn de e16525e3 n'est pas connue (§8).
6. **La mémoire.** La part de mémoire spéculative de la loi 8 n'est plus un pourcentage. Elle
   découle de la règle d'admission : une anticipation n'est livrée que sous la ligne basse et
   ne déplace jamais une copie du besoin présent (`deliver_one`, **[code]**, inchangé). Elle
   ne déplace un autre préchargement que si sa valeur le dépasse avec la marge. La borne « ≤
   cible − octets protégés » tient donc par construction. `memory_permille` disparaît.
   *Écart à la lettre de la loi 8, à confirmer (D3).*

---

## 4. Garanties

### G1. Le sol n'est jamais noir (preuve)

1. L'ensemble protégé `Π` (tuiles dessinées, bloqueurs d'un échange, chaîne de repli de tout
   ce qui est dessiné ou voulu, plancher épinglé, ressources en transit) est calculé depuis
   la sélection présente et la résidence. **Il ne lit aucun terme du score** : ni `P`, ni
   `V`, ni `U`, ni `L`. Le changement proposé ne modifie que des termes du score et
   l'ensemble des voulus d'anticipation. `Π` est donc identique, pas à pas, à entrées égales
   **[dérivé du code : `held.rs`, `sweep.rs`, filtre de `make_room`]**.
2. Aucun relâchement ne prend un élément de `Π` (invariant I11, testé chaque pas du
   simulateur) **[code]**. Les échanges restent « le nouveau actif avant de relâcher
   l'ancien » (groupes d'échange, I9) **[code]**.
3. Les voulus d'anticipation ne font qu'**ajouter** des départs, sous admission et sous la
   bande du besoin présent. Ils ne retirent rien de `Π`.
4. Donc toute place couverte au pas `k−1` l'est au pas `k`, sauf si aucune copie d'elle ni
   d'un ancêtre n'a jamais été dessinable, ce qui est impossible une fois le plancher épinglé
   arrivé. ∎

Ce que G1 ne couvre pas : un trou de **sélection**, du sol que la traversée ne sélectionne
pas. C'est l'instrument géométrique de la loi 2b (T11), et c'est un objet de mesure, pas une
preuve.

### G2. Un préchargement ne retarde jamais un besoin présent (preuve, et sa limite)

- **File** : `Precedence::Value` (besoin présent) précède `Precedence::Anticipation` dans
  `PriorityKey` **[code]**. Aucun préchargement ne passe avant un voulu présent démarrable.
- **Emplacements** : à chaque pas, si `w` voulus présents attendent dans la classe `c` et
  que `f` emplacements sont libres, la préemption (§3.5) libère `min(w − f, a)` emplacements
  tenus par l'anticipation (`a` en vol). Donc le nombre de voulus présents démarrés vaut
  `min(w, quota_c − présents_en_vol)`, exactement ce qu'il serait sans aucune anticipation.
  ∎ (Ce qui est réservé par `r_c` n'évite que le coût d'une annulation, pas un retard.)
- **Mémoire** : une anticipation ne déplace jamais une copie du besoin présent, et la
  contre-pression `decoded_full` ne s'applique qu'à elle **[code]**. ∎
- **CPU de décodage** : le pool ordonne par rang de chargement (`offload::ForLoad`)
  **[code]**, et le besoin présent est mieux rangé. ∎
- **Limite** : le **débit du lien**. Des préchargements en vol, non préemptés parce qu'il y
  avait de la place, partagent la bande passante avec les chargements présents et les
  ralentissent. G2 ne le couvre pas. On le borne en plafonnant les octets d'anticipation en
  vol à `débit mesuré × T̂` (un temps de chargement). C'est **[supposé]** suffisant, et à
  mesurer : `fetch_seconds` des voulus présents, avec et sans anticipation.

### G3. Coût par pas borné (complexité)

- **Runtime.** Une passe de traversée des `K` vues élargies, au plus toutes les
  `AHEAD_EVERY` étapes ou quand les vues bougent : `O(N_visités)`, comme aujourd'hui, en plus
  grand du facteur d'élargissement. Puis `O(|C| · K)` évaluations fermées de `P_k` (deux
  différences d'`erf` par axe). `|C| ≤ Σ_k |sél(V_k⁺)| + Σ_m π(ρ_m/s_m + √2)²`. Le
  calcul de `C_plancher` ne se refait que si `n`, la tuile sous l'œil, `T_exp` (quantifié à
  3 bits significatifs) ou `G` changent, comme `Neighbourhood::update` aujourd'hui.
- **Budget.** Une réévaluation par entrée sale (`NeedDelta`) : `O(Δ · K)`. Une
  réévaluation complète `O(|C| · K)` au plus une fois par intervalle de mesure (1 s), quand
  `F` change. L'histogramme est quantifié, donc la plupart des intervalles ne changent rien.
  Les seaux coûtent `O(1)`. Les échanges sont au plus `b` (seau à jetons), chacun `O(|S|)`,
  et `|S| ≤ 4` sur le quadtree du sol. Les relâchements sont au plus `release_visits`.
- **Taille enregistrée.** `K × (u16 + u32)` par candidate, seulement dans les deltas :
  ≈ 24 o × |C|. À mesurer (`NeedDelta` en octets par pas), parce que c'est un coût de MCAP.

### G4. Déterminisme (rejeu exact)

Les flottants (`σ`, géométrie, `erf`) sont calculés en f64 **avant** `decide`, dans
`tuile-camera` et `tuile-core`, puis quantifiés **une fois** dans `Need.ahead` et
`AheadView`. Ces deux objets sont des entrées enregistrées (`BudgetEvent::Predictor`,
`StepRecord`) **[code]**. L'histogramme de latence est un champ de `Measured`, enregistré.
Le décideur ne fait que des entiers, départagés par l'identifiant. Un rejeu d'un
enregistrement donne donc le même condensé, bit à bit, quelle que soit la cible. Les écarts
flottants entre natif, wasm et Metal/Vulkan restent possibles **avant** la quantification
(rapport *Monnaie unique* §8). Ils n'atteignent pas le rejeu, qui relit les valeurs
quantifiées. Ils peuvent seulement faire diverger deux sessions *vivantes* sur deux cibles
différentes, ce qui est acceptable.

---

## 5. Validation

### 5.1 Tests rouges d'abord, chacun vu échouer contre une mutation

Chaque test est écrit avant le comportement. On lance ensuite la mutation indiquée et on
note l'échec dans la PR.

| # | Test (crate) | Ce qu'il exige | Mutation qui doit le faire échouer |
|---|---|---|---|
| T1 | `p_matches_monte_carlo` (tuile-core) | sur un corpus de 500 tuiles × poses × σ, `|P̂ − P_MC| ≤ 0,02` (MC : 65 536 poses tirées d'un générateur à graine, test de frustum et d'horizon exact) | retirer le terme `σ_⊥/d` ; prendre le test d'horizon à la moyenne pour `σ` grand |
| T2 | `p_is_calibrated_on_scripted_motion` (tuile-budget-sim) | trajectoires scénarisées + bruit de geste à graine : par décile de `P`, fréquence observée dans ±0,1 ; score de Brier < celui de la référence « caméra immobile » | remettre `p = e^(−τ/λ)` uniforme par vue |
| T3 | `a_preload_never_delays_a_present_need` (tuile-budget) | tous les emplacements pris par l'anticipation, un voulu `Drape` arrive : démarré au même pas | retirer la préemption étendue |
| T3b | `no_preemption_at_rest` (tuile-budget-sim) | scénario immobile (et celui où e16525e3 churnait) : 0 `CancelReason::Preempted` après convergence | conditionner la préemption à « présent voulu » plutôt que « présent en attente » |
| T4 | `anticipation_slots_follow_little` (tuile-budget) | au repos `r_c = 0` ; en rafale `r_c = ⌈λ̂ T̂⌉` ; l'anticipation en vol ≤ `quota − r_c` à chaque pas | réserve fixe de 25 % |
| T5 | `candidates_follow_the_predicted_frustum` (tuile-core) | pan vers l'est : les tuiles devant ont `P > 0`, celles derrière, hors `C_plancher`, ne sont pas candidates | réintroduire le disque `ρ = 4` |
| T6 | `coarse_first_by_value` (tuile-budget, propriété) | pour tout parent et enfant candidats à `P` égal, `V(parent) > V(enfant)` tant que l'ancêtre résident est plus grossier que le parent | `Δq` compté depuis le plancher plutôt que depuis l'ancêtre résident |
| T7 | `urgency_rises_toward_the_deadline` (tuile-budget) | `U` croît quand `τ_k` décroît vers `q50(F)`, puis décroît ; aucun seuil | remplacer `F(τ) − F(τ−Δ)` par une constante |
| T8 | `reachable_floor_bound_and_coverage` (tuile-core) | `|C_plancher(m)| ≤ π(ρ_m/s_m + √2)²` ; depuis 1 000 poses atteignables tirées dans `T_exp`, chaque lieu visible a une tuile de `C_plancher` à au plus `G` niveaux sous son besoin | `ρ_m` sans le terme `v_pan T_exp` ; `G + 1` dans le test |
| T9 | `replay_is_exact_with_predicted_samples` (tuile-budget) | un enregistrement avec `AheadView.σ` et `Need.ahead[K]` rejoué donne le même condensé | lire `σ` hors de l'entrée (horloge ou état caché) |
| T10 | `predicted_views_ahead_leave_fewer_tiles_under_their_need` (tuile-budget-sim, **existant, `#[ignore]` retiré**) | avec le modèle, moins de tuile·pas sous le besoin que sans, 0 trou | — (c'est le rouge d'aujourd'hui) |
| T11 | `aggressive_sequence_has_no_ground_hole` (tuile-budget-sim) | zoom-in → pan → tilt → rotation → zoom-out : chaque tuile de l'empreinte géométrique (frustum ∩ ellipsoïde au niveau du plancher) est dessinée, couverte ou partitionnée, à chaque pas (loi 2b) | retirer `C_plancher` |
| T12 | `work_follows_changes_not_holdings` (tuile-budget) | les réévaluations par pas dépendent de `Δ` et non des tenus (4 000 contre 40 000 copies) ; existe déjà sous `a_turn_re_keys_what_it_names_not_what_is_held` | réévaluer tous les tenus à chaque pas |
| T13 | `siblings_share_their_step_value` (tuile-budget) | les frères absents d'un même groupe ont la même valeur par octet | valeur par tuile seule |

### 5.2 Comparaison à une référence exacte, en simulation

Ceci prépare le « exact TIP reference » du spec §1c, qui n'existe pas encore **[code]**.

- **(a) Référence aux entrées parfaites (borne de prédiction).** Le décideur reçoit la vraie
  trajectoire future (`P ∈ {0, 1}`) et les vraies latences du `SimNet`. L'écart
  `J(rapide) − J(parfait)` mesure le coût de la prédiction. C'est la *relaxed Belady* de LRB
  **[lit.]**.
- **(b) Référence exacte des approximations.** On prend les mêmes entrées que le décideur
  rapide. `P` vient du Monte Carlo (65 536 tirages) et non de la forme fermée. Tout est
  recalculé chaque pas, sans seau ni plafond d'échanges. L'écart mesure le coût des
  approximations (forme fermée, seaux, travail borné). **Acceptation (spec §1c)** : `J`
  rapide à moins de 2 % de `J` exact sur chaque petit scénario, sans aucune loi violée. On
  rapporte aussi l'écart **relatif** à la marge entre la ligne de base « sans prédiction »
  et la référence, comme le recommande Berger (FOO) **[lit.]**.
- **(c) Énumération sur scènes minuscules.** On prend au plus 12 tuiles candidates, au plus
  4 emplacements et une trajectoire scénarisée déterministe. On énumère tous les ordres de
  départ pour obtenir l'optimum de `J`, puis on mesure l'écart du glouton. Le glouton n'est
  qu'à moitié garanti dans le pire cas (Funkhouser & Séquin) **[lit.]** : ce test chiffre
  l'écart réel.
- **(d) Lignes de base obligatoires.** Sans prédiction ; et le score d'aujourd'hui
  (`A × D` + disque, figé par ses condensés enregistrés, puis supprimé : mémoire « pas de
  copie de moteur en référence »).

### 5.3 Avant / après `budget_sim`

Scénarios du catalogue (`aggressive-sequence`, `long-pan`, les FPV, le soak de 5 min)
× profils `Desktop` et `Phone`, et **`Web`, à ajouter** : le catalogue n'a que deux appareils
**[code]**. Le web doit avoir ses propres limites (WebGPU, pas de stockage local,
`http_cache`). On rapporte :

- `J` (niveau·s sous le besoin) ;
- les trous (doivent être 0) ;
- les tuile·pas sous le besoin ;
- les départs gaspillés (`wasted_starts`) et le thrash ;
- les octets d'anticipation tenus ;
- la part de prédictions utilisées (`ahead_used/(used+unused)`) ;
- le travail par pas et le p99 de `decide` ;
- `|C|` par pas (avant : disque ; après : `C_prévu` + `C_plancher`).

Le tableau avant/après va dans le corps de chaque PR. Les goldens et `recorded.txt` sont
re-blessés volontairement (`TUILE_BLESS_REFERENCE=1`), avec l'explication de chaque condensé
qui bouge.

### 5.4 Mesures réelles

On compare par révision, sur Prometheus `job="tuile-viewer"`, étiquette
`vcs_ref_head_revision` :

```promql
# déficit intégré par seconde de session, par type
sum by (vcs_ref_head_revision, kind) (increase(tuile_budget_deficit_level_seconds_total{job="tuile-viewer"}[$__range]))
  / ($__range_s)
# pic de tuiles sous le besoin
max by (vcs_ref_head_revision, kind) (max_over_time(tuile_budget_under_need_tiles{job="tuile-viewer"}[$__range]))
# convergence
sum by (vcs_ref_head_revision) (increase(tuile_budget_converged_seconds_sum{job="tuile-viewer"}[$__range]))
  / sum by (vcs_ref_head_revision) (increase(tuile_budget_converged_seconds_count{job="tuile-viewer"}[$__range]))
# attentes, prédictions utilisées, préemptions
max by (vcs_ref_head_revision, kind) (tuile_budget_wait_p95_ms{job="tuile-viewer"})
```

Les noms exacts des compteurs `ahead_used`/`ahead_unused` et des annulations par raison
sont **à vérifier** dans `tuile-telemetry/src/otlp.rs` avant la première mesure.

**Le même scénario de gestes.** Un rejeu (`app/tape.rs`) passe aujourd'hui par
`Future::Certain` (`predicting.rs:240-242`) **[code]** : il donne `P = 1` et mesure donc la
spécification « chemin connu », pas l'interactive. Il faut une option de rejeu « comme une
main », qui rejoue les poses sans exposer le futur au prédicteur. C'est une valeur de
configuration de mesure, pas un interrupteur de comportement. Sans elle, les comparaisons à
la main ne sont pas reproductibles.

### 5.5 Ce que le simulateur ne couvre pas, et comment le corriger

`NetProfile::desktop` donne un p95 d'environ 150 ms (`net.rs:45-60`) **[code]**. Le viewer
réel a montré des attentes p50 de 8 s et p95 de 33 s (`reports/anneau-du-budget.md` §3), et
1 579 tuiles d'anneau contre une centaine. L'écart est de deux ordres de grandeur, et il ne
vient pas que du réseau.

- **Le simulateur modélise la latence par requête, pas la file.** Les 8 s réelles sont une
  attente en file : emplacements tenus, décodage CPU saturé, upload. Correction : modéliser
  le décodage comme un pool de `N` serveurs, avec `N` et le temps de décodage par classe
  mesurés, ainsi que les voies d'upload par frame (`FrameDeadline`).
- **Les latences sont inventées.** Correction : un profil `desktop_mesuré` ajusté aux
  quantiles p50/p95/p99 de `tuile_tiles_fetch_seconds{source}` d'une révision donnée. La loi
  est log-normale ou un mélange, à choisir sur l'histogramme : la queue compte, puisque `U`
  lit `F`. Les profils sont des données, et l'ancien reste comme `desktop_idéal`.
- **La densité du monde n'est pas celle du vrai arbre.** On compare les départs par seconde
  en pan, simulés et réels, et on cale la densité du monde simulé dessus.
- **En attendant, on contourne.** Chaque conclusion de §5.3 doit tenir sur un **balayage**
  de latence (×1, ×10, ×50 du profil), et non à un seul point : c'est une analyse de
  sensibilité. Une décision qui ne tient qu'à 150 ms n'est pas une décision.
- **Le simulateur ne voit ni la prédiction réelle ni le rendu.** La calibration de `P`
  (T2) sur des gestes simulés ne prouve rien pour une vraie main. La calibration réelle se
  mesure sur les sessions enregistrées (MCAP), §8.

---

## 6. Découpage en PR

Une PR de comportement à la fois. Chacune passe le test manuel de Laurent sur le viewer
avant la suivante (mémoire « un merge, puis Laurent teste »). Les PR « mesure » ne changent
aucune décision.

| PR | Nature | Contenu | Rouges |
|---|---|---|---|
| **M1** | mesure | histogramme de latence par niveau de stockage dans `TierSample`, compté de bout en bout du départ à l'arrivée, enregistré mais non lu par les décisions ; calibration de la prédiction d'aujourd'hui (diagramme de fiabilité de `p`, NIS par canal) sur le board ; `T_détection` mesuré | condensés inchangés (preuve qu'aucune décision ne bouge) |
| **M2** | mesure (sim) | profil `Web` ; profil réseau ajusté aux mesures + pool de décodage ; balayage de latence ; décompte `|C|` | — |
| **B1** | comportement | **loi 8** : réserve de Little + préemption quand un présent attend (§3.4–3.5). Indépendante du score, elle corrige le défaut mesuré (« 0 en vol, 16 préchargements »). | T3, T3b, T4 |
| **B2** | comportement | **ensemble candidat + score** : `σ` dans `AheadView`, `P_k` fermé, `Need.ahead[K]`, `V` et `U` par groupe de frères, `C_plancher` ; suppression de `neighbourhood.rs`, `A × D`, `Progressive`, `Anticipation`, `Proximity`, `rho` | T1, T2, T5–T8, T10, T11, T13 |
| **B3** | comportement | **côté tenu** : le crédit de `Valuation` devient `L(r)` ; l'échange compare `V` et `L` en seaux, avec la borne basse contre la borne haute | T6 (version éviction), T9, T12 |
| **R** | validation | références (a)–(c) de §5.2 dans `tuile-budget-sim` | acceptation 2 % |

B2 est gros, mais il ne peut pas se couper sans interrupteur. Retirer le disque sans
`C_plancher` casserait la loi 2b. Mettre `P_k` sans retirer `A × D` ferait deux scores en
même temps. C'est donc **un** changement de comportement, et on le prouve par les tests.

---

## 7. Risques

1. **`P` mal calibré.** Ce sont alors des chargements gaspillés (G1 et G2 tiennent). Le
   garde-fou est la calibration T2, puis le diagramme de fiabilité réel. Si `P` est
   systématiquement trop haut, on élargit `σ` (le `q` du filtre) d'après le NIS mesuré
   (test de cohérence de Bar-Shalom) **[lit.]**, et non à la main.
2. **Coût de la traversée élargie.** Elle visite plus de nœuds pour `σ` grand, en
   mouvement rapide. `|C|` est mesuré (M2). S'il le faut, on borne `z · σ_ang` par le
   demi-angle qui fait tout le tour, c'est-à-dire qu'on tombe sur `C_plancher`.
3. **Taille des deltas de `Need`.** `K` échantillons par candidate. À mesurer en octets par
   pas d'enregistrement.
4. **Surcompte de `Δq`.** Un parent et son enfant tous deux en vol comptent les mêmes
   niveaux. Le glouton se corrige à l'arrivée du parent (réévaluation des descendants
   sales), mais pas pendant le vol. On accepte ce défaut et on le mesure par la référence
   (b).
5. **Surface `w` des tuiles à cheval sur le bord.** `P^vis × w` surestime les tuiles
   partiellement visibles. C'est une approximation connue, à mesurer contre le Monte Carlo
   de T1 (étendu à la surface visible attendue).
6. **Churn de préemption.** La cause de e16525e3 est inconnue (§8). T3b la cherche, et un
   jeton de préemption par pas en limiterait le débit si elle revenait.
7. **`T_exp` grand** : avec un réseau lent, le plancher atteignable grossit fortement
   (§2.2 b). L'admission le borne par la mémoire, et il s'ordonne lui-même du grossier au
   fin. Mais ses octets doivent être rapportés par profil (`tuile_budget_bytes_by_level`),
   comme le demande le §5.2b.
8. **La bande du besoin présent garde son poids d'écran en ‰** (`500 + centrality/2`),
   tandis que l'anticipation passe en pixels. Les deux bandes ne se comparent pas dans la
   file (précédence), mais elles se comparent à l'éviction. À unifier ensuite, par
   `Need::screen_px` (spec §1, suivi prévu).

---

## 8. Ce que je ne sais pas (à mesurer d'abord)

- **La vraie distribution des temps de chargement**, du départ à l'arrivée, par niveau de
  stockage, sur le viewer réel, et la part d'attente en file dans les 8 s observées. Tout le
  dimensionnement en dépend (`U`, `V`, `T_exp`). C'est M1, avant toute PR de comportement.
- **La précision de la prédiction sur un globe.** Aucune donnée publiée n'existe, à ma
  connaissance, ni dans les deux rapports. Les erreurs par horizon sont collectées
  (`PredictionErrors`), mais je ne les ai pas lues. Il faut aussi savoir si la loi d'erreur
  est gaussienne ou de Laplace (Jarque-Bera sur les histogrammes), à quel horizon le
  modèle cesse de battre « caméra immobile » (ce qui fixe `H`), et si le `q` du filtre est
  cohérent (NIS).
- **Les bornes de geste** `reach_heights_per_s = 3`, `reach_zoom_out_per_s = 4` : ce sont
  des défauts **[supposé]**, à prendre au contrôleur ou au p99 des sessions.
- **`e_ℓ / s_ℓ` sur le vrai terrain** (l'exemple du §2.2 b suppose `1/64`) et l'effet de la
  SSE à profondeur de vue (#323) sur `d_ℓ`.
- **`σ_ln d ≈ σ_ln h`** en vue rasante.
- **La cause du churn de e16525e3** (« W 14 827 contre 256, chargements annulés caméra
  immobile »). Je n'ai pas relu ce commit. Le revert seul est établi.
- **La part des « tuiles sous le besoin » qui vient de la sélection** (#323, #333) plutôt que
  du chargement. Le nouveau score ne corrige que la seconde.
- **Si la convexification des pas** (un enfant qui vaut plus par octet que son parent,
  rapport *Monnaie unique* §1) est nécessaire. Par P6 je l'attends rare ; à vérifier sur la
  référence (c).
- **Le coût réel de `erf` × `|C|` × `K` en wasm.** Il n'est pas mesuré, mais sans doute
  petit (quelques milliers d'évaluations par passe).

---

## 9. Décisions demandées à Laurent

- **D1. Plusieurs poses échantillonnées, ou une seule vue à `L`.** Le plan du 23 nov. §3
  dit « une décision sur la vue à `t + L` (500 ms), qui *remplace* la vue présente ». Je
  propose de garder la vue présente comme besoin présent (`P = 1`, sa bande) et d'ajouter
  les `K` instants prévus, avec un `P` par tuile, en anticipation. Les raisons : (a) une vue
  qui remplace la vue présente contredit P9 / loi 4 (un mauvais `L` affame l'écran) ; (b)
  une seule pose jette `σ` et l'échéance, donc `U` ; (c) c'est déjà la passe de #282, qui ne
  coûte qu'une traversée de plus. La « conflation » du plan est tenue par
  `AHEAD_EVERY`, les ensembles sales et la quantification. **À trancher.**
- **D2. Le disque isotrope supprimé, remplacé par le plancher atteignable de P8, et la
  valeur de `G`.** `G` est la seule grandeur non mesurée de la loi : le flou toléré, en
  niveaux, pendant un geste imprévu de vitesse maximale. Je propose `G = 1`. `G = 0`
  coûterait environ 4 fois plus de tuiles près du besoin ; `G = 2` laisserait voir un flou
  marqué. Corollaire à confirmer : pas de `hole_penalty` (§2.3), parce que le plancher
  épinglé rend `Δq` fini et déjà dominant.
- **D3. La loi 8 appliquée par la loi de Little, et la préemption étendue.** On remplace
  `slots_permille = 750` et `memory_permille = 200` par une réserve `⌈λ̂ T̂⌉` qui vaut 0 au
  repos, et on préempte un préchargement dès qu'un voulu présent attend. C'est un écart à la
  lettre de la loi 8 (des quotas fixes) au profit de son intention. La réserve fixe a été
  mesurée nuisible, et la préemption étendue a été revert une fois pour churn. **À trancher
  avant B1.**

---

## Références

- Code : `crates/tuile-budget/src/value.rs`, `priority.rs`, `engine.rs` (`key_of_at`,
  `start`, `select_tile`), `spec.rs`, `inputs.rs`, `need.rs` ;
  `crates/tuile-core/src/neighbourhood.rs`, `runtime.rs` (`need`, passes, `on_client`) ;
  `crates/tuile-camera/src/predict.rs` ; `crates/tuile-viewer/src/app/predicting.rs` ;
  `crates/tuile-budget-sim/tests/prediction.rs` ; `crates/tuile-budget/tests/budget_sim/net.rs`.
- Documents : `budget-eviction-prefetch-spec.md` (§1, §1b, §1c, §2, §4, §5, §6.3) ;
  `budget-progressive-coarse-preload.md` ; plan du 2026-11-23 §3 (décider sur la vue à
  500 ms) ; `reports/anneau-du-budget.md`, `reports/cercles-concentriques.md`,
  `reports/Bibliographie éviction et préchargement.md`,
  `reports/Monnaie unique et approximations rapides.md`.
- Littérature (niveaux de preuve tels que les rapports les donnent) : Patterson et al.,
  *Informed Prefetching and Caching*, SOSP 1995 (TIP : une monnaie, `ΔT_pf`, horizon) ;
  Cao, Felten, Karlin & Li 1995–96 (*Do No Harm*, *First Opportunity*) ; Young et Cao &
  Irani (GreedyDual-Size) ; Zou et al., arXiv 1906.08575 (probabilité par tuile = produit
  des marginales lacet/tangage, Laplace) ; Qian et al., Flare, MobiCom 2018 (précision par
  horizon, marge pilotée par la précision) ; Ng et al., CGI 2005 (frustum conservateur,
  vitesse–densité–débit) ; Funkhouser & Séquin, SIGGRAPH 1993 (sac à dos glouton
  incrémental) ; Berger, Beckmann & Harchol-Balter, FOO 2018 (encadrement de l'optimum,
  écart relatif) ; Song et al., LRB, NSDI 2020 (*relaxed Belady*, *good decision ratio*) ;
  Bar-Shalom, Li & Kirubarajan (cohérence d'un filtre de Kalman par NIS/NEES) ; Little
  (1961), `L = λ W`.
