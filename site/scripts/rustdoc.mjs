// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

/*
 * Builds the workspace rustdoc and stages it where the site can serve it.
 *
 *   node ./scripts/rustdoc.mjs            # cargo doc, then stage
 *   node ./scripts/rustdoc.mjs --stage-only   # stage an existing target/doc
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

const stageOnly = process.argv.includes('--stage-only');

if (!stageOnly) {
  console.log('> cargo doc --workspace --no-deps');
  const r = spawnSync('cargo', ['doc', '--workspace', '--no-deps'], {
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
