<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Le régulateur du budget : les lois en clair

Point 4 du plan du budget (ordre révisé du 23 nov.) : un régulateur
drift-plus-penalty de forme BOLA, qui **dit au préchargement quand s'arrêter
avant qu'il n'y ait plus de GPU**, pas après. Après, c'est l'urgence ; on
corrige avant d'y arriver (Laurent, 28 nov.).

Ce document vient avant le code. Chaque loi tient en une phrase ; viennent
ensuite la quantité régulée, la consigne, les signaux mesurés et les bornes
SASO (dépassement, temps d'établissement, pic d'évictions, pic d'envois). La
règle « un maillage avec son drapé » a sa propre section (§2), parce qu'elle
part d'un état des lieux mesuré et qu'elle laisse une question ouverte à
Laurent.

Statut : **lois écrites, implémentation en cours** sur `feat/budget-regulator`.

---

## 0. Pourquoi : le défaut mesuré

Le décideur règle sa mémoire aujourd'hui par **paliers** (watermarks, `config::Watermarks`) :

- le balayage ne part qu'au-dessus de 100 % de la cible et redescend à 90 % ;
- un préchargement n'est livré que sous 90 % de la cible (`deliver_one`) ;
- un préchargement n'est plus *lancé* que lorsque le niveau décodé est
  plein (`start`, `decoded_full`).

Trois défauts découlent de ces paliers.

1. **Admis au chargement, refusé à la livraison.** Le lancement d'un
   préchargement ne regarde pas s'il y aura de la place à sa livraison. Il
   regarde seulement si le niveau décodé déborde. Mesure de l'agent #364
   (branche locale `wip/364-preload-drapes`, `dd8d204e`) : 184 drapés
   d'ancêtre décodés restent en tâches de livraison. Ils sont reclassés à
   chaque pas, soit environ 6 700 unités de travail pour le tri. Le niveau
   décodé les relâche puis les recharge. Sept invariants de `budget_sim`
   cassent, dont P6.4 (8 062 contre 608), P4.1, P4.3, P5.1 et CB9.
2. **Des libérations tardives et par à-coups.** Rien n'est libéré tant
   qu'on reste sous 100 % de la cible. Au premier dépassement, le balayage
   enlève 10 % d'un coup. Le pic d'évictions par pas tombe juste au moment
   où le besoin présent arrive.
3. **Des parts fixes.** Le préchargement a droit à des parts fixes :
   `speculation.slots_permille = 750` des emplacements réseau et
   `memory_permille = 200` de la cible. Elles ne suivent pas le rythme réel
   des besoins : trop au repos, trop peu en geste.

---

## 1. Les lois

Les lois sont numérotées R1–R12 (R pour régulateur), pour ne pas les
confondre avec les lois 1–13 du spec ni avec P1–P12 du chantier du score.
**Les lois absolues restent au-dessus de tout** : le sol n'est jamais noir ;
une tuile dessinée, sa chaîne de repli et le plancher épinglé ne sont jamais
retirés par la régulation ; le nouveau est actif avant que l'ancien parte.

### Ce qui est régulé

- **R1. Le préchargement a une marge à tenir, pas une part.** Pour chaque
  classe du pool (maillages, drapés), la place libre sous la cible ne doit
  pas descendre sous une consigne de marge `m_c`. Le préchargement remplit
  le reste. Aucune classe n'a de taille réservée : la cible de chaque classe
  reste celle que le pool partagé lui laisse (D-A3), et cette part suit la
  vue.
- **R2. La consigne de marge suit le rythme réel des besoins (loi de
  Little), et vaut zéro au repos.** `m_c = (1 + κ) · λ_c · W_c` :
  - `λ_c`, les octets par pas que le besoin présent demande dans la classe
    (filtrés) ;
  - `W_c`, le temps de chargement en pas ;
  - `κ`, la marge d'erreur de l'estimation.
  Une vue immobile ne demande rien : `λ = 0`, `m = 0`, et le préchargement
  peut tout prendre.
- **R3. La consigne monte tout de suite et redescend lentement.** Le filtre
  de `λ_c` réagit à une hausse au premier pas (attaque) et décroît d'une
  fraction par pas (relâche). La marge est donc là avant que le geste
  n'arrive à l'urgence, et elle ne retombe pas entre deux à-coups d'un même
  geste. *(Voir §4, question Q1 : le plan dit « monte lentement, descend
  vite » ; je l'applique au débit du préchargement, R6, pas à la marge.)*

### Ce que le préchargement a le droit de faire

- **R4. Pas d'admission d'un préchargement sans marge à sa livraison
  prévue.** Un préchargement n'est lancé que si ses octets estimés, ajoutés
  à ceux des préchargements déjà en vol ou décodés pour la classe, tiennent
  sous `cible − m_c`. Il est livré sous la même ligne. Ce qu'on admet au
  chargement est donc livrable, et plus rien ne s'accumule en tâches
  décodées.
- **R5. Une tâche refusée faute de place sort de la file et se réveille
  quand la place revient dans sa classe.** Un préchargement décodé que la
  livraison refuse (la marge a monté entre-temps) est mis en attente. Il
  n'est plus reclassé à chaque pas. Il revient quand la place libre de sa
  classe dépasse `m_c` plus ses octets. S'il attend plus que l'horizon du
  balayage, il est relâché (il n'était qu'une prévision).
- **R6. Le débit du préchargement monte lentement et chute vite (PANDA),
  sans emballement.** Les octets de préchargement admis par pas suivent une
  allocation `a_c`. Elle croît d'un incrément fixe à chaque pas où la marge
  tient. Elle est multipliée par β < 1 au premier pas où elle ne tient plus.
  Elle est bornée en haut par ce que la marge laisse (anti-emballement,
  anti-windup) : elle ne s'accumule pas pendant qu'on ne peut rien en faire.
- **R7. Un seul réglage, V, arbitre qualité contre marge
  (drift-plus-penalty).** Le régulateur tient trois files virtuelles :
  - `Q_gpu`, le manque de marge GPU accumulé ;
  - `Q_ram`, le manque de marge du niveau décodé ;
  - `Q_vol`, l'excès d'octets en vol.
  Chacune grossit quand sa marge manque et se vide quand elle est tenue. Un
  préchargement de valeur `v` et d'octets `b` n'est admis que si
  `V · v ≥ (Q_gpu + Q_ram + Q_vol) · b`. Marges tenues : les files sont
  vides et tout préchargement de valeur positive passe (sous R4). Marges
  entamées : seuls passent les plus utiles par octet, jusqu'à ce que les
  files se vident. V grand favorise la qualité, V petit la marge ; c'est le
  seul nombre à régler.

### Ce que le régulateur libère

- **R8. On libère dès que la marge manque, pas au palier, par petits pas
  plafonnés (seau à jetons).** Quand la place libre d'une classe est sous
  `m_c`, le régulateur libère au plus `min(m_c − libre, jetons)` octets par
  pas, sous la loi du balayage : jamais ce que la vue veut maintenant, ni ce
  qu'elle a voulu dans l'horizon (`sweep_keep_poses`). Les jetons se
  remplissent d'un débit fixe par pas jusqu'à une profondeur fixe. Le pic
  d'évictions par pas est donc borné par construction. Au-dessus de la
  cible, le retour sous la cible reste immédiat et sans jetons (I2, loi
  existante).
- **R9. Le besoin présent ne passe jamais derrière un préchargement (loi 8
  du spec), par une réserve qui suit son rythme.** Les emplacements réseau
  de la classe gardent `⌈μ_c · W_c⌉` places pour le besoin présent, où
  `μ_c` est le débit filtré de ses départs (Little). Le reste est au
  préchargement, et la réserve est nulle au repos. En plus de la réserve :
  la priorité par besoin actuel puis par score, et la préemption d'un
  préchargement en vol quand la réserve ne suffit pas (existante). Cette loi
  remplace `speculation.slots_permille`.
- **R10. Le préchargement n'affiche rien.** Ce qui est préchargé est un
  réservoir de mémoire GPU : il n'entre dans le dessin qu'en devenant le
  besoin. La vue présente reste le besoin.

### Ce qui borne le régulateur

- **R11. Pas d'interrupteur.** Le régulateur remplace les paliers :
  `Watermarks` disparaît, et `speculation` disparaît quand R9 est en place.
  Seuls les nombres se règlent, tous dans `Params` (donc dans la
  spécification, son digest et sa requête `tuile://budget?…`) : V, κ, les
  coefficients du filtre, W tant que M1 ne le mesure pas, l'incrément et le
  β de R6, le débit et la profondeur du seau de R8. Aucun nombre n'est codé
  en dur.
- **R12. Tout se rejoue.** Le régulateur est en entiers (‰, octets, pas),
  sans flottant dans `engine` ni `state`. Son état est celui du décideur.
  Ses signaux sont calculés à partir des entrées enregistrées :
  - les arrivées et les acquittements ;
  - le besoin ;
  - les temps de chargement, qui entrent en paramètre, puis comme entrée
    enregistrée quand M1 les fournira.

  Aucune mesure de l'exécuteur n'entre dans `decide` autrement. Le coût par
  pas est borné : quelques entiers par classe, et le réveil de R5 lit la
  tête d'une file triée.

---

## 2. La règle « un maillage avec son drapé »

> Laurent, 28 nov. : « un mesh doit être accompagné de son drapé, tout le
> temps, même en dehors du préchargement ». Un maillage dessiné sans drapé,
> c'est un sol beige et plat, symptôme déjà vu.

### 2.1 Ce qui est vrai aujourd'hui, par le code

**Côté décideur (`tuile-budget`), au dessin :**

- Un maillage dont le besoin demande de l'imagerie **n'est jamais dessiné
  sans drapé** (I1, D-A1). `Budget::dressable` exige, pour chaque couche,
  un drapé dessinable : le sien, ou celui d'un ancêtre avec sa projection
  UV. `build_item` compte `bare` quand une couche manque, et `budget_sim`
  casse sur `bare > 0`.
- Deux exceptions seulement :
  - un changement de couche en cours : l'ancienne couche reste portée tant
    que la nouvelle n'est dessinable nulle part ;
  - une tuile sans besoin d'imagerie (3D Tiles, aucune couche configurée).
- Le drapé porté peut être **celui d'un ancêtre, sans limite de
  profondeur**, jusqu'au plancher épinglé (niveau 2), c'est-à-dire un texel
  étiré sur des kilomètres. C'est le sol « plat » : il est habillé au sens
  de I1, mais d'une couleur presque uniforme.
- La doublure de #344 (`stand_ins_of`) demande au parent le drapé un niveau
  sous le besoin le plus fin de ses enfants dessinés qui portent deux
  niveaux ou plus sous le leur. C'est elle qui raccourcit l'écart. Mais tant
  qu'elle n'est pas arrivée, l'écart vaut ce que la chaîne détient.
- Un drapé qui échoue est réessayé avec un délai croissant
  (`FailurePolicy::RetryBackoff`). Pendant ce temps, la tuile porte la
  doublure ou un ancêtre.

**Côté décideur, en mémoire (résidence) :**

- `Budget::draped` n'ouvre les drapés qu'aux tuiles `Drawn | Blocker`.
- Une tuile de l'anneau ou de la chaîne d'ancêtres se charge **maillage
  seul** : c'est #364, test rouge `a preload carries its drapes`.
- Un maillage résident sans aucun drapé de sa propre tuile est donc l'état
  normal de tout préchargement. Quand la vue le dessine, il porte un drapé
  d'ancêtre le temps que le sien arrive (`Urgency::Drape`, un aller-retour
  en retard).
- Seul le plancher est chargé en groupe d'échange maillage + drapés (D-A1,
  `floor_group`) : aucun ne part avant que tous soient chargés.

