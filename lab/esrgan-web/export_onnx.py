# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""The same network as ONNX, for the ONNX Runtime Web yardstick (`ort.html`): ``data/realesr-general-x4v3.onnx``.

    python -m venv .venv-onnx && .venv-onnx/bin/pip install onnx==1.19.0 numpy==2.2.6
    .venv-onnx/bin/python export_onnx.py

Built from ``data/weights.bin`` (the flat f32 tensors `prepare.py` writes), with the graph written out by hand:
34 Conv, 33 PRelu, DepthToSpace (CRD, which is PyTorch's pixel shuffle), a nearest Resize and an Add. A yardstick
only: nothing in the Burn page depends on it.
"""

import argparse
from pathlib import Path

import numpy as np
import onnx
from onnx import TensorProto, helper, numpy_helper

FEAT, NUM_CONV, SCALE = 64, 32, 4

p = argparse.ArgumentParser()
p.add_argument("--f16", action="store_true", help="f16 weights and activations (input and output stay f32)")
args = p.parse_args()
data = Path(__file__).parent / "data"
flat = np.fromfile(data / "weights.bin", dtype="<f4")
dt = np.float16 if args.f16 else np.float32
at, inits, nodes = 0, [], []


def take(shape):
    global at
    n = int(np.prod(shape))
    t = flat[at:at + n].reshape(shape)
    at += n
    return t


x = "x16" if args.f16 else "x"
if args.f16:
    nodes.append(helper.make_node("Cast", ["x"], ["x16"], to=TensorProto.FLOAT16))
y = x
for i in range(NUM_CONV + 2):
    cin, cout = (3, FEAT) if i == 0 else (FEAT, 3 * SCALE * SCALE) if i == NUM_CONV + 1 else (FEAT, FEAT)
    w, b = take((cout, cin, 3, 3)), take((cout,))
    inits += [numpy_helper.from_array(w.astype(dt), f"w{i}"), numpy_helper.from_array(b.astype(dt), f"b{i}")]
    nodes.append(helper.make_node("Conv", [y, f"w{i}", f"b{i}"], [f"c{i}"], kernel_shape=[3, 3], pads=[1, 1, 1, 1]))
    y = f"c{i}"
    if i <= NUM_CONV:
        inits.append(numpy_helper.from_array(take((cout,)).reshape(cout, 1, 1).astype(dt), f"a{i}"))
        nodes.append(helper.make_node("PRelu", [y, f"a{i}"], [f"p{i}"]))
        y = f"p{i}"
assert at == flat.size, (at, flat.size)
inits.append(numpy_helper.from_array(np.array([1, 1, SCALE, SCALE], dtype=np.float32), "scales"))
nodes += [
    helper.make_node("DepthToSpace", [y], ["ps"], blocksize=SCALE, mode="CRD"),
    helper.make_node("Resize", [x, "", "scales"], ["up"], mode="nearest", coordinate_transformation_mode="asymmetric",
                     nearest_mode="floor"),
    helper.make_node("Add", ["ps", "up"], ["y16" if args.f16 else "y"]),
]
if args.f16:
    nodes.append(helper.make_node("Cast", ["y16"], ["y"], to=TensorProto.FLOAT))
graph = helper.make_graph(nodes, "srvgg_compact_x4", [helper.make_tensor_value_info("x", TensorProto.FLOAT, [1, 3, 256, 256])],
                          [helper.make_tensor_value_info("y", TensorProto.FLOAT, [1, 3, 1024, 1024])], inits)
model = helper.make_model(graph, opset_imports=[helper.make_opsetid("", 17)])
onnx.checker.check_model(model)
out = data / f"realesr-general-x4v3{'.f16' if args.f16 else ''}.onnx"
onnx.save(model, out)
print(out, out.stat().st_size, "bytes")
