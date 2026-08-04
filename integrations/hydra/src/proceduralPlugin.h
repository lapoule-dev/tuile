// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#ifndef TUILE_HYDRA_PROCEDURAL_PLUGIN_H
#define TUILE_HYDRA_PROCEDURAL_PLUGIN_H

#include <pxr/pxr.h>
#include <pxr/imaging/hdGp/generativeProceduralPlugin.h>

PXR_NAMESPACE_OPEN_SCOPE

/// Makes [TuileGlobeProcedural] discoverable by `primvars:hdGp:proceduralType`.
class TuileGlobeProceduralPlugin final : public HdGpGenerativeProceduralPlugin
{
public:
    HdGpGenerativeProcedural *Construct(
        const SdfPath &proceduralPrimPath) override;
};

PXR_NAMESPACE_CLOSE_SCOPE

#endif
