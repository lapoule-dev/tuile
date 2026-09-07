// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The smallest possible HdGpGenerativeProcedural: one hard-coded cube mesh
// child. Its only job is to prove, end to end, that a generative procedural
// cooks inside a host's Hydra pipeline (Blender's USD export mode first).
// It is deliberately shaped like the future TuileGlobe procedural: same
// registry, same plugInfo, same child-prim data sources.

#include "pxr/imaging/hdGp/generativeProcedural.h"
#include "pxr/imaging/hdGp/generativeProceduralPlugin.h"
#include "pxr/imaging/hdGp/generativeProceduralPluginRegistry.h"

#include "pxr/imaging/hd/meshSchema.h"
#include "pxr/imaging/hd/meshTopologySchema.h"
#include "pxr/imaging/hd/primvarsSchema.h"
#include "pxr/imaging/hd/primvarSchema.h"
#include "pxr/imaging/hd/purposeSchema.h"
#include "pxr/imaging/hd/retainedDataSource.h"
#include "pxr/imaging/hd/tokens.h"
#include "pxr/imaging/hd/visibilitySchema.h"
#include "pxr/imaging/hd/xformSchema.h"

#include "pxr/base/gf/matrix4d.h"
#include "pxr/base/vt/array.h"

PXR_NAMESPACE_USING_DIRECTIVE

namespace {

const TfToken _cubeName("provedCube");

class CubeProcedural final : public HdGpGenerativeProcedural
{
public:
    explicit CubeProcedural(const SdfPath &proceduralPrimPath)
        : HdGpGenerativeProcedural(proceduralPrimPath)
    {
    }

    DependencyMap UpdateDependencies(const HdSceneIndexBaseRefPtr &) override
    {
        return DependencyMap();
    }

    ChildPrimTypeMap Update(
        const HdSceneIndexBaseRefPtr &,
        const ChildPrimTypeMap &,
        const DependencyMap &,
        HdSceneIndexObserver::DirtiedPrimEntries *) override
    {
        ChildPrimTypeMap result;
        result[_GetProceduralPrimPath().AppendChild(_cubeName)] =
            HdPrimTypeTokens->mesh;
        return result;
    }

    HdSceneIndexPrim GetChildPrim(
        const HdSceneIndexBaseRefPtr &,
        const SdfPath &childPrimPath) override
    {
        HdSceneIndexPrim prim;
        if (childPrimPath.GetNameToken() != _cubeName) {
            return prim;
        }
        prim.primType = HdPrimTypeTokens->mesh;

        static const VtVec3fArray points = {
            {-1, -1, -1}, {1, -1, -1}, {1, 1, -1}, {-1, 1, -1},
            {-1, -1, 1},  {1, -1, 1},  {1, 1, 1},  {-1, 1, 1},
        };
        static const VtIntArray counts = {4, 4, 4, 4, 4, 4};
        static const VtIntArray indices = {
            0, 3, 2, 1,  4, 5, 6, 7,  0, 1, 5, 4,
            2, 3, 7, 6,  1, 2, 6, 5,  0, 4, 7, 3,
        };

        GfMatrix4d xf(1.0);
        xf.SetTranslate(GfVec3d(0.0, 0.0, 2.0));

        prim.dataSource = HdRetainedContainerDataSource::New(
            HdMeshSchema::GetSchemaToken(),
            HdMeshSchema::Builder()
                .SetTopology(
                    HdMeshTopologySchema::Builder()
                        .SetFaceVertexCounts(
                            HdRetainedTypedSampledDataSource<VtIntArray>::New(
                                counts))
                        .SetFaceVertexIndices(
                            HdRetainedTypedSampledDataSource<VtIntArray>::New(
                                indices))
                        .SetOrientation(
                            HdRetainedTypedSampledDataSource<TfToken>::New(
                                HdTokens->rightHanded))
                        .Build())
                .SetDoubleSided(
                    HdRetainedTypedSampledDataSource<bool>::New(true))
                .Build(),
            HdPrimvarsSchema::GetSchemaToken(),
            HdRetainedContainerDataSource::New(
                HdTokens->points,
                HdPrimvarSchema::Builder()
                    .SetPrimvarValue(
                        HdRetainedTypedSampledDataSource<VtVec3fArray>::New(
                            points))
                    .SetInterpolation(
                        HdPrimvarSchema::BuildInterpolationDataSource(
                            HdPrimvarSchemaTokens->vertex))
                    .SetRole(HdPrimvarSchema::BuildRoleDataSource(
                        HdPrimvarSchemaTokens->point))
                    .Build()),
            HdXformSchema::GetSchemaToken(),
            HdXformSchema::Builder()
                .SetMatrix(
                    HdRetainedTypedSampledDataSource<GfMatrix4d>::New(xf))
                .SetResetXformStack(
                    HdRetainedTypedSampledDataSource<bool>::New(false))
                .Build(),
            HdVisibilitySchema::GetSchemaToken(),
            HdVisibilitySchema::Builder()
                .SetVisibility(
                    HdRetainedTypedSampledDataSource<bool>::New(true))
                .Build());
        return prim;
    }
};

class CubeProceduralPlugin final : public HdGpGenerativeProceduralPlugin
{
public:
    HdGpGenerativeProcedural *Construct(
        const SdfPath &proceduralPrimPath) override
    {
        return new CubeProcedural(proceduralPrimPath);
    }
};

} // namespace

TF_REGISTRY_FUNCTION(TfType)
{
    HdGpGenerativeProceduralPluginRegistry::Define<
        CubeProceduralPlugin, HdGpGenerativeProceduralPlugin>();
}
