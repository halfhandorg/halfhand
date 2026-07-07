//! NFR-2 no-network guarantee (SRS §2.1/NFR-2; ARCHITECTURE.md
//! "No-network guarantee").
//!
//! Halfhand is local-first: the recorder must never gain the ability to
//! exfiltrate recorded agent data. The cheapest, most durable enforcement is a
//! tripwire on the resolved dependency tree — if an HTTP *client* crate ever
//! lands in the graph (directly or transitively), this test fails and names the
//! offender before it ships.
//!
//! This runs `cargo metadata`, which resolves from the on-disk `Cargo.lock`
//! and does **not** touch the network — so the no-network test itself needs no
//! network. It covers the whole workspace (`hh-core`, `hh-record`, `hh`) and
//! every transitive package.

use std::process::Command;

/// Crates whose purpose is acting as an HTTP *client*. Any of these in the
/// graph would let the binary speak outbound HTTP, violating NFR-2.
///
/// `hyper`, `http`, and `hyper-util` are deliberately **not** listed: they are
/// generic transport/type crates that can appear transitively (e.g. behind a
/// dev-tool) without enabling outbound HTTP from the recorder. The crates here
/// are unambiguous HTTP clients — a real tripwire, not a false-alarm net.
const HTTP_CLIENT_CRATES: &[&str] = &[
    "reqwest",
    "ureq",
    "isahc",
    "attohttpc",
    "surf",
    "minreq",
    "curl",
    "wreq",
    "crabq",
    "async-h1",
];

/// `cargo metadata` resolves the full workspace graph (all three crates and
/// their transitive deps) from `Cargo.lock` without network access.
#[test]
fn workspace_dependency_tree_has_no_http_client() {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1"])
        .output()
        .expect("`cargo metadata` must be runnable for the NFR-2 test");
    assert!(
        output.status.success(),
        "`cargo metadata` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let meta: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("`cargo metadata` emits valid JSON");
    let packages = meta["packages"]
        .as_array()
        .expect("`metadata.packages` is an array");

    let mut names: Vec<&str> = packages.iter().filter_map(|p| p["name"].as_str()).collect();
    names.sort_unstable();
    names.dedup();

    let offenders: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| HTTP_CLIENT_CRATES.contains(n))
        .collect();
    assert!(
        offenders.is_empty(),
        "NFR-2 violation: HTTP client crate(s) present in the workspace dependency tree: {offenders:?}. \
         Halfhand must not depend on an HTTP client; see ARCHITECTURE.md."
    );
}
