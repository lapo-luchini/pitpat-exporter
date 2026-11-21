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
    // S7: take the paths from git. In a worktree (or a submodule) `.git`
    // is a file, so the literal `.git/…` paths never exist and cargo
    // rebuilt every time. `--git-path HEAD` is per worktree; the tags
    // live in the shared common dir. Git missing (a nix store copy)
    // prints nothing more; the version falls back as usual.
    let Some(head_path) = git_out(&["rev-parse", "--git-path", "HEAD"]) else {
        return;
    };
    let common = git_out(&["rev-parse", "--git-common-dir"]).unwrap_or_else(|| ".git".into());
    // S2: the version changes with the commit (HEAD) AND with the
    // nearest tag, which tag creation/deletion moves without touching
    // HEAD — so the tag files must also trigger a rebuild, or the
    // version goes stale after a release
    println!("cargo:rerun-if-changed={head_path}");
    // U1: watch the branches DIRECTORY, not the ref file: `git pack-refs`
    // (git gc runs it now and then) removes the loose file, and a missing
    // watched path makes cargo rebuild on every build. Cargo scans a
    // directory recursively, so a created file counts too.
    println!("cargo:rerun-if-changed={common}/refs/heads");
    println!("cargo:rerun-if-changed={common}/refs/tags");
    println!("cargo:rerun-if-changed={common}/packed-refs");
    let version = env::var("PITPAT_VERSION")
        .ok()
        .or_else(describe)
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=PITPAT_VERSION={version}");
}

/// One git invocation's trimmed stdout, or `None` on any failure.
fn git_out(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}