**Côté exécuteur (`tuile-wgpu`, `pump.rs`) :**

- `resolve` compte `bare` quand un drapé choisi par le décideur n'est pas
  sur le GPU (une faute de ce côté), et dessine quand même le maillage avec
  ce qu'il a.
- **Le fond (`backdrop`) dessine les maillages du niveau plancher avec le
  plus fin de leurs propres drapés détenus, « ou aucun : un maillage du
  plancher est dessiné nu plutôt qu'omis ».** C'est le seul chemin du code
  où un maillage de sol est dessiné **sans aucun drapé**, et donc dans la
  couleur de base du matériau. C'est la piste la plus directe du sol beige.
- `smeared_drawn` compte les surfaces dont l'imagerie la plus fine est plus
  de `SMEAR_GAP` niveaux au-dessus de la tuile : c'est l'étirement à plat.

### 2.2 Ce qui est vrai aujourd'hui, mesuré (`budget_sim`)

Séquence d'acceptation (zoom → pan → tilt → rotation → zoom arrière, puis
4 s de repos), graine 101, profils natifs.
Test : `tuile-budget-sim/tests/regulator.rs`, `saso_of_the_acceptance_sequence`.

Les nombres sont des éléments·pas, sommés sur les 1 080 pas de la séquence
(main `1efa54f1`).

