<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# The Tuile site

The project's marketing and documentation site: Astro, server-side rendered
through the standalone Node adapter, with the workspace's rustdoc served
alongside it.

It is a single-page application in the sense that matters — `ClientRouter`
(Astro's view transitions) intercepts in-site navigation, so moving between
pages swaps the document without a full reload. Links into `/docs/api/` are
real navigations, because rustdoc is a separate document tree with its own
scripts.

## Commands

All of these run from this directory.

| Command | What it does |
|---|---|
| `npm install` | Install dependencies. |
| `npm run dev` | Development server on <http://localhost:4321>. |
| `npm run docs:api` | `cargo doc --workspace --no-deps`, then stage the result under `public/docs/api/`. |
| `npm run build` | Astro SSR build into `dist/`. |
| `npm start` | Serve the build: `node ./dist/server/entry.mjs`. |
| `npm run build:all` | `docs:api` then `build`, in that order. |

The usual first run:

```bash
npm install
npm run docs:api          # a couple of minutes cold, seconds afterwards
npm run build
node ./dist/server/entry.mjs      # http://localhost:4321
```

`PORT=8080 node ./dist/server/entry.mjs` moves it; `HOST=0.0.0.0` exposes it.

## How the Rust API documentation is wired

`cargo doc --workspace --no-deps` writes rustdoc into the workspace's
`target/doc`. `scripts/rustdoc.mjs` runs that and then copies the tree into
`public/docs/api/`, which Astro copies verbatim into `dist/client/` at build
time; the standalone Node server serves `dist/client`, so the docs land at
`/docs/api/<crate>/index.html`.

There is deliberately no route handler in the middle. Rustdoc's relative links,
its shared assets under `static.files/` and its search index all work
unmodified precisely because nothing rewrites them.

The one thing not copied is a root `index.html`, so `/docs/api/` stays the
site's own crate-index page (`src/pages/docs/api/index.astro`). Everything
below that path is rustdoc's.

**Ordering matters**: `docs:api` must run *before* `build`, because the staging
directory is read by Astro's build when it copies `public/`. Re-staging after a
build without rebuilding will not reach `dist/`. `npm run build:all` gets the
order right.

The generated tree is about 22 MB and is gitignored (`public/docs/api/`), along
with `dist/`, `node_modules/` and `.astro/`. A fresh checkout therefore starts
with the API links returning 404, and the API page says so on the page itself
rather than pretending otherwise.

## Layout

```
site/
├── astro.config.mjs        # output: 'server' + @astrojs/node (standalone)
├── scripts/rustdoc.mjs     # cargo doc → public/docs/api/
├── src/
│   ├── layouts/Base.astro  # shell, nav, ClientRouter, footer
│   ├── pages/
│   │   ├── index.astro             /                what Tuile is and why
│   │   ├── architecture.astro      /architecture    crates, seams, rules
│   │   ├── 3d-tiles.astro          /3d-tiles        the standard, condensed
│   │   ├── roadmap.astro           /roadmap         milestones and criteria
│   │   ├── getting-started.astro   /getting-started the real cargo commands
│   │   ├── docs/api/index.astro    /docs/api        rustdoc entry point
│   │   └── 404.astro
│   └── styles/global.css   # every colour is a token; dark via prefers-color-scheme
└── public/docs/api/        # generated, gitignored
```

## Writing for this site

Two constraints inherited from the repository's rules, and they are not
negotiable:

1. **No third-party brand** in the copy. The project defines itself by the OGC
   3D Tiles Community Standard, never relative to another product. Describe the
   gap in the ecosystem; do not name who occupies it. The single exception in
   the codebase — connector crates named after the service they connect to —
   stays out of the prose, which is why the API page hides those two crate names
   from its index and points at rustdoc's own crate switcher instead.
2. **SPDX headers** on every source file: `MIT OR Apache-2.0`, copyright
   lapoule.dev.

The content is written from `docs/00-vision.md`, `docs/01-architecture.md`,
`docs/02-3d-tiles-primer.md`, `docs/03-roadmap.md` and the per-crate specs. When
those change, the pages should follow — and where the site would have to claim
something the workspace does not yet contain, it says so plainly instead.
