// SPDX-License-Identifier: ISC

//! R3: `PITPAT_VERSION` for `env!()` in `main.rs`/`metrics.rs` —
//! `git describe --tags --dirty --match '[0-9]*'` (e.g. `1.3.0-3-gabc1234`,
//! `-dirty` on a modified tree) when building from a checkout. An env var
//! that is already set wins (the nix build hands in the flake-computed
//! version, since a store copy has no `.git`); without git or without
//! tags the Cargo.toml release version is used.

use std::env;
use std::process::Command;

fn describe() -> Option<String> {
    let out = Command::new("git")
        .args(["describe", "--tags", "--dirty", "--match", "[0-9]*"])
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
    // R3: the version changes with the commit: HEAD plus the ref it
    // points to (a direct sha in HEAD is covered by the HEAD file)
    println!("cargo:rerun-if-changed=.git/HEAD");
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