**Au dessin**, selon le drapé de base porté par chaque élément dessiné :

| profil | le sien (écart ≤ 1) | le sien, ≥ 2 sous le besoin | d'un ancêtre : écart 0 / 1 / 2 / ≥ 3 | aucun |
|---|---|---|---|---|
| desktop | 82 882 | 5 | 131 867 / 489 / 97 / **145** (max 7) | 0 |
| handheld | 11 779 | 0 | 13 755 / 598 / 41 / **37** (max 3) | 0 |
| web | 84 681 | 10 | 79 531 / 2 134 / 1 408 / **2 086** (max 8) | 0 |
| handheld 160 Mio | 13 197 | 0 | 5 321 / 1 547 / 24 / **42** (max 6) | 0 |

**En mémoire**, sur les maillages résidents sur le GPU :

| profil | résidents | sans drapé de leur tuile, dessinés | sans drapé de leur tuile, non dessinés | non dessinés qui seraient étalés (≥ 3 niveaux) | au dernier pas (repos), sans drapé propre |
|---|---|---|---|---|---|
| desktop | 1 109 918 | 127 050 | 693 324 | **291 353** (26 %) | 564 / 1 015 |
| handheld | 185 770 | 13 084 | 92 956 | **35 517** (19 %) | 132 / 315 |
| web | 1 155 392 | 75 887 | 756 922 | **365 277** (32 %) | 544 / 1 010 |
| handheld 160 Mio | 102 174 | 6 866 | 40 665 | **24 819** (24 %) | 23 / 109 |

