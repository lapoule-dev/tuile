// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

import { defineConfig } from 'astro/config';

// A static build: no page needs a server, so `npm run build` emits plain files
// into `dist/` (that is where `public/` lands, rustdoc included) and any static
// host can serve them. `wrangler.toml` describes the deployment.
export default defineConfig({
  output: 'static',
  // The entry page of the API reference used to live one level down.
  redirects: { '/docs/api': '/docs' },
  server: { port: 4321 },
  devToolbar: { enabled: false },
  // `format: 'file'` writes `docs.html` rather than `docs/index.html`, so the
  // slash-less addresses the site links to are served without a redirect.
  build: { format: 'file', inlineStylesheets: 'always' },
});
