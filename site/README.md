<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# The Tuile site

The project's marketing and documentation site: Astro, built statically, with
the workspace's rustdoc served alongside it. It is published at
<https://tuile.lapoule.dev>, the API reference under
<https://tuile.lapoule.dev/docs>.

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
| `npm run docs:api` | `cargo doc --no-deps` for the published crates, then stage the result under `public/docs/api/`. |
| `npm run build` | Static build into `dist/`. |
| `npm run preview` | Serve `dist/` locally on <http://localhost:4321>. |
| `npm run build:all` | `docs:api` then `build`, in that order. |
| `npm run deploy` | Publish `dist/` (see *Publishing* below). |

The usual first run:

```bash
npm install
npm run docs:api          # a couple of minutes cold, seconds afterwards
npm run build
npm run preview           # http://localhost:4321
```

`npm run preview -- --port 8080` moves it; `--host` exposes it.

## How the Rust API documentation is wired

Not every crate of the workspace is documented here: `scripts/rustdoc.mjs`
holds the list — twenty-seven libraries and three programs — and
`src/pages/docs/index.astro` groups the same names under headings. The crates
that record and replay camera paths, the ones that drive a render farm, their
tools and most examples are held back for a later publication. Adding a crate
means adding it to both files.

The script clears `target/doc`, because rustdoc's search index and crate
switcher remember every crate ever documented there, runs `cargo doc --no-deps`
for the selection, checks that what came out is exactly the selection, and then
copies the tree into
`public/docs/api/`, which Astro copies verbatim into `dist/` at build time;
`dist/` is served as plain files, so the docs land at
`/docs/api/<crate>/index.html`.

There is deliberately no route handler in the middle. Rustdoc's relative links,
its shared assets under `static.files/` and its search index all work
unmodified precisely because nothing rewrites them.

The entry point is `/docs`, the site's own crate-index page
(`src/pages/docs/index.astro`), which links into the generated tree one level
down. Everything below `/docs/api/` is rustdoc's; the one thing not copied is a
root `index.html`, and `/docs/api` itself redirects to `/docs`.

**Ordering matters**: `docs:api` must run *before* `build`, because the staging
directory is read by Astro's build when it copies `public/`. Re-staging after a
build without rebuilding will not reach `dist/`. `npm run build:all` gets the
order right.

The generated tree is about 45 MB in some 1,700 files and is gitignored (`public/docs/api/`), along
with `dist/`, `node_modules/` and `.astro/`. A fresh checkout therefore starts
with the API links returning 404, and the API page says so on the page itself
rather than pretending otherwise.

## Publishing

The build is a folder of files, so publishing is uploading `dist/`.
`wrangler.toml` describes it as a Cloudflare Worker made of static assets only
— no script — and the custom domain is attached at deploy time:

```bash
npm run build:all
CLOUDFLARE_ACCOUNT_ID=<the account that owns the zone> \
  wrangler deploy --domain tuile.lapoule.dev      # what `npm run deploy` runs
```

Two settings in that file carry behaviour. `html_handling =
"auto-trailing-slash"` serves an `.html` file at its extensionless address:
`/docs` answers with `docs.html` (the build writes one file per page, see
`build.format` in `astro.config.mjs`), and a link to `tuile_core/index.html` is
redirected to `tuile_core/` — the same directory, so rustdoc's relative links
still resolve. `not_found_handling = "404-page"` answers
an unknown path with `dist/404.html` and a 404 status.

The platform caps a deployment's file count and each file's size (25 MiB). The
rustdoc tree is nearly all of the count; `wrangler deploy` prints it, and
refuses outright past the cap rather than publishing part of a tree.

## Layout

```
site/
├── astro.config.mjs        # output: 'static', one .html file per page
├── wrangler.toml           # the deployment: dist/ as static assets
├── scripts/rustdoc.mjs     # cargo doc → public/docs/api/
├── src/
│   ├── layouts/Base.astro  # shell, nav, ClientRouter, footer
│   ├── pages/
│   │   ├── index.astro             /                what Tuile is and why
│   │   ├── architecture.astro      /architecture    crates, seams, rules
│   │   ├── 3d-tiles.astro          /3d-tiles        the standard, condensed
│   │   ├── roadmap.astro           /roadmap         milestones and criteria
│   │   ├── getting-started.astro   /getting-started the real cargo commands
│   │   ├── docs/index.astro        /docs            rustdoc entry point
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
   stays out of the prose: the API page lists those two crates by their crate
   name, as it does every other, and says nothing more about them.
2. **SPDX headers** on every source file: `MIT OR Apache-2.0`, copyright
   lapoule.dev.

The content is written from `docs/00-vision.md`, `docs/01-architecture.md`,
`docs/02-3d-tiles-primer.md`, `docs/03-roadmap.md` and the per-crate specs. When
those change, the pages should follow — and where the site would have to claim
something the workspace does not yet contain, it says so plainly instead.
