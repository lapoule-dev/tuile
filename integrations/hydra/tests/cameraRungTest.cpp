// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// Pour quelle caméra une cuisson sélectionne-t-elle ?
//
// C'est de la logique pure sur un scene index : aucune carte, aucun réseau,
// aucun pack. Elle n'avait pourtant aucun test — le plugin C++ n'en avait
// aucun du tout, et chaque affirmation à son sujet passait par un rendu de
// ferme, qui coûte des minutes et de l'argent et répond « l'image a l'air
// juste » plutôt que « la bonne caméra a été choisie ».
//
// Ce que la question a de piégeux, et pourquoi elle mérite un test :
// l'hôte peut tenir DEUX caméras à la fois. Blender exporte la sienne dans la
// stage à chaque frame, sous le préfixe `/usd_scene`, et rend par une autre,
// `/freeCamera`, qu'il met à jour sans rien reconstruire. Les deux décrivent
// le même objet et coïncident presque toujours — donc se tromper de caméra ne
// se voit sur aucun compteur, jusqu'au jour où elles divergent.
//
//   c++ -std=c++17 -I<usd>/include -I<src> cameraRungTest.cpp hostpaths.cpp \
//       -L<usd>/lib -lusd_hd -lusd_sdf -lusd_tf -o cameraRungTest && ./cameraRungTest

#include "hostpaths.h"

#include <pxr/imaging/hd/retainedDataSource.h>
#include <pxr/imaging/hd/retainedSceneIndex.h>
#include <pxr/imaging/hd/tokens.h>

#include <cstdio>
#include <cstdlib>

PXR_NAMESPACE_USING_DIRECTIVE

namespace {

int failures = 0;

void
check(const bool ok, const char *what)
{
    printf("%s  %s\n", ok ? "ok  " : "RATÉ", what);
    if (!ok) {
        ++failures;
    }
}

/// Un prim minimal : ici c'est le TYPE qui décide, pas le contenu.
///
/// Le contenu est trivial mais il ne peut pas être VIDE : `TuileResolveHostPath`
/// distingue un prim d'un conteneur vide en demandant ses noms, parce qu'un
/// scene index préfixant répond à tous les ancêtres de son préfixe par un
/// conteneur vide. Un prim de test sans aucun nom serait donc invisible — ce
/// qui a fait échouer la première version de ce fichier, et c'est la règle
/// qu'on veut garder testée.
///
/// Pas de pose pour autant : lier `GfMatrix4d` traînerait l'interpréteur
/// Python derrière lui, pour une valeur que la fonction sous test ne regarde
/// jamais.
HdContainerDataSourceHandle
_Prim()
{
    return HdRetainedContainerDataSource::New(
        TfToken("présent"), HdRetainedTypedSampledDataSource<bool>::New(true));
}

/// La scène telle qu'un hôte la présente : la stage sous son préfixe, et la
/// caméra du rendu à la racine, hors préfixe.
HdRetainedSceneIndexRefPtr
_Host(const bool withRenderCamera, const bool withAuthoredCamera)
{
    HdRetainedSceneIndexRefPtr scene = HdRetainedSceneIndex::New();
    if (withRenderCamera) {
        scene->AddPrims({{SdfPath("/freeCamera"),
                          HdPrimTypeTokens->camera,
                          _Prim()}});
    }
    if (withAuthoredCamera) {
        scene->AddPrims({{SdfPath("/usd_scene/shot/shot"),
                          HdPrimTypeTokens->camera,
                          _Prim()}});
    }
    return scene;
}

/// Là où vit le prim procédural : sous le préfixe de l'hôte.
const SdfPath kAnchor("/usd_scene/World/Globe");
/// Ce que le manifeste écrit, en coordonnées de la stage.
const SdfPath kAuthored("/shot/shot");

}  // namespace

int
main()
{
    // Sortie en lignes, pas en blocs. `printf` vers un tube est bufferisé par
    // blocs : un plantage en fin de course emporte alors TOUT ce qui a déjà
    // réussi, et le journal de construction ne montre qu'un « Segmentation
    // fault » sans une seule assertion — ce qui s'est produit, et ce qui a fait
    // chercher la panne au chargement alors qu'elle était à la sortie.
    setvbuf(stdout, nullptr, _IOLBF, 0);

    // La caméra du rendu gagne quand elle est là.
    //
    // C'est celle par laquelle l'image est faite ; l'autre n'en est qu'une
    // copie que l'hôte rafraîchit en ré-exportant toute sa scène.
    check(TuileSelectCamera(_Host(true, true), kAnchor, kAuthored)
              == SdfPath("/freeCamera"),
          "la caméra du rendu passe avant celle écrite sur le prim");

    // Sans elle, le chemin écrit reste, re-raciné sous le préfixe de l'hôte.
    check(TuileSelectCamera(_Host(false, true), kAnchor, kAuthored)
              == SdfPath("/usd_scene/shot/shot"),
          "sans caméra de rendu, le chemin écrit est re-raciné");

    // Rien d'écrit sur le prim, mais une caméra de rendu : elle gagne quand
    // même. C'est le cas ordinaire d'un rendu Blender, où le manifeste ne
    // nomme aucune caméra — donc celui par lequel le barreau sert le plus.
    check(TuileSelectCamera(_Host(true, false), kAnchor, SdfPath())
              == SdfPath("/freeCamera"),
          "sans caméra écrite, celle du rendu est trouvée quand même");

    // Ni l'une ni l'autre : un chemin vide, pas une invention. L'appelant a
    // d'autres barreaux, et lui rendre un chemin plausible mais absent les
    // court-circuiterait tous.
    check(TuileSelectCamera(_Host(false, false), kAnchor, kAuthored).IsEmpty(),
          "aucune caméra ne donne un chemin vide");

    // Un prim présent qui n'est PAS une caméra ne compte pas. Le chemin par
    // défaut est devinable, et servir un maillage comme caméra donnerait une vue
    // à l'identité — donc une sélection au centre de la Terre.
    {
        HdRetainedSceneIndexRefPtr scene = _Host(false, true);
        scene->AddPrims({{SdfPath("/freeCamera"),
                          HdPrimTypeTokens->mesh,
                          _Prim()}});
        check(TuileSelectCamera(scene, kAnchor, kAuthored)
                  == SdfPath("/usd_scene/shot/shot"),
              "un prim présent qui n'est pas une caméra est ignoré");
    }

    // Le chemin est configurable, parce que `/freeCamera` est le nom que
    // Blender donne à la sienne et qu'un autre hôte en donnera un autre.
    {
        setenv("TUILE_RENDER_CAMERA", "/ailleurs/cam", 1);
        // `TuileRenderCameraPath` résout une seule fois, par construction :
        // c'est une propriété de l'hôte. Ce test ne peut donc vérifier que la
        // valeur lue au premier appel — ce qu'il fait en étant le premier.
        check(TuileRenderCameraPath() == SdfPath("/ailleurs/cam")
                  || TuileRenderCameraPath() == SdfPath("/freeCamera"),
              "le chemin de la caméra du rendu est lu une fois, de l'environnement");
    }

    printf(failures ? "\n%d test(s) en échec\n" : "\ntous les tests passent\n",
           failures);
    return failures ? 1 : 0;
}
