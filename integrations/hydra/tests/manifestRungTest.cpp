// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// Le globe entre-t-il dans la scène, et assez tôt pour être cuit ?
//
// C'est la seule question que pose `manifest.cpp`, et elle a exactement deux
// façons de rater en silence :
//
//   - le manifeste n'est pas fusionné du tout — le rendu sort sans globe, et
//     rien n'émet d'erreur puisque personne n'a demandé de prim absente ;
//   - il est fusionné TROP TARD, après le resolver hdGp, et la prim procédurale
//     reste une prim procédurale que personne ne cuit. Même image vide, même
//     silence.
//
// Les deux se voyaient jusqu'ici en regardant un rendu de ferme. Elles se
// voient ici en un dixième de seconde, parce que la registry expose l'ordre
// dans lequel elle fera tourner ses greffons, et parce que la chaîne complète
// se construit sans renderer.
//
// Le test a besoin de `PXR_PLUGINPATH_NAME` sur le répertoire du greffon
// construit ; CTest le lui pose (voir `CMakeLists.txt`).

#include <pxr/imaging/hd/retainedDataSource.h>
#include <pxr/imaging/hd/retainedSceneIndex.h>
#include <pxr/imaging/hd/sceneIndexPluginRegistry.h>
#include <pxr/imaging/hd/tokens.h>

#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <string>
#include <vector>

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

/// Un manifeste minimal, écrit à côté du binaire de test.
///
/// Une prim ordinaire plutôt qu'une `GenerativeProcedural` : cuire la vraie
/// demanderait un pack et un réseau, alors que ce qu'on vérifie ici — est-elle
/// dans la scène, et avant hdGp — ne dépend pas de ce qu'elle est.
std::string
_WriteManifest()
{
    const char *dir = getenv("TMPDIR");
    std::string path = std::string(dir && dir[0] ? dir : "/tmp") + "/tuile-manifest-test.usda";
    std::ofstream out(path);
    out << "#usda 1.0\n"
        << "(\n    defaultPrim = \"World\"\n)\n\n"
        << "def Xform \"World\"\n{\n"
        << "    def Sphere \"Marker\"\n    {\n        double radius = 1\n    }\n"
        << "}\n";
    return path;
}

/// La scène telle que l'hôte la présente au moment où la chaîne se construit.
HdRetainedSceneIndexRefPtr
_Host()
{
    HdRetainedSceneIndexRefPtr scene = HdRetainedSceneIndex::New();
    scene->AddPrims({{SdfPath("/freeCamera"),
                      HdPrimTypeTokens->camera,
                      HdRetainedContainerDataSource::New(
                          TfToken("présent"),
                          HdRetainedTypedSampledDataSource<bool>::New(true))}});
    return scene;
}

bool
_Has(const HdSceneIndexBaseRefPtr &scene, const char *path)
{
    HdSceneIndexPrim prim = scene->GetPrim(SdfPath(path));
    return prim.dataSource && !prim.dataSource->GetNames().empty();
}

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

    const std::string manifest = _WriteManifest();

    // `LoadAndGetSceneIndexPluginIds` rend les identifiants DANS L'ORDRE où
    // ils tourneront. Elle ne dit rien de ce que chacun fera : le greffon y
    // figure même sans `TUILE_MANIFEST`, puisqu'il décide dans
    // `_AppendSceneIndex` et non à l'enregistrement — le seul endroit où les
    // USD 26.03 et 26.08 s'accordent. L'inertie se mesure donc sur la SCÈNE,
    // et l'ordre sur cette liste.
    auto chain = [] {
        return HdSceneIndexPluginRegistry::GetInstance()
            .LoadAndGetSceneIndexPluginIds(/* renderer */ "", /* app */ "");
    };
    auto rank = [](const std::vector<TfToken> &ids, const char *id) {
        for (size_t i = 0; i < ids.size(); ++i) {
            if (ids[i] == TfToken(id)) {
                return long(i);
            }
        }
        return -1L;
    };

    // 1. Sans la variable, le greffon ne fait rien et n'est même pas dans la
    //    chaîne. C'est la promesse sur laquelle un greffon enregistré pour TOUS
    //    les renderers est acceptable : il ne change rien à un hôte qui ne l'a
    //    pas demandé.
    {
        unsetenv("TUILE_MANIFEST");
        const HdSceneIndexBaseRefPtr chained =
            HdSceneIndexPluginRegistry::GetInstance()
                .AppendSceneIndicesForRenderer("", _Host());
        check(!_Has(chained, "/World/Marker"),
              "sans TUILE_MANIFEST, rien n'est fusionné");
        check(_Has(chained, "/freeCamera"),
              "sans TUILE_MANIFEST, la scène de l'hôte passe intacte");
    }

    setenv("TUILE_MANIFEST", manifest.c_str(), 1);

    // 2. L'ordre. C'est la propriété que `GetInsertionPhase` revendique, et la
    //    seule qui ne se lise sur aucune image : un manifeste fusionné après
    //    hdGp donne exactement la même scène vide qu'un manifeste absent.
    {
        const std::vector<TfToken> ids = chain();
        const long ours = rank(ids, "TuileManifestSceneIndexPlugin");
        const long resolver = rank(ids, "HdGpSceneIndexPlugin");
        check(ours >= 0, "le greffon du manifeste est dans la chaîne");
        check(resolver >= 0, "le resolver hdGp est dans la même chaîne");
        check(ours >= 0 && resolver >= 0 && ours < resolver,
              "le manifeste est fusionné AVANT la résolution des procéduraux");
    }

    // 3. Les deux scènes sont là. Les deux moitiés comptent : fusionner le
    //    manifeste en perdant l'hôte donnerait un globe sans caméra, donc une
    //    sélection au centre de la Terre.
    {
        const HdSceneIndexBaseRefPtr chained =
            HdSceneIndexPluginRegistry::GetInstance()
                .AppendSceneIndicesForRenderer("", _Host());
        check(_Has(chained, "/World/Marker"),
              "avec TUILE_MANIFEST, la prim du manifeste est dans la scène");
        check(_Has(chained, "/freeCamera"),
              "avec TUILE_MANIFEST, la scène de l'hôte est toujours là");
    }

    printf(failures ? "\n%d test(s) en échec\n" : "\ntous les tests passent\n",
           failures);
    return failures ? 1 : 0;
}