Définitions :

- **Écart** : le niveau d'imagerie demandé moins le niveau porté.
- **Étalé** : le drapé le plus fin détenu, sur la tuile ou ses ancêtres, est
  au moins 3 niveaux sous le niveau du maillage. Dessiné à cet instant, le
  maillage porterait un texel étiré, le sol plat.

Ce que les tableaux disent :

1. **Un maillage dessiné porte toujours un drapé** (`bare` = 0 partout). La
   loi tient déjà au dessin, dans le décideur.
2. **Porter le drapé d'un ancêtre est l'état normal, pas un défaut.**
   - 99 % de ces cas (desktop) sont à **écart 0** : le drapé de l'ancêtre au
     niveau d'imagerie demandé, composé sur une texture plus grande
     (`max_drape_side`), a la même finesse que le drapé propre.
   - Le flou réel (écart ≥ 2) est rare : 0,1 % des éléments dessinés sur
     desktop, 2 % sur web, où l'exécuteur a un pas de retard.
   - Le pire écart vu est de 8 niveaux (web).
3. **Le défaut est en mémoire.**
   - Un quart à un tiers des maillages résidents non dessinés seraient
     étalés s'ils devenaient dessinés. Ce sont les préchargements et la
     chaîne d'ancêtres, chargés maillage seul (#364).
   - C'est ce que montre un zoom arrière ou un repli : la chaîne apparaît
     avec le drapé du plancher.
