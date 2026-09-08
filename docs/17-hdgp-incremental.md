# hdGp incrémental : garder le procédural vivant d'une frame à l'autre

État mesuré : **60-65 s par frame** sur la ferme, pour une image que le GPU
dessine en millisecondes. La charge est purement CPU et purement du travail
refait. Ce document établit, source d'OpenUSD à l'appui, comment hdGp est
*censé* fonctionner, ce que notre chemin viole, et la liste exacte des
modifications pour passer en incrémental — en gardant Blender.

## 1. Le contrat hdGp, tel qu'il est écrit dans la source

### 1.1 L'instance est réutilisée — c'est le postulat de tout le reste

`generativeProceduralResolvingSceneIndex.cpp:709` :

```cpp
if (!procEntry.proc || procType != procEntry.typeName) {
    proc.reset(_ConstructProcedural(procType, proceduralPrimPath));  // 1re fois
} else {
    proc = procEntry.proc;                                           // réutilisé
}
```

Le resolver tient une carte `_procedurals` : **une instance par (resolver,
chemin de prim)**, construite une fois et réutilisée à chaque cook. Sa durée
de vie est celle du resolver — donc, chez un hôte normal (usdview, usdrecord,
Houdini), **toute la durée du rendu**.

### 1.2 `previousResult` est la carte du cook précédent, tenue par le resolver

`_UpdateProcedural` passe `procEntry.childTypes` en deuxième argument
d'`Update`. Ce n'est pas une commodité décorative : c'est l'état qui permet de
ne produire que le delta.

### 1.3 Le diff que fait le resolver — et le prix de chaque cas

`_UpdateProceduralResult` (ligne 1027) compare l'ancienne carte à la nouvelle :

| Cas | Notice émise | Coût |
|---|---|---|
| Même chemin, même type | **aucune** (« previously existed and type is the same, do nothing ») | **zéro** — `GetChildPrim` n'est même pas rappelé |
| Chemin nouveau, ou type changé | `PrimsAdded` | le renderer tire la data source |
| Chemin disparu | `PrimsRemoved` | le renderer libère |

Autrement dit : **une tuile qui reste dans la sélection et garde son chemin ne
coûte rien**, à condition que le procédural la re-déclare à l'identique. C'est
là que se trouvent les soixante secondes.

Pour une donnée qui change sur un prim *conservé*, le seul canal est le
quatrième argument d'`Update` : `outputDirtiedPrims`, avec les locators
concernés.

### 1.4 `GetChildPrim` est appelé à la demande, depuis plusieurs threads

La doc de l'API le dit explicitement. Conséquence pour nous : la data source
d'une tuile doit rester servable **à tout moment**, longtemps après le cook qui
l'a produite — et sans verrou.

### 1.5 Une API asynchrone existe

`AsyncBegin(bool)` / `AsyncUpdate(outputDirtiedPrims)` / `AsyncState
{Continuing, Finished, ..NewChanges}` : cook en tâche de fond avec polling,
conçu pour du streaming. Le resolver l'appelle dès la construction
(ligne 717). Hors sujet pour un rendu batch (on veut le contrat eager-fatal),
mais c'est la voie pour un futur viewport interactif.

## 2. Ce que notre chemin viole

### 2.1 Blender détruit le resolver à chaque frame

Notre patch `usd_scene_delegate.cc::populate()` est appelé par frame et fait
`RemoveSceneIndex` puis reconstruit toute la chaîne. Le resolver meurt, sa
carte `_procedurals` meurt, le `shared_ptr` lâche l'instance. Frame suivante :
instance neuve, `previousResult` vide, **250 tuiles re-déclarées comme
nouvelles**, `Session` rouverte (ion re-résolu, résidence perdue).

### 2.2 Le fait décisif : un *resync* tue l'instance, un *dirty* la préserve

`stageSceneIndex.cpp:685`, dans le traitement des resyncs :

```cpp
removedPrims.emplace_back(primPath);
_PopulateSubtree(prim, &addedPrims);
```

Un resync émet **`PrimsRemoved` puis `PrimsAdded`**. Or `_PrimsRemoved` →
`_RemoveProcedural` → l'entrée et l'instance sont effacées. Un simple dirty de
propriété, lui, ne touche pas à l'entrée.

Corollaire, vérifié dans le code : un `PrimsAdded` **seul** sur un prim
procédural existant ne détruit rien — le resolver marque « full invalidation »
et re-cuit, mais `procEntry.proc` et `procEntry.childTypes` survivent. C'est
`SetStage()` qui est fatal, parce qu'il commence par
`_SendPrimsRemoved({"/"})`.

### 2.3 Notre `Update` ignore tout ce qui précède

