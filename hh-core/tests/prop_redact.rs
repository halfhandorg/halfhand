//! Property tests for the redaction pipeline (docs/redaction-design.md).
//!
//! Two contracts are locked here:
//!
//! 1. **Detection** — a named-type secret embedded in arbitrary surrounding
//!    text is always found (false negatives on named types are bugs), and
//!    redacted output is a detection fixed point.
//! 2. **Irreversibility** (invariants I1–I3) — after `Store::redact_session`
//!    on a session seeded with secrets in every location (summary, inline
//!    body, overflowed blob, file-change blob, terminal output, command
//!    line), no detector match survives anywhere: `scan_session` is empty,
//!    a byte-search of the raw `hh.db` file finds nothing (the checkpoint +
//!    VACUUM step), every surviving blob decompresses clean, and the blob
//!    refcount books still balance.

use hh_core::event::{ChangeKind, Event, EventKind, FileChange, NewSession};
use hh_core::redact::{Detectors, SecretKind};
use hh_core::{Config, RedactionConfig, Store};
use proptest::prelude::*;
use std::path::Path;

fn detectors() -> Detectors {
    Detectors::new(&RedactionConfig::default()).expect("built-ins compile")
}

/// Deterministically build one secret of each named type from a numeric
/// seed, so shrinking stays meaningful. The generated values match the real
/// token shapes (charset + length).
fn seeded_secret(kind: usize, seed: u64) -> (SecretKind, String) {
    // A seeded charset expander: repeats the hex of `seed` mapped into
    // `alphabet` until `len` chars.
    fn expand(alphabet: &[u8], seed: u64, len: usize) -> String {
        let mut out = String::with_capacity(len);
        let mut x = seed | 1;
        for _ in 0..len {
            // xorshift64 — deterministic, spread across the alphabet.
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let idx = usize::try_from(x % alphabet.len() as u64).unwrap();
            out.push(alphabet[idx] as char);
        }
        out
    }
    const UPPER_NUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    match kind % 6 {
        0 => (
            SecretKind::AwsAccessKeyId,
            format!("AKIA{}", expand(UPPER_NUM, seed, 16)),
        ),
        1 => (
            SecretKind::GithubToken,
            format!("ghp_{}", expand(ALNUM, seed, 36)),
        ),
        2 => (
            SecretKind::GitlabToken,
            format!("glpat-{}", expand(ALNUM, seed, 20)),
        ),
        3 => (
            SecretKind::SlackToken,
            format!("xoxb-{}", expand(ALNUM, seed, 24)),
        ),
        4 => (
            SecretKind::PrivateKey,
            format!(
                "-----BEGIN PRIVATE KEY-----\n{}\n{}\n-----END PRIVATE KEY-----",
                expand(ALNUM, seed, 48),
                expand(ALNUM, seed.wrapping_add(1), 48),
            ),
        ),
        _ => (
            SecretKind::Jwt,
            format!(
                "eyJ{}.eyJ{}.{}",
                expand(ALNUM, seed, 20),
                expand(ALNUM, seed.wrapping_add(1), 20),
                expand(ALNUM, seed.wrapping_add(2), 20),
            ),
        ),
    }
}

