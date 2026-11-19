//! A release `ck` ignores the test-profile variables (D21): `ck paths` reports
//! the user's normal folders and creates nothing.
//!
//! Release builds only: `cargo test -p ck-cli --release --locked --test
//! release_build`. It runs `ck paths` alone, which never starts a supervisor
//! or a service.
#![cfg(not(debug_assertions))]

#[path = "support/temp.rs"]
mod temp;

use std::fs;
use std::path::Path;
use std::process::Command;

use ck_core::paths::{DATA_VAR, HOME_VAR};
use serde_json::Value;

use temp::{TempRoot, is_inside};

#[test]
fn a_release_build_ignores_the_test_profile() {
    let root = TempRoot::create();
    let output = Command::new(env!("CARGO_BIN_EXE_ck"))
        .args(["paths", "--json"])
        .env(HOME_VAR, root.path())
        .env(DATA_VAR, root.path().join("data"))
        .output()
        .expect("run ck paths --json");
    assert!(output.status.success(), "{output:?}");

    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(document["schema"].as_u64(), Some(1), "{document}");
    assert_eq!(document["ok"].as_bool(), Some(true), "{document}");
    assert_eq!(document["build"].as_str(), Some("release"), "{document}");
    assert_eq!(document["profile"].as_str(), Some("standard"), "{document}");
    let data = Path::new(document["data"].as_str().expect("a data folder"));
    assert!(!is_inside(data, root.path()), "{document}");
    assert!(
        fs::read_dir(root.path())
            .expect("list the temporary folder")
            .next()
            .is_none(),
        "ck paths created something in {}",
        root.path().display()
    );
}
