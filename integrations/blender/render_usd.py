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
import sys
import time

import bpy


def parse_args():
    argv = sys.argv[sys.argv.index("--") + 1:] if "--" in sys.argv else []
    p = argparse.ArgumentParser()
    p.add_argument("--stage", required=True, help="OpenUSD stage to render")
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


def main():
    args = parse_args()
    first, last = (int(x) for x in args.frames.split(":"))

    bpy.ops.wm.read_factory_settings(use_empty=True)
    bpy.ops.wm.usd_import(filepath=args.stage, import_all_materials=True)

    scene = bpy.context.scene
    scene.render.resolution_x = args.width
    scene.render.resolution_y = args.height or (args.width * 3 // 4)
    scene.render.fps = args.fps
    scene.render.use_persistent_data = True

    cameras = [o for o in bpy.data.objects if o.type == "CAMERA"]
    scene.camera = (bpy.data.objects[args.camera] if args.camera else cameras[0])

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

    if args.tier == "cycles":
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
