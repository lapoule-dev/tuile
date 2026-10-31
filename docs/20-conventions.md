# 20 — Conventions

## Rust

- Edition 2021. MSRV pinned in the workspace `Cargo.toml` (`rust-version = "1.97"`, raised with the move to wgpu 30) and checked by its own CI job on a pinned patch release (`msrv`, Rust 1.97.1); every other job runs on stable.
- `cargo fmt` defaults; `clippy -D warnings` in CI.
- Workspace lints: `unsafe_code = "deny"` everywhere — an exception carries its justification in a comment; `clippy::unwrap_used` and `clippy::panic` warn, so CI denies them, and tests are exempt (`clippy.toml`).
- Errors: `thiserror` in the libs, `anyhow` allowed only in the binaries/examples.
- No `unwrap`/`expect` in the prod paths of the libs; `expect` tolerated in tests and examples with a message.
- Public API documented; `cargo doc --workspace --no-deps` builds with `-D warnings` in CI. Intra-doc links must resolve.
- Logging: `tracing` everywhere, never `println!` in the libs.
- **Wasm: stable crates only.** In any path compiled to wasm, rely as much as possible on stable, widely adopted and actively maintained crates (wasm-bindgen, web-sys, js-sys, gloo, getrandom with the `js` feature). No experimental, exotic pre-1.0 or poorly maintained crate without written justification in the PR.
- **A bug in a dependency is fixed in a fork**, not worked around: fork under `lapoule-dev`, a tested fix, the revision pinned in `Cargo.toml`.
- Naming: types and docs in English (international open source project).

## Licenses and headers

- Root: `LICENSE-MIT`, `LICENSE-APACHE`, `NOTICE`.
- Header of each source file:
  ```rust
  // SPDX-License-Identifier: MIT OR Apache-2.0
  // Copyright (c) lapoule.dev
  ```
- Code ported from a third-party Apache-2.0 project (exceptional — prefer algorithmic inspiration, which creates no obligation): attribution comment at the head of the ported item + a line in `NOTICE` (project, source file, license).
- Fixtures: provenance and license noted in a README beside them (`crates/<crate>/tests/fixtures/README.md`). Refuse any fixture without a clear license.

## Git and PRs

- Conventional commits: `feat(core): ...`, `fix(viewer): ...`, `docs:`, `test:`, `chore:`, `ci:`.
- No assistant attribution in commit messages (no `Co-Authored-By` naming an assistant, no session links). A commit carrying one is refused by `.githooks/commit-msg` — enable it once per clone with `git config core.hooksPath .githooks` — and by the `commit-messages` CI job on every pull request.
- One PR = one topic. Milestones are split into reviewable PRs (< ~800 lines of diff excluding fixtures/snapshots).
- Each PR: description of the what/why; when it meets a roadmap criterion, tick it in `docs/03-roadmap.md` in the same PR.
- **Work in a git worktree**, one per topic, branched from `origin/main` — never in the main checkout.
- **One build directory per worktree.** Never share a `CARGO_TARGET_DIR` between two worktrees of this repository: cargo names artefacts by path relative to the workspace root, so two worktrees produce the same artefact names and one silently compiles against the other's sources. Delete the build directory once the PR is pushed; to save disk, `cargo clean` and remove worktrees, never share.
- **Dependent PRs are a stack** (GitHub's native stacked pull requests, the `gh stack` extension of the GitHub CLI) — never a branch silently based on another branch:
  - the bottom PR targets `main`; each PR above targets the branch of the one below;
  - every PR of a stack is reviewed on its own; merging a PR merges every PR below it, so a PR is merged only once all the PRs under it are approved;
  - merge bottom-up with the **merge commit** method only — never squash, never rebase-merge;
  - when `main` moves, rebase the whole stack — never merge `main` into a branch of the stack; a layer the rebase leaves empty is dropped;
  - a contributor's PR from a fork cannot join a stack: it is rebased onto `main` and merged on its own.
- Releases are cumulative and in order: a `chore: version X.Y.Z` PR, merged, then the tag. The tag triggers the release workflows — the viewer applications, and crates.io when it is turned on — which never tag or bump a version themselves: see `docs/releasing.md`.

## Tests

- Every public trait has tests; every parser is tested on real data.
- **A green suite is not proof.** Write the test in the same change as the behaviour, then revert the behaviour and confirm the test fails. Say plainly what a test does not cover.
- Never pipe a test, build or clippy run through `head` or `tail`: redirect the whole run to a file and grep it.
- **A render is a picture**: when a run produces a render, open it and look at it. Renders go to `videos/` (gitignored), with their parameters in `videos/README.md`.

## CI (GitHub Actions, `.github/workflows/ci.yml`)

Jobs:
1. `lint`: `cargo fmt --check` + `cargo clippy --workspace --all-targets -D warnings` (Linux, stable).
2. `msrv`: check + clippy on Rust 1.97.1, the pinned `rust-version`.
3. `test`: `cargo test --workspace` on `ubuntu-24.04` and `macos-latest`, `fail-fast: false`. The Linux image is pinned because it is also the Vulkan driver (lavapipe) the headless GPU tests render through; moving it is a deliberate change.
4. `budget-sim`: the graphics budget's simulation and property tests in release (10,000 property cases), golden decision digests checked on Linux as on the Mac that recorded them.
5. `wasm-guard`: `cargo check --target wasm32-unknown-unknown` with `-D warnings` for every crate that claims wasm (core, web, wgpu, planetary, terrain, camera, atmosphere, film, film-gpu, ui), and clippy for the viewer's web shell.
6. `web-package`: the wasm build and bindings of `packages/tuile-viewer` (wasm-bindgen at the locked version), its typings, and a bundler build of `examples/web-viewer-vite`.
7. `android`: clippy for `aarch64-linux-android` and the APK through `crates/tuile-viewer/android/build.sh`.
8. `docs`: `cargo doc --workspace --no-deps` with `-D warnings`.

Release workflows, on a `vX.Y.Z` tag: `release-apps.yml` (macOS, iOS, Android) and `release-crates.yml` (crates.io, behind the `CRATES_IO_PUBLISH` variable) — `docs/releasing.md`.

No Windows CI (to be added on demand), no bench in CI (criterion locally).

## Project documentation

- Root `README.md`: what tuile is, what is in it, the crates, getting started. The disclaimer stands: 3D Tiles is an OGC Community Standard; Tuile is an independent implementation, unaffiliated with any vendor.
- Each crate: a README, included as the crate's rustdoc (`#![doc = include_str!("../README.md")]`), with doc-tested examples.
- `docs/`: the design documents; `docs/explorations/`: research notes, measurements and plans, which are not specifications.
- `site/`: the project site and the API reference, built from these documents and from rustdoc (`site/README.md`).

## Definition of "done"

A feature is done when: green tests + clean clippy + green wasm-guard + up-to-date doc + criterion checked in the roadmap. Not before.
