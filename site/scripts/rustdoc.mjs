// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

/*
 * Builds the rustdoc of the published crates and stages it where the site can
 * serve it.
 *
 *   node ./scripts/rustdoc.mjs            # cargo doc, then stage
 *   node ./scripts/rustdoc.mjs --stage-only   # stage an existing target/doc
 *
 * What is documented is the selection below, not the whole workspace. Rustdoc
 * accumulates in `target/doc` — its search index and crate switcher remember
 * every crate ever documented there — so a build starts by clearing that
 * directory, and staging refuses a tree that holds anything but the selection.
 *
 * The output lands in `site/public/docs/api/`, which Astro copies verbatim into
 * `dist/` at build time, so it is served as plain files at `/docs/api/...` with
 * no route of our own in the way. It is gitignored: twenty-odd megabytes
 * of machine-written HTML has no place in the history, and one command
 * regenerates it.
 *
 * One deliberate omission: `target/doc` has no root `index.html` for a
 * workspace build, and if it ever grows one we skip it, because `/docs/api/`
 * redirects to our own crate-index page at `/docs`. Every other rustdoc artefact — the per-crate
 * trees, `static.files/`, the search index, `crates.js` — is copied as is, so
 * rustdoc's own search and cross-crate links keep working.
 */

import { spawnSync } from 'node:child_process';
import { cp, mkdir, rm, readdir, stat } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const siteDir = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const repoRoot = resolve(siteDir, '..');
const docDir = join(repoRoot, 'target', 'doc');
const stageDir = join(siteDir, 'public', 'docs', 'api');

// The published documentation: these packages' libraries…
const libraries = [
  // core and formats
  'tuile-core',
  'tuile-b3dm',
  'tuile-terrain',
  'tuile-pack',
  // sources and storage
  'tuile-native-fetchers',
  'tuile-web',
  'tuile-storage-foyer',
  'tuile-tile-server',
  'tuile-repository',
  'tuile-bing',
  'tuile-cesium-ion',
  // scene and rendering
  'tuile-planetary',
  'tuile-camera',
  'tuile-ui',
  'tuile-atmosphere',
  'tuile-radiometry',
  'tuile-wgpu',
  // film
  'tuile-bake',
  'tuile-film',
  'tuile-film-gpu',
  'tuile-film-native',
  'tuile-film-web',
  'tuile-mp4',
  // integrations
  'tuile-usd',
  'tuile-hydra',
  'tuile-metrics',
  'tuile-pack-worker',
];

// …and these programs, as [package, binary].
const programs = [
  ['tuile-film-native', 'tuile-film-render'],
  ['tuile-pack-api', 'tuile-pack-api'],
  ['wgpu-viewer', 'wgpu-viewer'],
];

// Held back for a later publication: tuile-tape and tuile-farm with all their
// tools, the tool of tuile-usd, and the examples other than the viewer.

const crateName = (n) => n.replaceAll('-', '_');
const expected = [...libraries, ...programs.map(([, bin]) => bin)].map(crateName).sort();

const packages = [...new Set([...libraries, ...programs.map(([pkg]) => pkg)])];
const cargoArgs = [
  'doc',
  '--no-deps',
  ...packages.flatMap((pkg) => ['-p', pkg]),
  '--lib',
  ...programs.flatMap(([, bin]) => ['--bin', bin]),
];

const stageOnly = process.argv.includes('--stage-only');

if (!stageOnly) {
  await rm(docDir, { recursive: true, force: true });
  console.log(`> cargo ${cargoArgs.join(' ')}`);
  const r = spawnSync('cargo', cargoArgs, {
    cwd: repoRoot,
    stdio: 'inherit',
  });
  if (r.error) {
    console.error(`cargo could not be started: ${r.error.message}`);
    process.exit(1);
  }
  if (r.status !== 0) {
    console.error(`cargo doc exited with ${r.status}; nothing staged.`);
    process.exit(r.status ?? 1);
  }
}

if (!existsSync(docDir)) {
  console.error(`no rustdoc at ${docDir} — run without --stage-only first.`);
  process.exit(1);
}

// A crate is a directory with its own index; `src/`, `static.files/` and the
// index directories are rustdoc's shared furniture.
const documented = (await readdir(docDir, { withFileTypes: true }))
  .filter((e) => e.isDirectory() && existsSync(join(docDir, e.name, 'index.html')))
  .map((e) => e.name)
  .sort();
const missing = expected.filter((c) => !documented.includes(c));
const extra = documented.filter((c) => !expected.includes(c));
if (missing.length || extra.length) {
  if (missing.length) console.error(`missing from ${docDir}: ${missing.join(', ')}`);
  if (extra.length) console.error(`not in the selection: ${extra.join(', ')}`);
  console.error('target/doc is not the selection; run without --stage-only to rebuild it.');
  process.exit(1);
}

await rm(stageDir, { recursive: true, force: true });
await mkdir(stageDir, { recursive: true });

const skip = new Set(['.lock', 'index.html']);
let files = 0;
let bytes = 0;

for (const entry of await readdir(docDir, { withFileTypes: true })) {
  if (skip.has(entry.name)) continue;
  await cp(join(docDir, entry.name), join(stageDir, entry.name), {
    recursive: true,
    dereference: true,
  });
  if (entry.isFile()) {
    files += 1;
    bytes += (await stat(join(docDir, entry.name))).size;
  }
}

const crates = (await readdir(stageDir, { withFileTypes: true }))
  .filter((e) => e.isDirectory() && existsSync(join(stageDir, e.name, 'index.html')))
  .map((e) => e.name)
  .sort();

console.log(`staged ${crates.length} crates into public/docs/api (${crates.join(', ')})`);
if (files) console.log(`plus ${files} shared files (${(bytes / 1024).toFixed(0)} KiB)`);
console.log('serve with: npm run build && npm run preview');
