// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// Runs the bench's wasm32-wasip1 build under Node's WASI, with one directory
// of PNG tiles preopened at /tiles.
//
//   node examples/decode-bench/wasi-run.mjs <bench.wasm> <directory of PNGs>
import { readFile } from "node:fs/promises";
import { WASI } from "node:wasi";
import { argv } from "node:process";

const [wasmPath, dir] = argv.slice(2);
const wasi = new WASI({
  version: "preview1",
  args: ["tuile-decode-bench", "/tiles"],
  preopens: { "/tiles": dir },
});
const module = await WebAssembly.compile(await readFile(wasmPath));
const instance = await WebAssembly.instantiate(module, wasi.getImportObject());
wasi.start(instance);
