# 20 — Conventions

## Rust

- Edition 2021+, MSRV pinned in the workspace Cargo.toml (choose current stable - 2).
- `cargo fmt` defaults; `clippy -D warnings` in CI; `#![deny(unsafe_code)]` everywhere except commented justification (likely: no unsafe needed in v1).
- Errors: `thiserror` in the libs, `anyhow` allowed only in the binaries/examples.
- No `unwrap`/`expect` in the prod paths of the libs; `expect` tolerated in tests and examples with a message.
- Public API documented (`#![warn(missing_docs)]` on core as soon as the API stabilizes — not blocking in M1).
- Logging: `tracing` everywhere, never `println!` in the libs.
- **Wasm: stable crates only.** In any path compiled to wasm, rely as much as possible on stable, widely adopted and actively maintained crates (wasm-bindgen, web-sys, js-sys, gloo, getrandom with the `js` feature). No experimental, exotic pre-1.0 or poorly maintained crate without written justification in the PR.
- Naming: types and docs in English (international open source project). The docs in the `docs/` folder remain as they are.

## Licenses and headers

- Root: `LICENSE-MIT`, `LICENSE-APACHE`, `NOTICE`.
- Header of each source file:
  ```rust
  // SPDX-License-Identifier: MIT OR Apache-2.0
  // Copyright (c) lapoule.dev
  ```
- Code ported from a third-party Apache-2.0 project (exceptional — prefer algorithmic inspiration, which creates no obligation): attribution comment at the head of the ported item + a line in `NOTICE` (project, source file, license).
- Fixtures: provenance and license noted in `fixtures/README.md`. Refuse any fixture without a clear license.

## Git and PRs

- Conventional commits: `feat(core): ...`, `fix(server): ...`, `docs:`, `test:`, `chore:`.
- One PR = one topic. Milestones are split into reviewable PRs (< ~800 lines of diff excluding fixtures/snapshots).
- Each PR: description of the what/why, milestone criteria checked off progressively in `docs/03-roadmap.md` (update the doc in the PR that validates the criterion).

## CI (GitHub Actions, `.github/workflows/ci.yml`)

Minimum jobs:
1. `fmt` + `clippy -D warnings` (linux).
2. `test`: linux + macos.
3. `wasm-guard`: `cargo check --target wasm32-unknown-unknown -p tuile-core` (and `-p tuile-server --no-default-features --features cloudflare` starting from M2).
4. `docs`: `cargo doc --no-deps` without warning.

No Windows CI in v1 (to be added on demand), no bench in CI (criterion locally).

## Project documentation

- Root `README.md`: pitch ("dynamic 3D Tiles server & renderer engine in Rust"), 5-line quickstart (`cargo install` + `tuile serve`), viewer GIF, table of crates, interoperability mention ("works with any OGC 3D Tiles client — web viewers, three.js 3DTilesRenderer…"), disclaimer ("3D Tiles is an OGC Community Standard. Tuile is an independent implementation, unaffiliated with any vendor.").
- Each crate: short README + doc-tested examples.
- `docs/manual-tests.md`: checklists of the manual verifications (web viewers, Quick Look) to run before release.

## Definition of "done"

A feature is done when: green tests + clean clippy + green wasm-guard + up-to-date doc + criterion checked in the roadmap. Not before.
