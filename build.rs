// SPDX-License-Identifier: ISC

//! R3/S2: `PITPAT_VERSION` for `env!()` in `main.rs`/`metrics.rs` —
//! `git describe --tags --match '[0-9]*'` (e.g. `1.3.0-3-gabc1234`) when
//! building from a checkout. An env var that is already set wins (the
//! nix build hands in the flake-computed version, since a store copy has
//! no `.git`); without git or without tags the Cargo.toml release
//! version is used.

use std::env;
use std::process::Command;

fn describe() -> Option<String> {
    let out = Command::new("git")
        .args(["describe", "--tags", "--match", "[0-9]*"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn main() {
    println!("cargo:rerun-if-env-changed=PITPAT_VERSION");
    // S2: the version changes with the commit (HEAD plus the ref it
    // points to) AND with the nearest tag, which tag creation/deletion
    // moves without touching HEAD — so the tag files must also trigger
    // a rebuild, or the version goes stale after a release
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/tags");
    println!("cargo:rerun-if-changed=.git/packed-refs");
    if let Ok(head) = std::fs::read_to_string(".git/HEAD") {
        if let Some(r) = head.trim().strip_prefix("ref: ") {
            println!("cargo:rerun-if-changed=.git/{r}");
        }
    }
    let version = env::var("PITPAT_VERSION")
        .ok()
        .or_else(describe)
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=PITPAT_VERSION={version}");
}
