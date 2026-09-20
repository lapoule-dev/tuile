// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

import { defineConfig } from 'astro/config';
import node from '@astrojs/node';

// SSR with the standalone Node adapter: `npm run build` emits
// `dist/server/entry.mjs`, which also serves everything in `dist/client`
// (that is where `public/` lands, rustdoc included).
export default defineConfig({
  output: 'server',
  adapter: node({ mode: 'standalone' }),
  server: { port: 4321 },
  devToolbar: { enabled: false },
  build: { inlineStylesheets: 'always' },
});
