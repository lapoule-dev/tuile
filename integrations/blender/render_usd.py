# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# The farm render driver: one OpenUSD stage, one frame range, one tier.
#
#   blender -b -P render_usd.py -- --stage gate.usda --tier cycles \
#       --frames 1:48 --width 960 --out /out/seq --video /out/segment.mp4
#
# Everything below was measured before it was written (M2 bench, 2026-09):
# hybrid CPU+GPU slows Metal down (GPU only, always); the default 1024
# samples are an animation killer (adaptive + a cap + one final denoise is
# the batch recipe); EEVEE's fast-GI blackens ground lit by the world; depth
# of field is the dominant sample eater and aerial product shots do not want
# it. The process is persistent across the whole range — scene, BVH and
# kernels are paid once (persistent_data), only the camera moves.

import argparse
import os
import pathlib
import sys
import time

import bpy


def parse_args():
    argv = sys.argv[sys.argv.index("--") + 1:] if "--" in sys.argv else []
    p = argparse.ArgumentParser()
    p.add_argument("--stage", required=True, help="OpenUSD stage to render")
    p.add_argument("--engine", choices=["native", "hydra"], default="native",
                   help="native = Cycles/EEVEE on imported geometry; hydra = "
                        "HYDRA_STORM over the exported stage, generative "
                        "procedurals cook (the manifest path)")
    p.add_argument("--delegate", choices=["storm", "cycles"], default="storm",
                   help="quel délégué Hydra cuit la scène (--engine hydra). "
                        "storm est un rasteriseur OpenGL : il n'appelle jamais "
                        "CUDA, donc CUDA_VISIBLE_DEVICES ne pilote rien et "
                        "quatre processus atterrissent sur un seul GPU. cycles "
                        "en délégué passe par OptiX, en aval du resolver hdGp "
                        "— le procédural cuit exactement pareil.")
    p.add_argument("--tier", choices=["cycles", "eevee"], default="cycles")
    p.add_argument("--frames", default="1:48", help="A:B inclusive")
    p.add_argument("--width", type=int, default=1920)
    p.add_argument("--height", type=int, default=0, help="0 = 3:4 of width")
    p.add_argument("--fps", type=int, default=24)
    p.add_argument("--samples", type=int, default=0, help="0 = tier default")
    p.add_argument("--adaptive-threshold", type=float, default=0.05,
                   help="cycles adaptive sampling noise threshold (lower = higher quality)")
    p.add_argument("--camera", default="", help="object name; default: first camera")
    p.add_argument("--out", required=True, help="frame path prefix")
    p.add_argument("--video", default="", help="encode this mp4 (H.264)")
    p.add_argument("--keep-frames", action="store_true",
                   help="with --video: also write the PNG sequence (default: direct-to-video, no frames touch disk)")
    p.add_argument("--batch-frames", type=int, default=60,
                   help="progress log granularity (frames per batch; 0 = quiet)")
    p.add_argument("--view-transform", default="Standard",
                   help="transformation de vue OCIO — le look, épinglé plutôt "
                        "qu'hérité. Blender change son défaut entre versions "
                        "(Filmic avant 4.0, AgX depuis) et un rendu dont le "
                        "look dépend de la version installée n'est pas "
                        "reproductible. 'Standard' pour un encodage sRGB "
                        "direct, sans courbe.\n"
                        "Standard par défaut, et c'est un choix : la texture "
                        "est DÉJÀ une photographie. AgX la re-étalonne — "
                        "mesuré sur la même frame, chroma moyenne 5,4 contre "
                        "10,9 et écart-type 11,5 contre 22,3. Pour de "
                        "l'imagerie aérienne, fidèle vaut mieux que "
                        "cinématographique.")
    p.add_argument("--exposure", type=float, default=-1.5,
                   help="exposition en diaphragmes, appliquée au film. Le "
                        "soleil et le ciel de la scène sont une composition, "
                        "pas une mesure ; ce réglage-ci est la mesure.\n"
                        "-1,5 par défaut, mesuré le 17 septembre 2026 sur la "
                        "frame 1 de l'orbite pyrénéenne, en Standard : à 0 "
                        "diaph 22,33 % des pixels sont écrêtés à 255, à -1,5 "
                        "il en reste 0,06 %, à -2,5 l'image est propre mais "
                        "terne (écart-type 16,7 contre 22,3). Le soleil de la "
                        "scène est à 4 W/m², ce qui est une composition, pas "
                        "une erreur — on la corrige au film plutôt que de "
                        "changer l'équilibre des sources.")
    p.add_argument("--no-dof", action="store_true", help="disable depth of field")
    p.add_argument("--demo-fixups", action="store_true",
                   help="gate-stage repairs: world sky, light energy, sphere-prim rebinding")
    return p.parse_args(argv)


