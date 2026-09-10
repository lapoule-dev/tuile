// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The generated code is committed. This is what stops it drifting.
//!
//! `flatc` is deliberately not a build dependency: the Rust stage of
//! `Dockerfile.globe` is a bare `rust:1-bookworm`, and a generator that is not
//! on the machine is a build nobody can reproduce. The cost of committing the
//! output is that an edit to the `.fbs` can land without it — the schema says
//! one thing, the code another, and nothing complains until a reader
//! misinterprets a field.
//!
//! So: where `flatc` exists, this regenerates and compares. Where it does not,
//! it says so out loud and passes, because a machine without the generator has
//! no way to know and refusing to build there would help nobody.

use std::process::Command;

#[test]
fn the_generated_code_matches_the_schema() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let committed = std::fs::read_to_string(root.join("src/tuile_pack_generated.rs"))
        .expect("the generated code is committed");

    let out = match Command::new("flatc")
        .arg("--rust")
        .arg("--gen-all")
        .arg("-o")
        .arg(std::env::temp_dir().join("tuile-pack-schema-check"))
        .arg(root.join("schema/tuile_pack.fbs"))
        .output()
    {
        Ok(out) => out,
        Err(e) => {
            eprintln!(
                "flatc is not on this machine ({e}); the committed code is taken \
                 on trust here. Run this where flatc exists before changing the schema."
            );
            return;
        }
    };
    assert!(
        out.status.success(),
        "flatc failed on the schema: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let fresh = std::fs::read_to_string(
        std::env::temp_dir()
            .join("tuile-pack-schema-check")
            .join("tuile_pack_generated.rs"),
    )
    .expect("flatc wrote its output");

    assert_eq!(
        fresh.trim(),
        committed.trim(),
        "schema/tuile_pack.fbs and src/tuile_pack_generated.rs disagree. \
         Regenerate and commit:\n    \
         flatc --rust --gen-all -o crates/tuile-pack/src \
         crates/tuile-pack/schema/tuile_pack.fbs"
    );
}
