# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Le rendu génératif de bout en bout : une caméra orbite un cube que le
# procédural hdGp génère au rendu, à travers le chemin Hydra de Blender,
# encodé en mp4 par l'encodeur intégré. Si ce fichier existe, S2 existe.
import bpy
import math

# Scène VIDE : --factory-startup charge la scène de démo (Cube/Camera/
# Light) et son cube gris se superposait au cube procédural.
bpy.ops.wm.read_factory_settings(use_empty=True)
sc = bpy.context.scene
bpy.ops.preferences.addon_enable(module='hydra_storm')
sc.render.engine = 'HYDRA_STORM'
sc.hydra.export_method = 'USD'
sc.render.resolution_x, sc.render.resolution_y = 960, 720
sc.render.fps = 24
sc.frame_start, sc.frame_end = 1, 96

# Sol + éclairage à trois sources : soleil chaud en clé, soleil froid en
# contre, et un ciel bleuté (exporté en DomeLight) pour l'ambiance.
bpy.ops.mesh.primitive_plane_add(size=30)
ground = bpy.data.materials.new('Ground')
ground.use_nodes = True
gb = ground.node_tree.nodes['Principled BSDF']
gb.inputs['Base Color'].default_value = (0.16, 0.20, 0.18, 1.0)
gb.inputs['Roughness'].default_value = 0.9
bpy.context.active_object.data.materials.append(ground)
sun = bpy.data.objects.new('sun', bpy.data.lights.new('s', 'SUN'))
sun.data.energy = 5.0
sun.data.color = (1.0, 0.95, 0.85)
sc.collection.objects.link(sun)
sun.rotation_euler = (0.7, 0.2, 0.3)
fill = bpy.data.objects.new('fill', bpy.data.lights.new('f', 'SUN'))
fill.data.energy = 1.2
fill.data.color = (0.65, 0.75, 1.0)
sc.collection.objects.link(fill)
fill.rotation_euler = (1.1, -0.4, 2.7)
world = bpy.data.worlds.new('Sky')
world.use_nodes = True
bg = world.node_tree.nodes['Background']
bg.inputs[0].default_value = (0.45, 0.58, 0.78, 1.0)
bg.inputs[1].default_value = 0.45
sc.world = world

# Caméra en orbite : parent tournant animé.
rig = bpy.data.objects.new('rig', None)
sc.collection.objects.link(rig)
cam = bpy.data.objects.new('cam', bpy.data.cameras.new('c'))
sc.collection.objects.link(cam)
cam.parent = rig
cam.location = (11, 0, 6)
cam.rotation_euler = (1.05, 0.0, math.pi / 2)
sc.camera = cam
# Interpolation LINEAR à la source : Blender 5.x a remplacé
# action.fcurves par les actions en couches (layers/strips/channelbags).
bpy.context.preferences.edit.keyframe_new_interpolation_type = 'LINEAR'
rig.rotation_euler = (0.0, 0.0, 0.0)
rig.keyframe_insert('rotation_euler', frame=1)
rig.rotation_euler = (0.0, 0.0, 2.0 * math.pi)
rig.keyframe_insert('rotation_euler', frame=96)


class ProcHook(bpy.types.USDHook):
    bl_idname = 'proc_video_hook'
    bl_label = 'proc video'

    @staticmethod
    def on_export(ctx):
        from pxr import Sdf
        stage = ctx.get_stage()
        prim = stage.DefinePrim('/proc_direct', 'GenerativeProcedural')
        prim.ApplyAPI('HydraGenerativeProceduralAPI')
        prim.CreateAttribute('primvars:hdGp:proceduralType',
                             Sdf.ValueTypeNames.Token).Set('TestCube')
        # Type hydra déterministe pour l'adaptateur usdProcImaging (le
        # fallback d'API schema n'est pas garanti dans ce process).
        prim.CreateAttribute('proceduralSystem',
                             Sdf.ValueTypeNames.Token).Set(
                                 'hydraGenerativeProcedural')
        print('VIDEO-HOOK-FIRED', flush=True)
        return True


bpy.utils.register_class(ProcHook)

ims = sc.render.image_settings
if hasattr(ims, 'media_type'):
    ims.media_type = 'VIDEO'
ims.file_format = 'FFMPEG'
sc.render.ffmpeg.format = 'MPEG4'
sc.render.ffmpeg.codec = 'H264'
sc.render.filepath = '/out/proc-cube.mp4'
bpy.ops.render.render(animation=True)
print('VIDEO-RENDER-FINISHED', flush=True)
