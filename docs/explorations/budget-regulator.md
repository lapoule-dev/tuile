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

Statut : **lois écrites (révisées le 28 nov. : un seul mécanisme, l'enveloppe), implémentation en cours** sur `feat/budget-regulator`.

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

## 1. Les lois : un seul mécanisme

L'objectif est celui de Laurent (28 nov.) : **un préchargement n'a jamais
d'effet sur la vue principale, ni en RAM, ni en charge GPU.** Il est tenu
par un mécanisme unique, l'**enveloppe** : le préchargement ne vit que dans
ce que le besoin présent n'utilise pas, réserve comprise. Il n'y a ni
paliers, ni parts fixes, ni files virtuelles, ni seconde boucle.

Les lois absolues restent au-dessus de tout : le sol n'est jamais noir ; une
tuile dessinée, sa chaîne de repli et le plancher épinglé ne sont jamais
retirés ; le nouveau est actif avant que l'ancien parte.

**Vocabulaire.**

- Le **besoin présent**, c'est ce qui est dessiné, protégé (groupes
  d'échange, transit, plancher) ou voulu à une urgence de la vue : `Floor`,
  `Hole`, `Blocker`, `Drape`, et la chaîne d'ancêtres qui sert de repli.
- Un **préchargement**, c'est une copie voulue à une urgence de prévision
  (un candidat du score de préchargement, un rafraîchissement, les drapés
  d'un ancêtre) et non dessinée. Le régulateur ne connaît pas la forme du
  voisinage d'où vient le candidat. Il ne voit qu'une entrée : une
  ressource, sa classe, sa taille et sa clé (valeur, échéance). Le plancher
  atteignable peut donc fournir les candidats sans que rien ne change ici.
- Le **cache**, ce sont les copies que plus rien ne veut. Toute admission
  les prend en premier, comme aujourd'hui.

**Les lois.**

- **E1. L'enveloppe.** Pour chaque ressource `d` :
  - les octets GPU de chaque classe du pool ;
  - les octets décodés en RAM ;
  - le crédit de livraison, c'est-à-dire les octets en route vers le GPU,
    donc la charge d'envoi.

  À chaque pas, `présent_d + préchargé_d + réserve_d ≤ capacité_d`, où la
  capacité est la cible du pas (la part que le pool laisse à la classe, la
  cible du décodé, le crédit).
- **E2. La réserve, c'est ce que le besoin présent a demandé et pas encore
  reçu, et elle vaut zéro au repos.** C'est le `L` de la loi de Little (le
  travail dans le système), compté directement plutôt qu'estimé par
  `λ · W` : ses chargements classés et en vol, à leur taille estimée (la
  moyenne apprise des livraisons de la classe), et ses copies décodées non
  livrées, à leur coût exact. Un ancêtre en file attend l'admission du
  score comme un préchargement : il compte une fois en vol.

  Le filtre monte d'un coup et redescend d'une fraction par pas
  (`fall_permille`). La place est donc prise dès que le geste demande, et
  elle n'est pas rendue au préchargement entre deux à-coups du même geste.
  *Révision, mesure à l'appui :* la première version (`λ · W`, `W` = 30 pas)
  réservait d'emblée 45 Mo sur une classe de 4 Mio dès la première
  livraison ; la réserve ne savait pas quand le besoin était servi.
  Compter `L` n'a plus besoin de `W`, donc plus besoin de M1 pour ce nombre.
- **E3. Le préchargement n'entre que dans l'enveloppe, et en sort le
  premier.**
  - *Entrer* : un préchargement est lancé, livré ou envoyé seulement si ses
    octets, estimés au lancement puis exacts à la livraison, tiennent dans
    l'enveloppe de chaque ressource qu'il occupera. Ce qu'on lance est donc
    livrable.
  - *Attendre* : un préchargement décodé que l'enveloppe refuse sort de la
    file et n'est plus classé. Il revient quand la place revient dans sa
    classe.
  - *Sortir* : quand le besoin présent a des octets en attente, le
    balayage fait sa place d'avance, jusqu'à `cible − en attente` (sans
    filtre, et décompté à chaque livraison du pas). Les préchargements y
    passent outre la loi d'horizon (raison `envelope`), le cache sous la
    loi. Une fois le besoin servi, plus rien n'est retiré : la ligne
    d'entrée (`cible − réserve`, filtrée) et la ligne de sortie
    (`cible − en attente`) forment l'hystérésis qui remplace les paliers,
    tirée d'un seul signal. Retirer à la ligne filtrée relâchait un
    préchargement pour des octets déjà livrés (mesuré : un candidat préchargé évincé
    au pas même où le besoin arrivait, puis relivré au suivant).
  - *Échanger* : au lancement, les préchargements évinçables (candidats du score) ne
    comptent pas ; un préchargement plus fort que la marge prend la place
    d'un plus faible à la livraison, où E1 est tenue.
- **E4. Un préchargement arrive avec son drapé** (§2, lecture (c)) : un
  maillage préchargé veut le drapé de sa tuile au niveau dont il aura besoin
  une fois dessiné. Les deux passent ensemble par E3.

**Ce qui découle de ces lois sans en être une de plus :**

- la loi 8 du spec (un besoin présent ne passe jamais derrière un
  préchargement) ;
- la fin des paliers (`Watermarks` disparaît) ;
- la fin des parts fixes de mémoire.

**Les nombres.** Un seul : `envelope.fall_permille` (100 ‰, une demi-vie
d'environ sept pas), dans `Params`, donc dans la spécification, son digest
et sa requête (`envelope_fall`). Il n'y a aucun interrupteur. Le régulateur est en entiers, ses
signaux viennent des entrées enregistrées, et aucune mesure de l'exécuteur
n'entre dans `decide`.

**Écarté, et pourquoi.** Le plan citait plusieurs mécanismes :
drift-plus-penalty avec V, AIMD (PANDA), seau à jetons (Kalyvianaki). Je
les écarte : chacun ajoute un état et un réglage pour un effet que
l'enveloppe donne déjà.

- La réserve filtrée anticipe, et c'est elle qui lisse les libérations.
- Un préchargement relâché ne coûte rien à la vue : il n'est pas dessiné.
  Le seul coût est le rechargement, que le thrash et le gaspillage mesurent.

Si les mesures SASO montrent un pic d'évictions que la réserve ne lisse
pas, un plafond par pas viendra *dans* E3. Ce ne sera pas une boucle de
plus.

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
- Un candidat du score de préchargement ou une tuile de la chaîne
  d'ancêtres se charge **maillage
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
    (E3) ;
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

## 3. Propriétés testables

| # | Propriété | Où elle est testée |
|---|---|---|
| T1 | **Invariant dur, à chaque pas** : pour chaque ressource, les octets préchargés ≤ `capacité − présent − réserve` | vérifié à chaque pas de `budget_sim` (`server.rs`, `check`), donc dans tous les scénarios |
| T2 | Un besoin présent n'est jamais refusé ni retardé faute de place prise par un préchargement : quand une livraison présente est bloquée, aucun préchargement évinçable n'est sur le GPU de sa classe | `check` de `budget_sim` |
| T3 | Charge GPU : les envois et le crédit de livraison des préchargements tiennent dans ce que le présent laisse, et un plan d'envoi ne commence jamais par un préchargement s'il dépasse le plafond de l'image | `check` de `budget_sim` |
| T4 | Propriétés sur des scripts aléatoires de caméra et de pression (proptest) : T1–T3 et zéro trou | `budget_properties` |
| T5 | Avec ou sans préchargement, la vue présente est identique ou meilleure, jamais pire : trous, éléments dessinés exacts, p95 de l'attente des besoins | `tuile-budget-sim/tests/regulator.rs` |
| T6 | SASO sur la séquence agressive, avant/après : dépassement, établissement en pas, pic d'évictions, pic d'envois par image | `tuile-budget-sim/tests/regulator.rs` |
| T7 | #364 : un préchargement porte ses drapés | `budget_preload_imagery` |
| T8 | Mutations : retirer chaque loi (E1–E4, chacune des trois ressources) fait échouer au moins un test | tableau de la PR |

Les bornes SASO (critères de Laurent) se mesurent sur la séquence
d'acceptation et sur le téléphone à son plancher (160 Mio, sous pression) :

- **dépassement** : le pire pas au-dessus de la cible, en ‰ de la cible ;
  jamais au-dessus de la limite dure ;
- **établissement** : le nombre de pas après la fin du geste jusqu'à ce que
  plus rien ne parte, ne soit évincé ni livré, et qu'aucune tuile ne reste
  sous son besoin, pour de bon ;
- **pic du taux d'éviction** : le pire pas, et la pire fenêtre de 10 pas ;
- **pic d'envois par image** : les éléments et les octets du plan.

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

- **Q1 (sens de « monte lentement et descend vite »).** Pour la place du
  préchargement, c'est exactement E2 : la réserve monte vite, donc la place
  du préchargement descend vite ; la réserve redescend lentement, donc
  cette place remonte lentement. Si la phrase visait la réserve elle-même,
  la réserve arriverait après le geste, à l'urgence. À confirmer par
  Laurent.
- **Q2 (la règle maillage + drapé)** : voir §2.3.
- **Q3 (W avant M1).** Sans objet depuis la révision d'E2 : la réserve
  compte `L` au lieu d'estimer `λ · W`. M1 reste utile pour la taille
  estimée d'un chargement et pour le crédit de livraison (T3).
- **Q4 (les planchers du pool).** `Pool::floor_percent` (sol 25 %, drapés
  40 %) est une part fixe qui ne joue qu'en contention. Le régulateur ne la
  touche pas. À rouvrir si Laurent veut aussi l'enlever.