def gpu_only(prefs, kinds):
    """Enable exactly one backend's GPUs, never the CPU alongside.

    compute_device_type is a *dynamic* enum: its enum_items introspect as
    empty, so availability is probed by assignment, not by reading.
    """
    for kind in kinds:
        try:
            prefs.compute_device_type = kind
        except TypeError:
            continue
        prefs.get_devices()
        gpus = [d for d in prefs.devices if d.type == kind]
        if gpus:
            for d in prefs.devices:
                d.use = d.type == kind
            return kind
    return None


# Which Blender render engine drives which Hydra delegate.
#
# Storm is what the manifest path has always used and it is an OpenGL
# rasteriser: it never calls CUDA. That is the whole reason
# CUDA_VISIBLE_DEVICES steers nothing on this path and four processes told to
# take four GPUs were measured all running on GPU 2. Cycles as a delegate sits
# downstream of the hdGp resolver — the procedural cooks identically — and
# renders through OptiX, so the device arithmetic means something again.
# Read off Blender's own source rather than guessed, because a wrong
# identifier here is a pod that boots, fails and is billed:
# `intern/cycles/hydra/addon/__init__.py` declares
# `bl_idname = 'HYDRA_CYCLES'` and `bl_delegate_id = 'HdCyclesPlugin'`, and
# `intern/cycles/hydra/CMakeLists.txt` installs that addon as `hydra_cycles`
# beside the `cycles` one, with the delegate under `cycles/hydra/`.
HYDRA_DELEGATES = {
    "storm": ("hydra_storm", "HYDRA_STORM"),
    "cycles": ("hydra_cycles", "HYDRA_CYCLES"),
}