/// Surrounding context that guarantees a clean token boundary (non-word,
/// non-charset chars at the joins). Detection inside identifiers is
/// deliberately not claimed — `\b`-anchored patterns are the contract.
fn contexts() -> impl Strategy<Value = (String, String)> {
    let side = || {
        prop::string::string_regex("[ \\t\\n\"':,;(){}<>|#*a-zA-Z0-9_./-]{0,60}")
            .expect("valid strategy regex")
    };
    (side(), side()).prop_map(|(mut pre, mut post): (String, String)| {
        // Force a hard boundary character at each join.
        pre.push(' ');
        post.insert(0, '\n');
        (pre, post)
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// Named token types are always detected in arbitrary surroundings, the
    /// finding covers the secret, redaction removes it, and the output is a
    /// fixed point. False negatives here are bugs by contract.
    #[test]
    fn named_secret_always_detected_and_redacted(
        kind in 0usize..6,
        seed in any::<u64>(),
        (pre, post) in contexts(),
    ) {
        let d = detectors();
        let (want_kind, secret) = seeded_secret(kind, seed);
        let text = format!("{pre}{secret}{post}");

        let findings = d.detect(&text);
        let covering = findings
            .iter()
            .find(|f| text[f.start..f.end].contains(&secret) || secret.contains(&text[f.start..f.end]));
        prop_assert!(
            covering.is_some(),
            "secret of kind {want_kind} not covered by any finding in {text:?}: {findings:?}"
        );

        let redacted = d.redact_text(&text).expect("must redact");
        prop_assert!(
            !redacted.text.contains(&secret),
            "secret survived redaction: {}",
            redacted.text
        );
        prop_assert!(redacted.text.contains("{{REDACTED:"), "token missing");
        prop_assert!(
            d.detect(&redacted.text).is_empty(),
            "redacted output must be a fixed point: {}",
            redacted.text
        );
    }

    /// Detection and redaction never panic on arbitrary input, and redacted
    /// output (when any) is always a fixed point — the same invariant the
    /// `redact_detect` fuzz target drives with coverage feedback.
    #[test]
    fn arbitrary_input_never_panics(text in ".{0,300}") {
        let d = detectors();
        let _ = d.detect(&text);
        if let Some(r) = d.redact_text(&text) {
            prop_assert!(d.detect(&r.text).is_empty());
        }
        let _ = d.redact_bytes(text.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// Storage-level irreversibility property (invariants I1–I3).
// ---------------------------------------------------------------------------

/// Build a finalized session seeded with `secrets` spread across every
/// location redaction must reach. Returns the session id.
#[allow(clippy::too_many_lines)] // one fixture, one location per block — splitting obscures it
fn seed_session(store: &Store, dir: &Path, secrets: &[String]) -> String {
    let new = NewSession {
        id: hh_core::event::now_v7(),
        started_at: 1,
        agent_kind: hh_core::AgentKind::Generic,
        adapter_status: hh_core::AdapterStatus::None,
        // Location: the recorded command line.
        command: vec!["agent".into(), format!("--token={}", secrets[0])],
        cwd: dir.to_path_buf(),
        hostname: None,
        hh_version: "test".into(),
        model: None,
        git_branch: None,
        git_sha: None,
        git_dirty: None,
    };
    let created = store.create_session(&new).expect("create session");
    let id = created.id;
    let writer = store.event_writer().expect("writer");
    let mut ts = 0i64;
    let mut next_ts = || {
        ts += 10;
        ts
    };
    for (i, secret) in secrets.iter().enumerate() {
        // Location: summary (truncation keeps short secrets intact; long
        // ones are covered by the body copy below).
        writer
            .append_event(Event {
                session_id: id.clone(),
                ts_ms: next_ts(),
                kind: EventKind::AgentMessage,
                step: None,
                summary: hh_core::truncate_summary(&format!("leak {i}: {secret}")),
                body_json: Some(serde_json::json!({ "text": format!("body leak: {secret}") })),
                blob_hash: None,
                blob_size: None,
                correlates: None,
            })
            .expect("append");
        // Location: a blob (an overflowed tool-result payload).
        let payload =
            serde_json::to_vec(&serde_json::json!({ "content": format!("blob leak: {secret}") }))
                .expect("serialize");
        let put = store.blobs().put(&payload).expect("put blob");
        writer
            .append_event(Event {
                session_id: id.clone(),
                ts_ms: next_ts(),
                kind: EventKind::ToolResult,
                step: None,
                summary: "tool_result".into(),
                body_json: Some(serde_json::json!({
                    "overflow": true,
                    "size": put.size,
                    "blob_hash": put.hash,
                    "encoding": "blob",
                })),
                blob_hash: Some(put.hash.clone()),
                blob_size: Some(put.size),
                correlates: None,
            })
            .expect("append blob event");
        // Location: a file-change snapshot blob.
        let file_content = format!("API_KEY={secret}\n");
        let fput = store.blobs().put(file_content.as_bytes()).expect("put");
        writer
            .append_file_change(
                Event {
                    session_id: id.clone(),
                    ts_ms: next_ts(),
                    kind: EventKind::FileChange,
                    step: None,
                    summary: format!(".env.{i} modified"),
                    body_json: Some(serde_json::json!({
                        "path": format!(".env.{i}"),
                        "change_kind": "modified",
                    })),
                    blob_hash: Some(fput.hash.clone()),
                    blob_size: Some(fput.size),
                    correlates: None,
                },
                FileChange {
                    event_id: 0,
                    path: format!(".env.{i}"),
                    change_kind: ChangeKind::Modified,
                    before_hash: None,
                    after_hash: Some(fput.hash.clone()),
                    is_binary: false,
                },
            )
            .expect("append file change");
        // Location: raw terminal output.
        writer
            .append_event(Event {
                session_id: id.clone(),
                ts_ms: next_ts(),
                kind: EventKind::TerminalOutput,
                step: None,
                summary: "terminal output".into(),
                body_json: Some(serde_json::json!({
                    "text": format!("$ export TOKEN={secret}\n"),
                    "encoding": "utf8",
                })),
                blob_hash: None,
                blob_size: None,
                correlates: None,
            })
            .expect("append terminal");
    }
    writer.finish().expect("finish writer");
    store
        .finalize_session(&id, 2_000, Some(0), hh_core::SessionStatus::Ok)
        .expect("finalize");
    store.assign_steps(&id).expect("steps");
    id
}

/// Assert the refcount books balance: every `blobs` row's refcount equals
/// the number of events referencing its hash, and every referenced blob is
/// fetchable (invariant I3).
fn assert_refcounts_balanced(dir: &Path, store: &Store) {
    let conn = rusqlite::Connection::open(dir.join("hh.db")).expect("open db");
    let mut stmt = conn
        .prepare(
            "SELECT b.hash, b.refcount,
                    (SELECT COUNT(*) FROM events e WHERE e.blob_hash = b.hash)
             FROM blobs b",
        )
        .expect("prepare");
    let rows: Vec<(String, i64, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    for (hash, refcount, live_refs) in rows {
        assert_eq!(
            refcount, live_refs,
            "blob {hash} refcount {refcount} != {live_refs} live references"
        );
        assert!(
            store.blobs().get(&hash).is_ok(),
            "referenced blob {hash} must be fetchable"
        );
    }
}

proptest! {
    // Each case builds a real store + session on disk; keep the case count
    // modest (the detection-side properties above run 128 cases cheaply).
    #![proptest_config(ProptestConfig { cases: 12, ..ProptestConfig::default() })]

    /// Invariants I1–I3: after `redact_session`, the seeded secrets are gone
    /// from every corner of the store — scan is empty, the raw DB file
    /// contains no secret bytes, every blob decompresses clean — and the
    /// refcount books balance. The audit event exists and carries no secret.
    #[test]
    fn redact_session_leaves_no_trace(
        kinds in prop::collection::vec(0usize..6, 1..4),
        seed in any::<u64>(),
    ) {
        let d = detectors();
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let dir = tmp.path();
        let store = Store::open(&dir.join("hh.db"), &dir.join("blobs")).expect("open store");
        let secrets: Vec<String> = kinds
            .iter()
            .enumerate()
            .map(|(i, k)| seeded_secret(*k, seed.wrapping_add(i as u64)).1)
            .collect();
        let id = seed_session(&store, dir, &secrets);

        // Pre-condition: the scan sees the seeded secrets.
        let before = store.scan_session(&id, &d).expect("scan before");
        prop_assert!(!before.is_empty(), "seeded session must scan dirty");

        let outcome = store.redact_session(&id, &d).expect("redact");
        prop_assert!(outcome.events_rewritten > 0);
        prop_assert!(!outcome.secrets.is_empty());

        // I1a: the scan is clean.
        let after = store.scan_session(&id, &d).expect("scan after");
        prop_assert!(after.is_empty(), "scan after redact must be clean: {after:?}");

        // I1b: no secret bytes survive in the raw DB file (checkpoint +
        // VACUUM purged WAL and freelist copies).
        let db_bytes = std::fs::read(dir.join("hh.db")).expect("read db");
        let wal = std::fs::read(dir.join("hh.db-wal")).unwrap_or_default();
        for secret in &secrets {
            for chunk in secret.split('\n') {
                let needle = chunk.as_bytes();
                prop_assert!(
                    !db_bytes.windows(needle.len()).any(|w| w == needle),
                    "secret bytes survived in hh.db"
                );
                prop_assert!(
                    wal.is_empty() || !wal.windows(needle.len()).any(|w| w == needle),
                    "secret bytes survived in hh.db-wal"
                );
            }
        }

        // I2: every blob on disk decompresses clean; originals are gone.
        for hash in store.blobs().iter_hashes().expect("iter") {
            let content = store.blobs().get(&hash).expect("get blob");
            prop_assert!(
                d.detect_bytes(&content).is_empty(),
                "blob {hash} still contains a detectable secret"
            );
        }

        // I3: refcount books balance and referenced blobs exist.
        assert_refcounts_balanced(dir, &store);

        // I4: the audit event exists, is a lifecycle step, and carries only
        // types + hash8 tallies.
        let audit = store.get_event_detail(outcome.audit_event_id).expect("audit");
        prop_assert_eq!(audit.kind, EventKind::Lifecycle);
        prop_assert!(audit.step.is_some(), "audit event must get a step ordinal");
        let body = audit.body_json.expect("audit body");
        prop_assert!(body.get("redaction_audit").is_some());
        let body_text = body.to_string();
        for secret in &secrets {
            for chunk in secret.split('\n') {
                prop_assert!(!body_text.contains(chunk), "audit must not leak the secret");
            }
        }

        // Idempotency: a second redact finds nothing new to rewrite.
        let again = store.redact_session(&id, &d).expect("redact again");
        prop_assert_eq!(again.events_rewritten, 0, "second redact must be a no-op");
        prop_assert_eq!(again.blobs_rewritten, 0);
    }
}

/// `Config::default()` is used above only for its `RedactionConfig`; keep a
/// compile-time check that the default config carries the redaction section
/// (guards against the section being dropped from `Config`).
#[test]
fn default_config_has_redaction_defaults() {
    let cfg = Config::default();
    assert!(!cfg.redaction.at_record);
    assert!(cfg.redaction.entropy);
}
