// SPDX-License-Identifier: ISC

//! Version-consistency tests (R3/R5.2). Compiled under `#[cfg(test)]`
//! only, from `main.rs`.

use serde_json::Value;
use std::process::Command;

// R3: the plasmoid's committed metadata.json carries the plain release
// version; it must equal CARGO_PKG_VERSION so a `kpackagetool6` install
// from a checkout and `pitpat --version` cannot drift apart.
#[test]
fn metadata_version_matches() {
    let meta: Value =
        serde_json::from_str(include_str!("../plasmoid/it.lapo.pitpat/metadata.json"))
            .expect("metadata.json is valid JSON");
    assert_eq!(
        meta["KPlugin"]["Version"],
        Value::String(env!("CARGO_PKG_VERSION").to_string()),
        "plasmoid metadata.json KPlugin.Version vs Cargo.toml version"
    );
}

/// Numeric version compare, so 1.10.0 > 1.9.0 (a string sort would not).
fn is_newer(a: &str, b: &str) -> bool {
    let num = |s: &str| {
        s.split('.')
            .map(|x| x.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let (pa, pb) = (num(a), num(b));
    for i in 0..3 {
        let (x, y) = (
            pa.get(i).copied().unwrap_or(0),
            pb.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

// R5.2: keep the tags and the Cargo.toml version in step. Fails when a
// release tag is newer than the crate version (tagged without the bump)
// and when HEAD sits exactly on a tag that differs from it; the window
// between the bump commit and the tag (crate newer, HEAD untagged)
// passes, as do git-less builds (nix, tarballs) with a printed note.
#[test]
fn version_matches_git_tag() {
    let Ok(out) = Command::new("git")
        .args(["describe", "--tags", "--match", "[0-9]*", "--abbrev=0"])
        .output()
    else {
        eprintln!("note: git not available (nix build?); skipping the tag check");
        return;
    };
    if !out.status.success() {
        eprintln!("note: no release tags in this tree; skipping the tag check");
        return;
    }
    let tag = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let crate_version = env!("CARGO_PKG_VERSION");
    assert!(
        !is_newer(&tag, crate_version),
        "tag {tag} is newer than Cargo.toml's {crate_version}: bump before tagging"
    );
    // HEAD exactly on a tag: it must be the crate version
    if let Ok(exact) = Command::new("git")
        .args(["describe", "--tags", "--exact-match", "--match", "[0-9]*"])
        .output()
    {
        if exact.status.success() {
            let head_tag = String::from_utf8_lossy(&exact.stdout).trim().to_string();
            assert_eq!(
                head_tag, crate_version,
                "HEAD is tagged {head_tag} but Cargo.toml says {crate_version}"
            );
        }
    }
    eprintln!("note: nearest tag {tag}, crate {crate_version}, HEAD untagged: in step");
}