def setup_hydra_manifest(scene, stage_path, first, last, delegate="storm"):
    """The manifest path: a Hydra delegate over Blender's own scene.

    The manifest's camera is rebuilt as a keyframed Blender camera straight
    from pxr rather than through `usd_import` — the importer's animation
    support is a bet, and a silently static camera renders 1440 identical
    frames.

    The Globe prim cannot survive an import at all: no Blender object maps to
    a `GenerativeProcedural`. It is composed into the render instead, by the
    scene index plugin in `integrations/hydra/src/manifest.cpp`, which merges
    the manifest into the chain one insertion phase ahead of hdGp's resolver.
    All this side has to do is name the file.

    That replaces a `USDHook.on_export`, which only fired on Blender's USD
    export path — the one Blender's own source calls "Slow USD export for
    reference", and which tears the scene down and rebuilds it once per frame
    (`USDSceneIndex::populate`, read at v5.2.2). That teardown is why a frame
    cost 9.7 s with a pack that had precomputed everything: our procedural is
    an instance member, so a procedural rebuilt every frame starts from an
    empty state and rebuilds all 444 tiles — measured, 8 frames, `cook #1 ...
    kept=0 built=444` eight times over.

    The fast path keeps its scene index across frames and populates the delta.
    It exports nothing, so nothing here may depend on an export.
    """
    import mathutils
    from pxr import Usd, UsdGeom

    module, engine = HYDRA_DELEGATES[delegate]
    if delegate == "cycles":
        # The delegate is installed inside the `cycles` addon, and enabling
        # `cycles` first is what puts its directory where ours looks.
        bpy.ops.preferences.addon_enable(module="cycles")
    bpy.ops.preferences.addon_enable(module=module)
    try:
        # By ASSIGNMENT, not by reading `enum_items`. That property answered
        # `['BLENDER_EEVEE']` in the image while `CYCLES` and `HYDRA_CYCLES`
        # were both perfectly settable — a check on it would have aborted
        # every job for a delegate that was there. Blender's own TypeError
        # names the engines that really exist, which is the diagnosis anyway.
        scene.render.engine = engine
    except TypeError as e:
        # Loud. A silent fall back to Storm would render — on one GPU, at a
        # different look — and the only symptoms would be the bill and a
        # picture nobody could account for.
        print(f"FATAL: the {delegate} Hydra delegate is not registered: {e}",
              file=sys.stderr, flush=True)
        sys.exit(1)
    # The fast path, and the only one on which the globe is incremental.
    # Blender's own enum describes the other as "for accurate comparison with
    # USD file export"; it is a reference path, not a production one.
    scene.hydra.export_method = "HYDRA"

    manifest = str(pathlib.Path(stage_path).resolve())
    # Lue par `tuile_insert_manifest` quand le moteur construit son render
    # index — le fork de Blender l'appelle par `TUILE_HYDRA_LIB`, et c'est tout
    # ce que ce côté-ci a à faire pour que le globe entre dans la scène.
    #
    # Posée avant le premier rendu, et elle reste posée : la lecture se fait à
    # chaque construction de chaîne plutôt qu'une fois pour toutes.
    os.environ["TUILE_MANIFEST"] = manifest

    source = Usd.Stage.Open(manifest)
    cam_prim = next(
        (p for p in source.Traverse() if p.IsA(UsdGeom.Camera)), None)
    if cam_prim is None:
        print("FATAL: the manifest has no camera", file=sys.stderr, flush=True)
        sys.exit(1)

    usd_cam = UsdGeom.Camera(cam_prim)
    cam_data = bpy.data.cameras.new("shot")
    cam = bpy.data.objects.new("shot", cam_data)
    scene.collection.objects.link(cam)
    scene.camera = cam

    aperture = usd_cam.GetVerticalApertureAttr().Get(float(first)) or 24.0
    cam_data.sensor_fit = "VERTICAL"
    cam_data.sensor_height = aperture
    cam_data.lens = usd_cam.GetFocalLengthAttr().Get(float(first)) or 35.0
    clip = usd_cam.GetClippingRangeAttr().Get(float(first))
    if clip:
        cam_data.clip_start = max(clip[0], 0.01)
        cam_data.clip_end = clip[1]

    # One keyframe per frame, straight from the time samples: the tape wrote
    # them exact, the render reads them exact.
    xf_cache = UsdGeom.XformCache()
    for f in range(first, last + 1):
        xf_cache.SetTime(float(f))
        m = xf_cache.GetLocalToWorldTransform(cam_prim)
        # USD is row-major with row vectors; Blender's Matrix applies to
        # column vectors — the transpose is the whole conversion.
        cam.matrix_world = mathutils.Matrix(
            [[m[0][0], m[1][0], m[2][0], m[3][0]],
             [m[0][1], m[1][1], m[2][1], m[3][1]],
             [m[0][2], m[1][2], m[2][2], m[3][2]],
             [0.0, 0.0, 0.0, 1.0]])
        cam.keyframe_insert("location", frame=f)
        cam.keyframe_insert("rotation_euler", frame=f)

    # The look is scene-side composition, as always: a sky dome and a sun,
    # built as Blender data. The manifest carries geometry and config only.
    world = bpy.data.worlds.new("Sky")
    world.use_nodes = True
    bg = world.node_tree.nodes["Background"]
    bg.inputs[0].default_value = (0.45, 0.58, 0.78, 1.0)
    bg.inputs[1].default_value = 0.6
    scene.world = world
    sun = bpy.data.objects.new("sun", bpy.data.lights.new("s", "SUN"))
    sun.data.energy = 4.0
    scene.collection.objects.link(sun)
    sun.rotation_euler = (0.7, 0.2, 0.3)


