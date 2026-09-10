# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Expose le délégué Hydra de Cycles comme moteur de rendu Blender.

# Pourquoi cet addon existe chez nous

Parce que Blender 5.1.2 embarque le **délégué** et pas l'addon qui l'expose.
Mesuré dans l'image : `addons_core/cycles/hydra/hdCycles.so` et son
`plugInfo.json` sont là, `addons_core/hydra_cycles` ne l'est pas — l'addon
amont (`intern/cycles/hydra/addon/`) est postérieur à cette version. Sans lui,
`scene.render.engine` ne connaît que `HYDRA_STORM`.

# Pourquoi ça compte

Storm est un rasteriseur OpenGL : il n'appelle jamais CUDA. C'est la raison
pour laquelle `CUDA_VISIBLE_DEVICES` ne pilote rien sur le chemin manifeste et
que quatre processus, un par GPU selon le script, ont été mesurés tous les
quatre sur le GPU 2. Cycles en délégué est en aval du resolver hdGp — le
procédural cuit exactement pareil — et rend par OptiX.

# Ce qui est copié d'amont, et pourquoi à la lettre

`bl_idname`, `bl_delegate_id` et l'emplacement du plugin viennent de
`intern/cycles/hydra/addon/__init__.py` et de son CMakeLists. Ce sont des noms
que Blender et USD comparent par égalité de chaîne : les inventer, c'est un
pod qui démarre, échoue et se facture.
"""

import os

import bpy

bl_info = {
    "name": "Hydra Cycles render engine",
    "author": "lapoule.dev",
    "version": (0, 1, 0),
    "blender": (5, 1, 0),
    "description": "Cycles via son délégué Hydra — OptiX au lieu de Storm/GL",
    "category": "Render",
}


class CyclesHydraRenderEngine(bpy.types.HydraRenderEngine):
    bl_idname = "HYDRA_CYCLES"
    bl_label = "Hydra Cycles"
    bl_info = "Cycles path tracing renderer using the Hydra render delegate"

    bl_use_preview = False
    bl_use_gpu_context = False
    bl_use_materialx = False

    bl_delegate_id = "HdCyclesPlugin"

    @classmethod
    def register(cls):
        # Le délégué est installé DANS l'addon cycles, pas à côté :
        # `addons_core/cycles/hydra/`. Trouvé plutôt que codé en dur, parce
        # qu'une version de Blender qui le déplace doit échouer en le disant,
        # pas en rendant sans texture.
        here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        candidates = [
            os.path.join(here, "cycles", "hydra"),
            os.path.join(here, "..", "addons_core", "cycles", "hydra"),
        ]
        for plugin_dir in candidates:
            plugin_dir = os.path.normpath(plugin_dir)
            if os.path.isfile(os.path.join(plugin_dir, "plugInfo.json")):
                import pxr.Plug
                pxr.Plug.Registry().RegisterPlugins([plugin_dir])
                print(f"hydra_cycles: {plugin_dir}", flush=True)
                return
        # Bruyant. Un délégué introuvable qui laisserait l'addon s'enregistrer
        # quand même donnerait un moteur qui existe et ne rend rien.
        print("hydra_cycles: aucun plugInfo.json de hdCycles trouvé dans "
              f"{candidates}", flush=True)

    def get_render_settings(self, engine_type):
        cscene = getattr(bpy.context.scene, "cycles", None)
        samples = 1024 if cscene is None else cscene.samples
        settings = {"cycles:samples": samples}
        if engine_type != "VIEWPORT":
            settings |= {"aovToken:Combined": "color", "aovToken:Depth": "depth"}
        return settings

    def update_render_passes(self, scene, render_layer):
        if render_layer.use_pass_combined:
            self.register_pass(scene, render_layer, "Combined", 4, "RGBA", "COLOR")
        if render_layer.use_pass_z:
            self.register_pass(scene, render_layer, "Depth", 1, "Z", "VALUE")


def register():
    bpy.utils.register_class(CyclesHydraRenderEngine)


def unregister():
    bpy.utils.unregister_class(CyclesHydraRenderEngine)
