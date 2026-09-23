// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The same contract, against a real bucket.
//!
//! Ignored by default: it needs credentials and the network. Run it with the
//! `TUILE_STORE_*` variables set:
//!
//! ```text
//! cargo test -p tuile-farm --test bucket -- --ignored
//! ```
//!
//! Parts are 5 MiB, the smallest a bucket accepts, so the multipart path runs
//! on files of 15 MiB rather than hundreds.

use tuile_farm::{assert_run_store_contract, BucketConfig, ObjectRunStore, Tuning};

#[tokio::test]
#[ignore = "needs TUILE_STORE_* credentials and the network"]
async fn a_bucket_keeps_the_contract() {
    let config = BucketConfig::from_env().expect("TUILE_STORE_* must be set for this test");
    let tuning = Tuning { part_bytes: 5 << 20, concurrency: 8 };
    let store = ObjectRunStore::bucket(&config, tuning).expect("bucket");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis();
    assert_run_store_contract(&store, &format!("tuile-farm-contract/{stamp}"), tuning.part_bytes)
        .await;
}
