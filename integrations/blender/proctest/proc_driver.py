# Rendu témoin puis rendu avec prim procédural injecté par USDHook.
import bpy

bpy.ops.preferences.addon_enable(module='hydra_storm')
sc = bpy.context.scene
sc.render.engine = 'HYDRA_STORM'
sc.hydra.export_method = 'USD'
sc.render.resolution_x, sc.render.resolution_y = 640, 480
sc.render.image_settings.file_format = 'PNG'

cam = bpy.data.objects.new('cam', bpy.data.cameras.new('c'))
sc.collection.objects.link(cam)
sc.camera = cam
cam.location = (10, -10, 7)
cam.rotation_euler = (1.05, 0.0, 0.785)
sun = bpy.data.objects.new('sun', bpy.data.lights.new('s', 'SUN'))
sun.data.energy = 3.0
sc.collection.objects.link(sun)
sun.rotation_euler = (0.7, 0.2, 0.3)
bpy.ops.mesh.primitive_plane_add(size=30)

sc.render.filepath = '/tmp/out_base.png'
bpy.ops.render.render(write_still=True)
print('BASE-RENDERED', flush=True)


MODE = {'v': 'ref'}


class ProcRefHook(bpy.types.USDHook):
    bl_idname = 'proc_ref_hook'
    bl_label = 'proc ref'

    @staticmethod
    def on_export(ctx):
        print('HOOK-FIRED mode=' + MODE['v'], flush=True)
        stage = ctx.get_stage()
        if MODE['v'] == 'ref':
            prim = stage.DefinePrim('/proc_ref')
            prim.GetReferences().AddReference('/tmp/proc.usda')
        else:
            # Définition directe, sans référence : élimine la résolution de
            # chemin comme variable.
            Sdf = __import__('pxr').Sdf
            prim = stage.DefinePrim('/proc_direct', 'GenerativeProcedural')
            ok = prim.ApplyAPI('HydraGenerativeProceduralAPI')
            print(f'APPLY-API ok={ok}', flush=True)
            attr = prim.CreateAttribute('primvars:hdGp:proceduralType',
                                        Sdf.ValueTypeNames.Token)
            attr.Set('TestCube')
            # L'adaptateur usdProcImaging type le prim hydra d'après
            # proceduralSystem ; sans lui (fallback d'API schema non
            # délivré), il sort inertGenerativeProcedural et le resolver
            # l'ignore. Autoré explicitement : déterministe.
            prim.CreateAttribute('proceduralSystem',
                                 Sdf.ValueTypeNames.Token).Set(
                                     'hydraGenerativeProcedural')
        stage.Export('/tmp/composed-' + MODE['v'] + '.usda')
        return True


bpy.utils.register_class(ProcRefHook)
sc.render.filepath = '/tmp/out_proc.png'
bpy.ops.render.render(write_still=True)
print('PROC-RENDERED', flush=True)

MODE['v'] = 'direct'
sc.render.filepath = '/tmp/out_direct.png'
bpy.ops.render.render(write_still=True)
print('DIRECT-RENDERED', flush=True)

import numpy as np
def load(p):
    img = bpy.data.images.load(p)
    a = np.array(img.pixels[:])
    return a
a, b = load('/tmp/out_base.png'), load('/tmp/out_proc.png')
c = load('/tmp/out_direct.png')
dd = float(np.abs(a - c).mean()) if a.shape == c.shape else -1.0
print(f'PIXEL-DIFF-DIRECT mean={dd:.6f}', flush=True)
diff = float(np.abs(a - b).mean()) if a.shape == b.shape else -1.0
changed = float((np.abs(a - b) > 0.05).mean()) if a.shape == b.shape else -1.0
print(f'PIXEL-DIFF mean={diff:.6f} changed={changed:.4%}', flush=True)
