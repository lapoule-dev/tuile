// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// Cooks a manifest over N frames and reports what each cook actually did —
// no renderer, no window, no GPU.
//
// The C++ half of this plugin had no way to be tested at all: every claim
// about it went through a farm render, which costs minutes and money and
// answers "the picture looks right" rather than "the work was skipped". This
// answers the second question, which is the one the incremental work is about:
//
//   * how many procedural INSTANCES were constructed (hdGp keeps one per prim
//     for a whole render — more than one means the host is rebuilding its
//     scene index, and previousResult is useless);
//   * how many tiles each cook KEPT, built, re-draped or dropped.
//
// Run it with TF_DEBUG=TUILE_HYDRA_PROCEDURAL to see both counters:
//
//   TUILE_ION_TOKEN=… TF_DEBUG=TUILE_HYDRA_PROCEDURAL \
//   PXR_PLUGINPATH_NAME=<build>/plugin/tuileHydra/resources \
//   HDGP_INCLUDE_DEFAULT_RESOLVER=1 \
//   frameProbe manifest.usda 3
//
// The chain it builds is the one the Blender patch builds — same resolver, in
// the same place (upstream of the flattening, via overridesSceneIndexCallback)
// — but it is built ONCE and then only advanced in time, which is what every
// well-behaved Hydra host does and what Blender's USD export path does not.

#include <pxr/usd/usd/stage.h>
#include <pxr/usd/usd/timeCode.h>
#include <pxr/usdImaging/usdImaging/sceneIndices.h>
#include <pxr/usdImaging/usdImaging/stageSceneIndex.h>
#include <pxr/imaging/hdGp/generativeProceduralResolvingSceneIndex.h>
#include <pxr/imaging/hd/sceneIndex.h>

#include <algorithm>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <string>

PXR_NAMESPACE_USING_DIRECTIVE

namespace {

/// Pulls every prim of a subtree, which is what forces a procedural to cook
/// and its children to be built. A renderer does this through its own
/// population; here it is explicit so the probe measures the same work.
size_t
Pull(const HdSceneIndexBaseRefPtr &scene, const SdfPath &path)
{
    size_t pulled = 0;
    HdSceneIndexPrim prim = scene->GetPrim(path);
    if (prim.dataSource) {
        // Touch the container so lazily-built data sources are actually built.
        prim.dataSource->GetNames();
        ++pulled;
    }
    for (const SdfPath &child : scene->GetChildPrimPaths(path)) {
        pulled += Pull(scene, child);
    }
    return pulled;
}

}  // namespace

int
main(int argc, char **argv)
{
    if (argc < 2) {
        std::fprintf(stderr, "usage: frameProbe <stage.usda> [frames]\n");
        return 2;
    }
    const std::string stagePath = argv[1];
    const int frames = argc > 2 ? std::atoi(argv[2]) : 3;

    UsdStageRefPtr stage = UsdStage::Open(stagePath);
    if (!stage) {
        std::fprintf(stderr, "PROBE-FAIL cannot open %s\n", stagePath.c_str());
        return 1;
    }

    // Built once, exactly as a host that does not throw its scene away.
    UsdImagingCreateSceneIndicesInfo info;
    info.overridesSceneIndexCallback =
        [](HdSceneIndexBaseRefPtr const &input) -> HdSceneIndexBaseRefPtr {
        return HdGpGenerativeProceduralResolvingSceneIndex::New(input);
    };
    UsdImagingSceneIndices indices = UsdImagingCreateSceneIndices(info);
    const HdSceneIndexBaseRefPtr terminal = indices.finalSceneIndex;
    indices.stageSceneIndex->SetStage(stage);

    const double start = stage->GetStartTimeCode();
    const double end = stage->GetEndTimeCode();
    for (int i = 0; i < frames; ++i) {
        // CONSECUTIVE timecodes, because that is what a render does. Spreading
        // the samples over the whole range moves the camera further between
        // cooks than any frame ever will, and then reports as "rebuilt" tiles
        // that a real sequence would have kept — flattering the old behaviour
        // and hiding the new one.
        const double t = std::min(start + double(i), end);
        const auto began = std::chrono::steady_clock::now();
        indices.stageSceneIndex->SetTime(UsdTimeCode(t));
        indices.stageSceneIndex->ApplyPendingUpdates();
        const size_t pulled = Pull(terminal, SdfPath::AbsoluteRootPath());
        const double seconds =
            std::chrono::duration<double>(std::chrono::steady_clock::now() - began)
                .count();
        std::printf("PROBE-FRAME %d time=%g prims=%zu seconds=%.2f\n",
                    i + 1, t, pulled, seconds);
        std::fflush(stdout);
    }
    std::printf("PROBE-DONE\n");
    return 0;
}
