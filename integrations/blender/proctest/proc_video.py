# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Le rendu génératif de bout en bout : une caméra orbite un cube que le
# procédural hdGp génère au rendu, à travers le chemin Hydra de Blender,
# encodé en mp4 par l'encodeur intégré. Si ce fichier existe, S2 existe.
import bpy
import math

sc = bpy.context.scene
bpy.ops.preferences.addon_enable(module='hydra_storm')
sc.render.engine = 'HYDRA_STORM'
sc.hydra.export_method = 'USD'
sc.render.resolution_x, sc.render.resolution_y = 960, 720
sc.render.fps = 24
sc.frame_start, sc.frame_end = 1, 96

# Sol + soleil natifs (exportés normalement vers le stage).
bpy.ops.mesh.primitive_plane_add(size=30)
sun = bpy.data.objects.new('sun', bpy.data.lights.new('s', 'SUN'))
sun.data.energy = 3.0
sc.collection.objects.link(sun)
sun.rotation_euler = (0.7, 0.2, 0.3)

# Caméra en orbite : parent tournant animé.
rig = bpy.data.objects.new('rig', None)
sc.collection.objects.link(rig)
cam = bpy.data.objects.new('cam', bpy.data.cameras.new('c'))
sc.collection.objects.link(cam)
cam.parent = rig
cam.location = (11, 0, 6)
cam.rotation_euler = (1.05, 0.0, math.pi / 2)
sc.camera = cam
rig.rotation_euler = (0.0, 0.0, 0.0)
rig.keyframe_insert('rotation_euler', frame=1)
rig.rotation_euler = (0.0, 0.0, 2.0 * math.pi)
rig.keyframe_insert('rotation_euler', frame=96)
for fc in rig.animation_data.action.fcurves:
    for kp in fc.keyframe_points:
        kp.interpolation = 'LINEAR'


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