4. **Le seul « dessiné sans aucun drapé » est hors de `budget_sim`.** C'est
   le fond de l'exécuteur (`pump.rs`, §2.1). `budget_sim` ne le modélise
   pas, donc sa fréquence n'est pas mesurée ici.

### 2.3 La loi proposée, et ce qu'il reste à trancher

Proposition, en deux parties.

- **MD1 (résidence).** Tout maillage de sol résident ou en chargement a son
  propre drapé, chaque couche, résident ou en chargement avec lui :
  - le drapé est au niveau dont la tuile aura besoin une fois dessinée ;
  - le maillage et son drapé sont lancés, livrés et relâchés ensemble,
    comme le groupe du plancher (D-A1) ;
  - un préchargement qui n'a pas de place pour les deux n'est pas lancé
    (R4) ;
  - relâcher le maillage relâche ses drapés ;
  - un drapé n'est pas relâché seul tant que son maillage est résident,
    sauf s'il est remplacé par un plus fin ou par le niveau dont la tuile a
    besoin.
- **MD2 (dessin).** Un maillage de sol n'est jamais dessiné sans drapé, ni
  par le décideur (déjà vrai), ni par l'exécuteur (le fond de `pump.rs` doit
  porter au pire le drapé du plancher de son ancêtre, jamais rien).

**Ambiguïté signalée, à trancher par Laurent (Q2).** Que veut dire « son
drapé » ? Trois lectures :

- **(a) Le drapé de sa propre tuile**, et rien d'autre.
  - Au dessin, c'est intenable : 60 % des éléments dessinés portent celui
    d'un ancêtre, presque tous à la bonne finesse. L'interdire ferait des
    trous (sol noir) ou retiendrait des tuiles plus grossières, contre la
    loi absolue.
  - En mémoire, cela revient à doubler les octets d'imagerie de la chaîne.
- **(b) Un drapé à son niveau d'imagerie, le sien ou celui d'un ancêtre**,
  avec la doublure de #344 en état transitoire explicite.
  - Écart borné à G = 1 niveau, la tolérance de flou d'un geste imprévu
    (P8).
  - Au-delà de G, on ne retire pas de couverture : l'écart devient une
    urgence `Drape` (le drapé manquant passe devant les préchargements).
  - En mémoire : un maillage résident a, sur lui ou sur un ancêtre, un
    drapé à moins de G niveaux du niveau dont il aura besoin une fois
    dessiné. Sinon il est chargé avec le sien (MD1).
- **(c) La lecture (b) au dessin, mais la lecture (a) en mémoire** pour
  les préchargements : c'est ce que teste #364 (`bare_preloads` exige un
  drapé de la tuile elle-même).

Je propose (b) : c'est ce que mesurent les tableaux, et c'est elle qui
supprime les 26–32 % de résidents étalés sans doubler l'imagerie. **Je n'ai
pas tranché.**

- MD1 est implémentée en lecture (c), parce que c'est le test rouge #364
  tel qu'il est écrit.
- Passer en (b), MD2 côté exécuteur et la borne G attendent la réponse de
  Laurent.

---

## 3. Quantités, consignes, signaux, bornes

| Loi | Quantité régulée | Consigne | Signaux (entrées enregistrées) | Borne SASO |
|---|---|---|---|---|
| R1–R3 | place libre GPU par classe, `cible_c − utilisé_c` | `m_c = (1+κ)·λ_c·W_c` | octets du besoin présent lancés par pas (du besoin), `W` (Params, puis M1) | dépassement : `utilisé ≤ cible` hors besoin présent ; jamais `> dur` |
| R4 | octets de préchargement engagés (en vol + décodés non livrés) | `≤ cible_c − m_c − utilisé_c` | tailles des arrivées (moyenne par classe, des événements) | aucune tâche de préchargement décodée en attente au-delà de l'horizon |
| R5 | tâches refusées faute de place | réveil quand `libre_c ≥ m_c + b` | ledger | travail de tri par pas : celui de main (P6.4) |
| R6 | octets de préchargement admis par pas, `a_c` | AIMD : `+α` si marge tenue, `×β` sinon, `≤ libre − m` | marge tenue ou non | établissement : nombre de pas après l'arrêt du geste |
| R7 | `Q_gpu`, `Q_ram`, `Q_vol` | 0 (files vides) | manque de marge, octets en vol | files bornées (`q_max`) |
| R8 | évictions par pas | `≤ min(m_c − libre, jetons)` | jetons | pic d'évictions par pas et sur 10 pas |
| R9 | emplacements réseau du besoin présent | `⌈μ_c·W_c⌉` | départs du besoin présent par pas | un besoin présent n'attend jamais un préchargement |
| — | envois par image | plafond existant (`upload_bytes_per_frame`, I18) | plan d'envoi | pic d'envois par image ≤ celui de main |