Il vide `_tilesByPath`, libère la frame précédente, et reconstruit chaque data
source à chaque cook. Il ne lit jamais `previousResult`, n'écrit jamais dans
`outputDirtiedPrims`. Et `GetChildPrim` lit `_frame`, qui est libérée au cook
suivant — un prim conservé par le resolver deviendrait donc **inservable**, ce
que seule notre destruction totale par frame masque aujourd'hui.

## 3. Ce que fait l'état de l'art sur le même problème

Cesium pour Omniverse streame du 3D Tiles dans Hydra et **ne reconstruit pas de
prims USD par frame** : la géométrie est écrite dans Fabric/USDRT
(représentation post-composée, accès GPU direct) avec un pool d'objets, et
n'est mise à jour qu'en delta. Le principe transposable n'est pas « utiliser
Fabric » — c'est que **l'état de streaming vit au niveau du process et n'est
jamais reconstruit ; seul le delta circule.**

## 4. Les modifications à réaliser

### É1 — Mémo de textures (fait, commit `e7c1f71`)

Le bake et l'encodage PNG d'un drapage ne sont plus refaits. Nécessaire dans
toutes les architectures, insuffisant à lui seul : il supprime le coût le plus
lourd, pas la reconstruction.

### É2 — Un scene index à nous, persistant, alimenté par dirty

Le cœur du correctif, et le seul moyen de garder Blender.

Aujourd'hui, notre prim Globe voyage dans le stage que Blender ré-exporte, donc
il subit le resync. Demain, il n'y voyage plus :

- à la **construction** du délégué (une fois), créer un `HdRetainedSceneIndex`
  contenant le prim `Globe` (type `hydraGenerativeProcedural`, ses primvars de
  config) et un prim caméra miroir ; l'envelopper dans le
  `HdGpGenerativeProceduralResolvingSceneIndex` ; l'insérer dans le render
  index. **Jamais retiré.**
- à chaque `populate()`, ne pas y toucher sauf pour **dirtier le locator xform
  de la caméra miroir** avec la matrice de la frame courante (et le temps).
  C'est un dirty de propriété : aucun resync, l'instance survit.
- Blender continue d'exporter sa scène (lumières, sol) dans son propre scene
  index, fusionné par le render index comme aujourd'hui.

Le resolver appelle alors `Update` avec `dirtiedDependencies` = la caméra, sur
**la même instance**, avec `previousResult` intact.

### É3 — Rendre `Update` incrémental

- Tenir `_tiles: map<SdfPath, TileEntry>` **à travers les cooks**, où
  `TileEntry` porte la data source déjà construite et la clé de drapage.
- À chaque cook : comparer la nouvelle sélection à `previousResult`.
  - tuile absente de la nouvelle sélection → ne pas la remettre dans la carte
    (le resolver émettra `PrimsRemoved`) et libérer son entrée ;
  - tuile déjà présente et drapage inchangé → la remettre **à l'identique** :
    aucune notice, aucun travail ;
  - tuile nouvelle → construire sa data source (une fois) et l'ajouter ;
  - tuile présente mais **re-drapée** → même chemin, même type : le resolver
    ne signalera rien tout seul. Il faut pousser un dirty explicite du locator
    matériau dans `outputDirtiedPrims`, et changer l'URI de la texture (voir
    É4) pour que le renderer recharge réellement les pixels.
- Construire les data sources **pendant `Update`**, puis libérer la
  `TuileFrame` : `GetChildPrim` devient une lecture de carte, sans verrou et
  sans dépendance à une frame libérée (le défaut latent du § 2.3).

### É4 — URI de texture adressée par contenu

`tuile://<dataset>/tile/<id>/texture/0.png` est stable alors que ses octets
changent quand la tuile est re-drapée : n'importe quel cache d'assets côté
renderer servira l'ancienne image. L'URI doit porter l'empreinte du drapage —
la même que celle du mémo — pour qu'un changement de niveau soit un
changement d'asset.

### É5 — Session partagée au niveau du process

Filet de sécurité : tant qu'un hôte peut détruire l'instance (et Blender le
fera encore lors d'un vrai changement de scène), la `Session` — résidence des
tuiles, mémo de textures, pool HTTP — doit survivre dans un registre de
process clé = config. Sans É2 c'est le minimum vital ; avec É2 c'est de la
robustesse.

## 5. Vérification

- Instrumentation déjà en place : `TUILE_LOG=tuile_hydra=info` imprime
  `tiles=N reused=M` par frame. Après É1+É2+É3, sur une orbite, `reused` doit
  approcher `tiles` et le temps par frame tomber d'un ordre de grandeur.
- Un compteur de cooks par instance (`TF_DEBUG=TUILE_HYDRA_PROCEDURAL`) doit
  montrer **une seule construction** pour toute la séquence : c'est la preuve
  directe que l'instance survit.
- Porte visuelle inchangée : les routes traversent les frontières de tuiles.
