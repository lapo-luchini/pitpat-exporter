// SPDX-License-Identifier: ISC

//! Version-consistency tests (R3/R5.2). Compiled under `#[cfg(test)]`
//! only, from `main.rs`.

use serde_json::Value;

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