def main():
    args = parse_args()
    first, last = (int(x) for x in args.frames.split(":"))

    bpy.ops.wm.read_factory_settings(use_empty=True)
    if args.engine == "native":
        bpy.ops.wm.usd_import(filepath=args.stage, import_all_materials=True)

    scene = bpy.context.scene
    scene.render.resolution_x = args.width
    scene.render.resolution_y = args.height or (args.width * 3 // 4)
    scene.render.fps = args.fps
    scene.render.use_persistent_data = True
    # Le look, nommé.
    #
    # Lu plutôt que supposé : sans cette ligne, 5.2 applique AgX, dont la
    # signature — noirs relevés, blancs arrêtés sous 255, contraste écrasé —
    # est difficile à distinguer à l'œil d'une texture sRGB lue comme
    # linéaire. Mesuré sur un rendu du 17 septembre 2026 : min 88, moyenne
    # 187, max 229, zéro pixel écrêté.
    try:
        scene.view_settings.view_transform = args.view_transform
    except TypeError:
        print(f"view transform inconnue: {args.view_transform!r} — celles de "
              f"ce Blender: "
              f"{[i.identifier for i in scene.view_settings.bl_rna.properties['view_transform'].enum_items]}",
              flush=True)
        raise
    scene.view_settings.exposure = args.exposure
    print(f"view transform: {scene.view_settings.view_transform}, "
          f"exposition {args.exposure:+g} diaph", flush=True)

    if args.engine == "hydra":
        setup_hydra_manifest(scene, args.stage, first, last, args.delegate)
    else:
        cameras = [o for o in bpy.data.objects if o.type == "CAMERA"]
        scene.camera = (
            bpy.data.objects[args.camera] if args.camera else cameras[0])

        if args.no_dof:
            for cam in cameras:
                cam.data.dof.use_dof = False

    if args.demo_fixups:
        # The importer maps neither DomeLight to a world nor USD light
        # intensities to watts, and drops material bindings on tessellated
        # Sphere prims (mesh prims keep theirs).
        world = bpy.data.worlds.new("Sky")
        world.use_nodes = True
        bg = world.node_tree.nodes["Background"]
        bg.inputs[0].default_value = (0.55, 0.65, 0.85, 1)
        bg.inputs[1].default_value = 0.6
        scene.world = world
        for obj in bpy.data.objects:
            if obj.type == "LIGHT":
                obj.data.energy = 500.0
        for name, mat in (("Chrome", "ChromeMat"), ("Clay", "ClayMat"),
                          ("Lacquer", "LacquerMat")):
            if name in bpy.data.objects and mat in bpy.data.materials:
                bpy.data.objects[name].data.materials.append(bpy.data.materials[mat])

    if args.engine == "hydra":
        # Engine set by setup_hydra_manifest. Storm has neither samplers nor
        # denoisers to configure — its cost lives in the procedural cook — but
        # the Cycles delegate is a path tracer and needs a sample count. It
        # reads it through the add-on's `get_render_settings`, which reads
        # `scene.cycles.samples`, so the flag has to land there.
        if args.delegate == "cycles":
            scene.cycles.samples = args.samples or 128
            scene.cycles.use_adaptive_sampling = True
            scene.cycles.adaptive_threshold = args.adaptive_threshold
            # Said out loud, because the delegate takes its device from
            # CYCLES_DEVICE and falls back to CPU in silence when nothing sets
            # it (cycles/src/hydra/render_delegate.cpp). A job path-tracing on
            # the host CPU looks exactly like a slow job.
            print(f"hydra cycles: CYCLES_DEVICE="
                  f"{os.environ.get('CYCLES_DEVICE', '<unset — CPU!>')}, "
                  f"CUDA_VISIBLE_DEVICES="
                  f"{os.environ.get('CUDA_VISIBLE_DEVICES', '<all>')}, "
                  f"{scene.cycles.samples} samples", flush=True)
        print(f"hydra ({scene.render.engine}), manifest camera keyframed",
              flush=True)
    elif args.tier == "cycles":
        scene.render.engine = "CYCLES"
        prefs = bpy.context.preferences.addons["cycles"].preferences
        kind = gpu_only(prefs, ("OPTIX", "CUDA", "METAL", "HIP"))
        scene.cycles.device = "GPU" if kind else "CPU"
        scene.cycles.samples = args.samples or 128
        scene.cycles.use_adaptive_sampling = True
        scene.cycles.adaptive_threshold = args.adaptive_threshold
        scene.cycles.use_denoising = True
        scene.cycles.denoiser = "OPTIX" if kind == "OPTIX" else "OPENIMAGEDENOISE"
        print(f"cycles on {kind or 'CPU'}, {scene.cycles.samples} samples,"
              f" denoiser {scene.cycles.denoiser}", flush=True)
    else:
        # 4.5 LTS names the engine BLENDER_EEVEE_NEXT; 5.x went back to
        # BLENDER_EEVEE.
        eevee_id = ("BLENDER_EEVEE_NEXT"
                    if "BLENDER_EEVEE_NEXT" in
                    scene.render.bl_rna.properties["engine"].enum_items
                    else "BLENDER_EEVEE")
        scene.render.engine = eevee_id
        ee = scene.eevee
        ee.taa_render_samples = args.samples or 32
        if hasattr(ee, "use_raytracing"):
            ee.use_raytracing = True
            ee.ray_tracing_options.resolution_scale = "2"
        if hasattr(ee, "use_fast_gi"):
            ee.use_fast_gi = False  # blackens world-lit ground
        print(f"eevee ({eevee_id}), {ee.taa_render_samples} samples", flush=True)

    expected = last - first + 1
    ffmpeg_capable = "FFMPEG" in scene.render.image_settings.bl_rna.properties[
        "file_format"].enum_items

    # Batch progress: one line per --batch-frames frames, never per frame.
    # render_write fires on every frame written, including inside a direct
    # animation render where our own loop never gets control back.
    batch = {"n": 0, "t": time.time()}

    def batch_log(*_):
        batch["n"] += 1
        size = args.batch_frames
        if size and batch["n"] % size == 0:
            now = time.time()
            print(f"lot {batch['n'] // size}/{(expected + size - 1) // size}:"
                  f" {batch['n']}/{expected} frames,"
                  f" {(now - batch['t']) / size:.2f} s/frame", flush=True)
            batch["t"] = now

    bpy.app.handlers.render_write.append(batch_log)

    # Et une ligne quand une frame COMMENCE.
    #
    # `render_write` ne parle qu'une fois la frame écrite, donc tout ce qui se
    # passe avant la première image est un silence indistinguable d'un blocage.
    # Mesuré le 15 septembre sur Cloud Run : vingt-sept minutes entre
    # `SOURCE pack` et rien du tout, sans aucun moyen de dire si Cycles
    # travaillait, si le procédural cuisait encore, ou si le process était
    # mort. Une ligne par frame entamée coûte un `print` et répond à la
    # question.
    started = {"t": time.time()}

    def frame_begin(scene_arg, *_):
        now = time.time()
        n = getattr(scene_arg, "frame_current", "?")
        print(f"frame {n}: début (+{now - started['t']:.1f}s)", flush=True)
        started["t"] = now

    bpy.app.handlers.render_pre.append(frame_begin)
    print(f"rendu: {expected} frames {first}:{last}, "
          f"moteur {scene.render.engine}", flush=True)

    if args.video and not args.keep_frames:
        # Direct-to-video: no frame ever touches the disk. 4.5 LTS carries
        # the built-in encoder (removed in 5.x, where this falls back to the
        # sequence path below).
        if not ffmpeg_capable:
            print("no built-in encoder (Blender 5.x): falling back to frames",
                  flush=True)
            args.keep_frames = True
        else:
            # Blender 5.x filters the format list by media_type; VIDEO must
            # be selected before FFMPEG exists as a choice (4.x has no such
            # property).
            if hasattr(scene.render.image_settings, "media_type"):
                scene.render.image_settings.media_type = "VIDEO"
            scene.render.image_settings.file_format = "FFMPEG"
            scene.render.ffmpeg.format = "MPEG4"
            scene.render.ffmpeg.codec = "H264"
            scene.render.ffmpeg.constant_rate_factor = "HIGH"
            scene.frame_start, scene.frame_end = first, last
            scene.render.filepath = args.video
            t0 = time.time()
            bpy.ops.render.render(animation=True, use_viewport=False)
            dt = time.time() - t0
            print(f"BENCH {args.tier}: {dt:.1f}s total,"
                  f" {dt / expected:.3f}s/frame"
                  f" ({expected} frames @ {scene.render.resolution_x}px,"
                  " direct video)", flush=True)
            print(f"video: {args.video}", flush=True)
            return

    scene.render.image_settings.file_format = "PNG"

    t0 = time.time()
    rendered = 0
    for f in range(first, last + 1):
        scene.frame_set(f)
        scene.render.filepath = f"{args.out}.{f}.png"
        bpy.ops.render.render(write_still=True)
        rendered += 1
    dt = time.time() - t0

    if rendered != expected:
        print(f"FATAL: {rendered} frames rendered, {expected} expected",
              file=sys.stderr, flush=True)
        sys.exit(1)
    print(f"BENCH {args.tier}: {dt:.1f}s total, {dt / rendered:.3f}s/frame"
          f" ({rendered} frames @ {scene.render.resolution_x}px)", flush=True)

    if args.video and ffmpeg_capable:
        # Encode from the rendered sequence via the sequencer (no re-render).
        if hasattr(scene.render.image_settings, "media_type"):
            scene.render.image_settings.media_type = "VIDEO"
        scene.render.image_settings.file_format = "FFMPEG"
        scene.render.ffmpeg.format = "MPEG4"
        scene.render.ffmpeg.codec = "H264"
        scene.render.ffmpeg.constant_rate_factor = "HIGH"
        scene.frame_start, scene.frame_end = first, last
        scene.sequence_editor_create()
        strip = scene.sequence_editor.sequences.new_image(
            name="seq", filepath=f"{args.out}.{first}.png", channel=1,
            frame_start=first)
        for f in range(first + 1, last + 1):
            strip.elements.append(f"{args.out.rsplit('/', 1)[-1]}.{f}.png")
        scene.render.filepath = args.video
        bpy.ops.render.render(animation=True, use_viewport=False)
        print(f"video: {args.video}", flush=True)


main()