**Bornes SASO** (critères de Laurent ; mesurées sur la séquence d'acceptation
et sur le combiné téléphone à son plancher, 160 Mio, sous pression), avant
et après :

- **dépassement** : le pire pas au-dessus de la cible, en ‰ de la cible ;
  jamais au-dessus de la limite dure ;
- **établissement** : le nombre de pas après la fin du geste jusqu'à ce que
  plus rien ne parte, ne soit évincé ni livré, et qu'aucune tuile ne reste
  sous son besoin, pour de bon ;
- **pic du taux d'éviction** : le pire pas, et la pire fenêtre de 10 pas ;
- **pic d'envois par image** : les éléments et les octets du plan.

Avec ces bornes : trous 0, gaspillage, thrash, écart de qualité en flouz et
travail par pas, par profil (desktop, handheld, web), dans le tableau de la
PR.

**Avant (main `1efa54f1`, les paliers)**, séquence d'acceptation, 1 080 pas :

| profil | dépassement | établissement (pas) | évictions : pic / pic 10 pas / total | envois : pic éléments / Kio | départs pic | sous le besoin (tuile·pas) | travail max / moyen |
|---|---|---|---|---|---|---|---|
| desktop | 0 ‰ | 238 | **421** / 488 / 6 600 | 28 / 21 845 | 61 | 1 006 | 226 610 / 17 532 |
| handheld | 0 ‰ | jamais | 67 / 67 / 294 | 13 / 5 461 | 16 | 702 | 23 612 / 2 689 |
| web | 0 ‰ | 45 | **408** / 499 / 4 714 | 29 / 21 845 | 91 | 5 162 | 186 260 / 15 693 |
| handheld 160 Mio | 0 ‰ | jamais | 48 / 56 / 661 | 13 / 4 013 | 16 | 6 021 | 17 285 / 2 685 |

Ce qu'on lit dans ce tableau :

- **Les libérations par à-coups sont mesurées.** Sur desktop, 421 évictions
  tombent en un seul pas, sur 488 dans la pire fenêtre de 10 pas : le palier
  saute d'un coup.
- **Le téléphone ne s'établit pas** dans les 4 s de repos.
- **Le dépassement est nul, mais c'est trompeur.** La somme des classes ne
  passe jamais la cible, mais c'est l'urgence qui l'obtient, pas la marge.

---

## 4. Questions ouvertes

- **Q1 (sens de « monte lentement et descend vite »).** Je l'applique au
  débit du préchargement (R6, PANDA : on sonde doucement, on recule
  vite). La consigne de marge fait l'inverse (R3 : elle monte tout de
  suite, elle redescend lentement). Appliquer la lenteur à la montée de la
  marge la ferait arriver après le geste, c'est-à-dire à l'urgence, ce que
  Laurent veut éviter. À confirmer.
- **Q2 (la règle maillage + drapé)** : voir §2.3.
- **Q3 (W avant M1).** Tant que M1 n'a pas livré les distributions de temps
  de chargement, `W_c` est un paramètre : 30 pas, soit 0,5 s à 60 Hz.
  Ensuite il devient une entrée enregistrée (le p90 mesuré par classe).
- **Q4 (les planchers du pool).** `Pool::floor_percent` (sol 25 %, drapés
  40 %) est une part fixe qui ne joue qu'en contention. Le régulateur ne la
  touche pas. À rouvrir si Laurent veut aussi l'enlever.
