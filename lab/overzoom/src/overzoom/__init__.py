# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Super-resolution past the deepest level of an imagery tile pyramid.

Three stages, each importable without the next one's dependencies:

- ``tiles``, ``archives``, ``pairs``, ``degrade``, ``dataset``: build a dataset of validated pairs (CPU, anywhere);
- ``methods``, ``inference``: run the methods and the GPU-side metrics (GPU machine, ``[infer]`` extra);
- ``metrics``, ``artifacts``: shared by both, readable by the exploration notebook.
"""
