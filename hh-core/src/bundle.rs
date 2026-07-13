//! Portable session bundle format (`hh export --bundle` / `hh import`).
//!
//! A bundle is a single zstd-compressed tar containing:
//! - `manifest.json` — `format_version`, the `hh` version that wrote it, a
//!   redacted session block, and integrity hashes.
//! - `events.ndjson` — one compact JSON object per event, in original
//!   `(ts_ms, id)` order, position-indexed by `seq` (DB row ids do not
//!   survive import, so `correlates` is expressed as `correlates_seq`
//!   instead).
//! - `blobs/<hash[0..2]>/<hash>` — raw content for every blob any event or
//!   file_change references. Only the outer tar is zstd-compressed; blobs are
//!   stored raw inside it (no double compression).
//!
//! Tar entries are added in a fixed order (manifest, events, then blobs
//! sorted ascending by hash) with fixed mtime/uid/gid/mode, so two exports of
//! an unchanged session produce byte-identical bundles.
//!
//! [`export`] is the trusted-data path (built from a live [`crate::store::Store`]).
//! [`parse`] is the untrusted-input path (`hh import file.hh`): it must never
//! panic on malformed input, and every failure mode reports precisely what
//! is wrong (v1.0.0 addendum).

use crate::error::{BundleError, Result};
use crate::event::{FileChange, RawEventRow, SessionRow};
use crate::redact::Detectors;
use crate::store::Store;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;

/// The bundle format version this build of `hh` writes, and the highest
/// version it will read. Bumped only for a breaking change to the manifest/
/// events/blob layout; new *optional* fields do not require a bump.
pub const FORMAT_VERSION: u32 = 1;

/// zstd compression level for the outer bundle stream. 3 matches the level
/// [`crate::blob::BlobStore`] already uses — fast and compact for the mixed
/// text/binary content a session bundle carries.
const ZSTD_LEVEL: i32 = 3;

/// Safety cap on decompressed bundle size (defense in depth against a
/// hostile/corrupt zstd frame claiming an enormous content size). 2 GiB is
/// far above any real session; `hh import` on a legitimate bundle never
/// approaches it.
const MAX_BUNDLE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// A parsed, validated bundle — the output of [`parse`] and the input to
/// [`crate::store::Store::import`].
#[derive(Debug, Clone)]
pub struct Bundle {
    /// The `hh` version string that produced this bundle.
    pub hh_version: String,
    /// The session block from `manifest.json` (already redacted at export
    /// time, if redaction was enabled).
    pub session: serde_json::Value,
    /// Events in original `(ts_ms, id)` order, `seq`-indexed (see module docs).
    pub events: Vec<serde_json::Value>,
    /// Every blob referenced by `events`, keyed by its (possibly
    /// redaction-remapped) BLAKE3 hash.
    pub blobs: BTreeMap<String, Vec<u8>>,
}

/// Build a portable bundle for `session_id` (the trusted-data path).
///
/// `hh_version` is the caller's version string (`hh-core` has no build-time
/// access to the binary's version). `detectors` mirrors the existing
/// `hh export`/`hh export --html` redaction chokepoint: `Some` redacts the
/// session block, every event, and every referenced blob's content before
/// anything is packed; `None` (only reachable via the interactive
/// `--no-redact` confirmation) packs the session as-is.
///
/// # Errors
///
/// Propagates any storage/blob-store failure reading the session.
pub fn export(
    store: &Store,
    session_id: &str,
    hh_version: &str,
    detectors: Option<&Detectors>,
) -> Result<Vec<u8>> {
    let session_row = store.get_session(session_id)?;

    let mut raw_events: Vec<RawEventRow> = Vec::new();
    store.for_each_event_raw(session_id, |row| {
        raw_events.push(row);
        Ok(())
    })?;
    let id_to_seq: HashMap<i64, u64> = raw_events
        .iter()
        .enumerate()
        .map(|(i, r)| (r.id, u64::try_from(i).unwrap_or(u64::MAX)))
        .collect();

    // Every blob any event or file_change references — a portable bundle
    // must carry these byte-for-byte (see `Store::for_each_event_raw`'s docs
    // for why `for_each_event_detail`'s resolved body is not enough).
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    for r in &raw_events {
        if let Some(h) = &r.blob_hash {
            referenced.insert(h.clone());
        }
        if let Some(fc) = &r.file_change {
            if let Some(h) = &fc.before_hash {
                referenced.insert(h.clone());
            }
            if let Some(h) = &fc.after_hash {
                referenced.insert(h.clone());
            }
        }
    }

    // Fetch (+ redact) every referenced blob, tracking hashes that changed
    // under redaction so event/file_change references can be remapped below.
    let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut remap: HashMap<String, String> = HashMap::new();
    for hash in &referenced {
        let content = store.blobs().get(hash)?;
        match detectors.and_then(|d| d.redact_bytes(&content)) {
            Some(redacted) => {
                let new_hash = crate::blob::BlobStore::hash(&redacted.bytes);
                if &new_hash != hash {
                    remap.insert(hash.clone(), new_hash.clone());
                }
                blobs.insert(new_hash, redacted.bytes);
            }
            None => {
                blobs.insert(hash.clone(), content);
            }
        }
    }

    let mut events_json: Vec<serde_json::Value> = Vec::with_capacity(raw_events.len());
    for (seq, r) in raw_events.iter().enumerate() {
        let correlates_seq = r.correlates.and_then(|cid| id_to_seq.get(&cid).copied());
        let file_change_json = r.file_change.as_ref().map(file_change_to_json);
        let mut ev = serde_json::json!({
            "seq": u64::try_from(seq).unwrap_or(u64::MAX),
            "ts_ms": r.ts_ms,
            "kind": r.kind.to_string(),
            "step": r.step,
            "correlates_seq": correlates_seq,
            "summary": r.summary,
            "body": r.body_json,
            "blob_hash": r.blob_hash,
            "blob_size": r.blob_size,
            "file_change": file_change_json,
        });
        if let Some(d) = detectors {
            let _ = d.redact_json(&mut ev);
        }
        if !remap.is_empty() {
            remap_hashes(&mut ev, &remap);
        }
        events_json.push(ev);
    }

    let mut session_json = session_to_bundle_json(&session_row);
    if let Some(d) = detectors {
        let _ = d.redact_json(&mut session_json);
    }

    let mut ndjson = String::new();
    for ev in &events_json {
        let line = serde_json::to_string(ev).map_err(|e| BundleError::Events(e.to_string()))?;
        ndjson.push_str(&line);
        ndjson.push('\n');
    }
    let events_bytes = ndjson.into_bytes();
    let events_blake3 = blake3::hash(&events_bytes).to_hex().to_string();

    let blob_list: Vec<serde_json::Value> = blobs
        .iter()
        .map(|(hash, content)| serde_json::json!({ "hash": hash, "size": content.len() }))
        .collect();
    let manifest = serde_json::json!({
        "format_version": FORMAT_VERSION,
        "hh_version": hh_version,
        "session": session_json,
        "integrity": {
            "events_blake3": events_blake3,
            "event_count": events_json.len(),
            "blobs": blob_list,
        },
    });
    let manifest_bytes =
        serde_json::to_vec(&manifest).map_err(|e| BundleError::Manifest(e.to_string()))?;

    let tar_bytes = build_tar(&manifest_bytes, &events_bytes, &blobs)?;
    let compressed = zstd::stream::encode_all(&tar_bytes[..], ZSTD_LEVEL)
        .map_err(|e| BundleError::Zstd(e.to_string()))?;
    Ok(compressed)
}

/// Parse and validate an untrusted bundle (`hh import file.hh`). Never
/// panics: every malformed-input path returns a precise [`BundleError`].
///
/// # Errors
///
/// See [`BundleError`]'s variants for every rejection reason (bad zstd, bad
/// tar entry, missing/malformed manifest, unsupported `format_version`,
/// blob hash mismatch, missing referenced blob, events digest mismatch).
pub fn parse(bytes: &[u8]) -> Result<Bundle> {
    let tar_bytes = bounded_decompress(bytes)?;
    let (manifest_bytes, events_bytes, blobs) = extract_tar_entries(&tar_bytes)?;

    let manifest_bytes =
        manifest_bytes.ok_or_else(|| BundleError::Manifest("missing manifest.json".into()))?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| BundleError::Manifest(e.to_string()))?;
    let (hh_version, session, expected_digest) = parse_manifest(&manifest)?;

    let events_bytes =
        events_bytes.ok_or_else(|| BundleError::Events("missing events.ndjson".into()))?;
    let actual_digest = blake3::hash(&events_bytes).to_hex().to_string();
    if expected_digest != actual_digest {
        return Err(BundleError::IntegrityMismatch.into());
    }
    let events = parse_events(&events_bytes, &blobs)?;

    Ok(Bundle {
        hh_version,
        session,
        events,
        blobs,
    })
}

/// Walk every tar entry, verify each blob's content against its filename
/// hash, and sort them into `(manifest.json, events.ndjson, blobs)`. Rejects
/// any entry outside the allow-list (see [`classify_entry`]) and any
/// non-regular-file entry (symlink/hardlink/directory).
#[allow(clippy::type_complexity)] // the three extracted pieces have no natural shared struct
fn extract_tar_entries(
    tar_bytes: &[u8],
) -> Result<(Option<Vec<u8>>, Option<Vec<u8>>, BTreeMap<String, Vec<u8>>)> {
    let mut archive = tar::Archive::new(tar_bytes);
    let mut manifest_bytes: Option<Vec<u8>> = None;
    let mut events_bytes: Option<Vec<u8>> = None;
    let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();

    let entries = archive
        .entries()
        .map_err(|e| BundleError::Tar(e.to_string()))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| BundleError::Tar(e.to_string()))?;
        if entry.header().entry_type().ne(&tar::EntryType::Regular) {
            return Err(BundleError::Tar(
                "bundle contains a non-regular-file entry (symlink/hardlink/directory) — refusing"
                    .into(),
            )
            .into());
        }
        let path = entry.path().map_err(|e| BundleError::Tar(e.to_string()))?;
        let path_str = path.to_string_lossy().into_owned();
        let mut data = Vec::new();
        entry
            .read_to_end(&mut data)
            .map_err(|e| BundleError::Tar(e.to_string()))?;
        match classify_entry(&path_str) {
            EntryKind::Manifest => manifest_bytes = Some(data),
            EntryKind::Events => events_bytes = Some(data),
            EntryKind::Blob(hash) => {
                let actual = blake3::hash(&data).to_hex().to_string();
                if actual != hash {
                    return Err(BundleError::HashMismatch {
                        expected: hash,
                        actual,
                    }
                    .into());
                }
                blobs.insert(hash, data);
            }
            EntryKind::Unknown => {
                return Err(
                    BundleError::Tar(format!("unexpected entry in bundle: {path_str}")).into(),
                );
            }
        }
    }
    Ok((manifest_bytes, events_bytes, blobs))
}

/// Validate `manifest`'s `format_version` and pull out `(hh_version, session,
/// expected events.ndjson digest)`.
fn parse_manifest(manifest: &serde_json::Value) -> Result<(String, serde_json::Value, String)> {
    let format_version = manifest
        .get("format_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| BundleError::Manifest("missing or non-numeric format_version".into()))?;
    let format_version = u32::try_from(format_version)
        .map_err(|_| BundleError::Manifest("format_version out of range".into()))?;
    if format_version > FORMAT_VERSION {
        return Err(BundleError::UnsupportedVersion {
            found: format_version,
            max: FORMAT_VERSION,
        }
        .into());
    }

    let hh_version = manifest
        .get("hh_version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let session = manifest
        .get("session")
        .cloned()
        .ok_or_else(|| BundleError::Manifest("missing session block".into()))?;
    let expected_digest = manifest
        .get("integrity")
        .and_then(|i| i.get("events_blake3"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok((hh_version, session, expected_digest))
}

/// Parse `events.ndjson` line by line and confirm every blob hash any event
/// or `file_change` references is among `blobs`.
fn parse_events(
    events_bytes: &[u8],
    blobs: &BTreeMap<String, Vec<u8>>,
) -> Result<Vec<serde_json::Value>> {
    let mut events = Vec::new();
    for line in events_bytes.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_slice(line).map_err(|e| BundleError::Events(e.to_string()))?;
        events.push(v);
    }

    for ev in &events {
        if let Some(h) = ev.get("blob_hash").and_then(serde_json::Value::as_str) {
            if !blobs.contains_key(h) {
                return Err(BundleError::MissingBlob(h.to_string()).into());
            }
        }
        if let Some(fc) = ev.get("file_change").filter(|v| !v.is_null()) {
            for key in ["before_hash", "after_hash"] {
                if let Some(h) = fc.get(key).and_then(serde_json::Value::as_str) {
                    if !blobs.contains_key(h) {
                        return Err(BundleError::MissingBlob(h.to_string()).into());
                    }
                }
            }
        }
    }
    Ok(events)
}

/// Decompress `bytes` as zstd, bounded to [`MAX_BUNDLE_BYTES`] — defense in
/// depth against a hostile/corrupt frame that claims an enormous content
/// size (`Read::take` stops pulling bytes from the decoder once the cap is
/// hit, so this never allocates unbounded memory).
fn bounded_decompress(bytes: &[u8]) -> Result<Vec<u8>> {
    let decoder =
        zstd::stream::Decoder::new(bytes).map_err(|e| BundleError::Zstd(e.to_string()))?;
    let mut limited = decoder.take(MAX_BUNDLE_BYTES + 1);
    let mut out = Vec::new();
    limited
        .read_to_end(&mut out)
        .map_err(|e| BundleError::Zstd(e.to_string()))?;
    if out.len() as u64 > MAX_BUNDLE_BYTES {
        return Err(BundleError::Tar(format!(
            "decompressed bundle exceeds the {MAX_BUNDLE_BYTES}-byte safety limit"
        ))
        .into());
    }
    Ok(out)
}

/// What a tar entry's path means, or [`EntryKind::Unknown`] if it matches
/// nothing on the allow-list (`manifest.json` / `events.ndjson` /
/// `blobs/<2hex>/<64hex>`). This allow-list is also the path-traversal
/// defense: parsing never writes to the filesystem (entries are read
/// straight into memory), so an unexpected path cannot escape anywhere — it
/// is simply rejected as an unrecognized entry.
enum EntryKind {
    Manifest,
    Events,
    Blob(String),
    Unknown,
}

fn classify_entry(path: &str) -> EntryKind {
    if path == "manifest.json" {
        return EntryKind::Manifest;
    }
    if path == "events.ndjson" {
        return EntryKind::Events;
    }
    if let Some(rest) = path.strip_prefix("blobs/") {
        if let Some((prefix, hash)) = rest.split_once('/') {
            if prefix.len() == 2
                && hash.len() == 64
                && hash.starts_with(prefix)
                && hash.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return EntryKind::Blob(hash.to_string());
            }
        }
    }
    EntryKind::Unknown
}

/// Append one entry to `builder` with fixed mtime/uid/gid/mode (determinism:
/// identical content at the same path always produces the same header).
fn append_entry(builder: &mut tar::Builder<Vec<u8>>, path: &str, data: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header
        .set_path(path)
        .map_err(|e| BundleError::Tar(e.to_string()))?;
    header.set_size(data.len() as u64);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, path, data)
        .map_err(|e| BundleError::Tar(e.to_string()))?;
    Ok(())
}

/// Build the deterministic tar: manifest, then events, then blobs sorted
/// ascending by hash (`blobs` is a `BTreeMap`, so iteration is already
/// sorted).
fn build_tar(manifest: &[u8], events: &[u8], blobs: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    append_entry(&mut builder, "manifest.json", manifest)?;
    append_entry(&mut builder, "events.ndjson", events)?;
    for (hash, content) in blobs {
        let path = format!("blobs/{}/{}", &hash[..2], hash);
        append_entry(&mut builder, &path, content)?;
    }
    builder
        .into_inner()
        .map_err(|e| BundleError::Tar(e.to_string()).into())
}

/// The `session` block for `manifest.json`. Mirrors the shape of `hh list
/// --json`'s per-session object (`session_to_json` in the `hh` binary) so
/// the two stay recognizable as the same "session" concept, without hh-core
/// depending on the binary crate.
fn session_to_bundle_json(row: &SessionRow) -> serde_json::Value {
    let duration_ms = row.ended_at.map(|end| (end - row.started_at).max(0));
    serde_json::json!({
        "schema": 1,
        "id": row.id,
        "short_id": row.short_id,
        "status": row.status.to_string(),
        "agent_kind": row.agent_kind.to_string(),
        "adapter_status": row.adapter_status.to_string(),
        "started_at": row.started_at,
        "ended_at": row.ended_at,
        "exit_code": row.exit_code,
        "duration_ms": duration_ms,
        "steps": row.step_count,
        "files_changed": row.files_changed,
        "command": row.command,
        "cwd": row.cwd.to_string_lossy(),
        "imported_from": row.imported_from,
    })
}

/// The JSON object for a `file_changes` row, as carried in a bundle event.
fn file_change_to_json(fc: &FileChange) -> serde_json::Value {
    serde_json::json!({
        "path": fc.path,
        "change_kind": fc.change_kind.to_string(),
        "before_hash": fc.before_hash,
        "after_hash": fc.after_hash,
        "is_binary": fc.is_binary,
    })
}

/// Replace every string leaf in `value` that exactly matches a key in
/// `remap` with its mapped value. Used after blob redaction changes a blob's
/// hash: rewrites every reference to the old hash (the event's top-level
/// `blob_hash`, a `file_change`'s `before_hash`/`after_hash`, and any
/// embedded `blob_hash` inside an unresolved overflow envelope) to the new
/// one, wherever it appears in the event's JSON tree — one generic walk
/// instead of special-casing each field.
fn remap_hashes(value: &mut serde_json::Value, remap: &HashMap<String, String>) {
    match value {
        serde_json::Value::String(s) => {
            if let Some(new_hash) = remap.get(s.as_str()) {
                *s = new_hash.clone();
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                remap_hashes(v, remap);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                remap_hashes(v, remap);
            }
        }
        _ => {}
    }
}

/// Fuzz-only entry point (`cargo fuzz` target `import_bundle`). Gated behind
/// the `fuzzing` feature so it never widens the crate's normal public API.
#[cfg(feature = "fuzzing")]
pub mod fuzzing {
    /// Fuzz [`super::parse`] on arbitrary bytes — must never panic regardless
    /// of malformed zstd/tar/JSON/hash content.
    pub fn fuzz_parse(bytes: &[u8]) {
        let _ = super::parse(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RedactionConfig;
    use crate::event::{
        AdapterStatus, AgentKind, ChangeKind, Event, EventKind, NewSession, SessionStatus,
    };
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn new_session() -> NewSession {
        NewSession {
            id: crate::event::now_v7(),
            started_at: 1_700_000_000_000,
            agent_kind: AgentKind::Generic,
            adapter_status: AdapterStatus::None,
            command: vec!["agent".into()],
            cwd: PathBuf::from("/tmp/proj"),
            hostname: Some("devbox".into()),
            hh_version: "test".into(),
            model: None,
            git_branch: None,
            git_sha: None,
            git_dirty: None,
        }
    }

    /// A store with one session: a plain user_message step, a correlated
    /// tool_call/tool_result pair, and a file change with real blob content —
    /// enough surface to exercise seq/correlates_seq and blob carrying.
    fn fixture() -> (TempDir, Store, String) {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        let created = store.create_session(&new_session()).unwrap();
        let sid = created.id.clone();
        let writer = store.event_writer().unwrap();
        writer
            .append_event(Event {
                session_id: sid.clone(),
                ts_ms: 0,
                kind: EventKind::UserMessage,
                step: None,
                summary: "hello".into(),
                body_json: Some(serde_json::json!({"text": "hello there"})),
                blob_hash: None,
                blob_size: None,
                correlates: None,
            })
            .unwrap();
        let call = writer
            .append_event(Event {
                session_id: sid.clone(),
                ts_ms: 1_000,
                kind: EventKind::ToolCall,
                step: None,
                summary: "tool_call: Bash".into(),
                body_json: Some(serde_json::json!({"name": "Bash", "input": {"command": "ls"}})),
                blob_hash: None,
                blob_size: None,
                correlates: None,
            })
            .unwrap();
        writer
            .append_event(Event {
                session_id: sid.clone(),
                ts_ms: 1_500,
                kind: EventKind::ToolResult,
                step: None,
                summary: "tool_result: ok".into(),
                body_json: Some(serde_json::json!({"content": "file.txt", "is_error": false})),
                blob_hash: None,
                blob_size: None,
                correlates: Some(call),
            })
            .unwrap();
        let after = store.blobs().put(b"created content\n").unwrap();
        writer
            .append_file_change(
                Event {
                    session_id: sid.clone(),
                    ts_ms: 2_000,
                    kind: EventKind::FileChange,
                    step: None,
                    summary: "created.txt created".into(),
                    body_json: Some(serde_json::json!({"path": "created.txt"})),
                    blob_hash: Some(after.hash.clone()),
                    blob_size: Some(after.size),
                    correlates: None,
                },
                FileChange {
                    event_id: 0,
                    path: "created.txt".into(),
                    change_kind: ChangeKind::Created,
                    before_hash: None,
                    after_hash: Some(after.hash.clone()),
                    is_binary: false,
                },
            )
            .unwrap();
        writer.finish().unwrap();
        store.assign_steps(&sid).unwrap();
        store
            .finalize_session(&sid, 1_700_000_003_000, Some(0), SessionStatus::Ok)
            .unwrap();
        (tmp, store, sid)
    }

    #[test]
    fn export_then_parse_round_trips_the_shape() {
        let (_tmp, store, sid) = fixture();
        let bytes = export(&store, &sid, "test-version", None).unwrap();
        let bundle = parse(&bytes).unwrap();
        assert_eq!(bundle.hh_version, "test-version");
        assert_eq!(
            bundle.session["short_id"],
            store.get_session(&sid).unwrap().short_id
        );
        assert_eq!(bundle.events.len(), 4);
        // The tool_result (seq 2) correlates back to the tool_call (seq 1).
        assert_eq!(bundle.events[2]["kind"], "tool_result");
        assert_eq!(bundle.events[2]["correlates_seq"], 1);
        assert_eq!(bundle.events[1]["correlates_seq"], serde_json::Value::Null);
        // The file_change's blob content is carried.
        let after_hash = bundle.events[3]["file_change"]["after_hash"]
            .as_str()
            .unwrap();
        assert_eq!(bundle.blobs[after_hash], b"created content\n");
    }

    #[test]
    fn export_is_deterministic() {
        let (_tmp, store, sid) = fixture();
        let a = export(&store, &sid, "v", None).unwrap();
        let b = export(&store, &sid, "v", None).unwrap();
        assert_eq!(
            a, b,
            "identical session must produce byte-identical bundles"
        );
    }

    #[test]
    fn redaction_scrubs_session_events_and_blob_and_remaps_the_hash() {
        let (_tmp, store, sid) = fixture();
        // Seed a secret into an event body and into the file-change blob.
        let writer = store.event_writer().unwrap();
        writer
            .append_event(Event {
                session_id: sid.clone(),
                ts_ms: 3_000,
                kind: EventKind::AgentMessage,
                step: None,
                summary: "leaking a key".into(),
                body_json: Some(serde_json::json!({"text": "key AKIAIOSFODNN7EXAMPLE end"})),
                blob_hash: None,
                blob_size: None,
                correlates: None,
            })
            .unwrap();
        writer.finish().unwrap();
        store.assign_steps(&sid).unwrap();

        let secret_blob = store.blobs().put(b"secret AKIAIOSFODNN7EXAMPLE\n").unwrap();
        writer_append_file_change_with_secret(&store, &sid, &secret_blob.hash);

        let detectors = Detectors::new(&RedactionConfig::default()).unwrap();
        let bytes = export(&store, &sid, "v", Some(&detectors)).unwrap();
        let bundle = parse(&bytes).unwrap();

        let joined = serde_json::to_string(&bundle.events).unwrap();
        assert!(
            !joined.contains("AKIAIOSFODNN7EXAMPLE"),
            "secret must not appear in events: {joined}"
        );
        for content in bundle.blobs.values() {
            let text = String::from_utf8_lossy(content);
            assert!(
                !text.contains("AKIAIOSFODNN7EXAMPLE"),
                "secret must not appear in any blob: {text}"
            );
        }
        // The redacted blob is stored under its new (redacted-content) hash,
        // and every reference to it in events.ndjson was remapped to match —
        // no event/file_change points at the pre-redaction hash.
        assert!(
            !bundle.blobs.contains_key(&secret_blob.hash),
            "the original (unredacted) blob hash must not survive export"
        );
    }

    fn writer_append_file_change_with_secret(store: &Store, sid: &str, hash: &str) {
        let writer = store.event_writer().unwrap();
        writer
            .append_file_change(
                Event {
                    session_id: sid.to_string(),
                    ts_ms: 4_000,
                    kind: EventKind::FileChange,
                    step: None,
                    summary: "secret.txt created".into(),
                    body_json: Some(serde_json::json!({"path": "secret.txt"})),
                    blob_hash: Some(hash.to_string()),
                    blob_size: Some(27),
                    correlates: None,
                },
                FileChange {
                    event_id: 0,
                    path: "secret.txt".into(),
                    change_kind: ChangeKind::Created,
                    before_hash: None,
                    after_hash: Some(hash.to_string()),
                    is_binary: false,
                },
            )
            .unwrap();
        writer.finish().unwrap();
        store.assign_steps(sid).unwrap();
    }

    #[test]
    fn parse_rejects_newer_format_version() {
        let manifest = serde_json::json!({
            "format_version": FORMAT_VERSION + 1,
            "hh_version": "future",
            "session": {},
            "integrity": {"events_blake3": blake3::hash(b"").to_hex().to_string(), "event_count": 0, "blobs": []},
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let tar_bytes = build_tar(&manifest_bytes, b"", &BTreeMap::new()).unwrap();
        let compressed = zstd::stream::encode_all(&tar_bytes[..], ZSTD_LEVEL).unwrap();
        let err = parse(&compressed).unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Bundle(BundleError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn parse_rejects_a_tampered_blob() {
        let (_tmp, store, sid) = fixture();
        let bytes = export(&store, &sid, "v", None).unwrap();
        let tar_bytes = bounded_decompress(&bytes).unwrap();
        let mut archive = tar::Archive::new(&tar_bytes[..]);
        // Rebuild the tar with one blob's content corrupted so its hash no
        // longer matches its path.
        let mut builder = tar::Builder::new(Vec::new());
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            if path.starts_with("blobs/") {
                data.push(0xFF);
            }
            append_entry(&mut builder, &path, &data).unwrap();
        }
        let tampered_tar = builder.into_inner().unwrap();
        let compressed = zstd::stream::encode_all(&tampered_tar[..], ZSTD_LEVEL).unwrap();
        let err = parse(&compressed).unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Bundle(BundleError::HashMismatch { .. })
        ));
    }

    #[test]
    fn parse_rejects_garbage_bytes_without_panicking() {
        for input in [&b""[..], &b"not a bundle"[..], &[0u8; 4096][..]] {
            let err = parse(input);
            assert!(err.is_err(), "garbage input must be rejected, not accepted");
        }
    }

    #[test]
    fn parse_rejects_an_unexpected_tar_entry() {
        let mut builder = tar::Builder::new(Vec::new());
        let manifest = serde_json::json!({
            "format_version": 1,
            "hh_version": "v",
            "session": {},
            "integrity": {"events_blake3": blake3::hash(b"").to_hex().to_string(), "event_count": 0, "blobs": []},
        });
        append_entry(
            &mut builder,
            "manifest.json",
            &serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        append_entry(&mut builder, "events.ndjson", b"").unwrap();
        append_entry(&mut builder, "readme.txt", b"not on the allow-list").unwrap();
        let tar_bytes = builder.into_inner().unwrap();
        let compressed = zstd::stream::encode_all(&tar_bytes[..], ZSTD_LEVEL).unwrap();
        let err = parse(&compressed).unwrap_err();
        assert!(matches!(err, crate::Error::Bundle(BundleError::Tar(_))));
    }

    #[test]
    fn store_import_mints_a_new_id_and_records_provenance() {
        let (_tmp, store, sid) = fixture();
        let original = store.get_session(&sid).unwrap();
        let bytes = export(&store, &sid, "v", None).unwrap();
        let bundle = parse(&bytes).unwrap();

        let target_tmp = TempDir::new().unwrap();
        let target = Store::open(
            &target_tmp.path().join("hh.db"),
            &target_tmp.path().join("blobs"),
        )
        .unwrap();
        let created = target.import(&bundle).unwrap();
        assert_ne!(created.id, original.id, "import must mint a fresh id");

        let imported = target.get_session(&created.id).unwrap();
        assert_eq!(
            imported.imported_from.as_deref(),
            Some(original.id.as_str())
        );
        assert_eq!(imported.command, original.command);
        assert_eq!(imported.status, original.status);

        let mut events = Vec::new();
        target
            .for_each_event_raw(&created.id, |r| {
                events.push(r);
                Ok(())
            })
            .unwrap();
        assert_eq!(events.len(), 4, "every bundled event must be re-inserted");
        // The tool_result correlates to the re-inserted tool_call, not any
        // stale id from the source store.
        let call = events
            .iter()
            .find(|e| e.kind == EventKind::ToolCall)
            .unwrap();
        let result = events
            .iter()
            .find(|e| e.kind == EventKind::ToolResult)
            .unwrap();
        assert_eq!(result.correlates, Some(call.id));
        // The file_change's blob content is retrievable in the new store.
        let fc_event = events
            .iter()
            .find(|e| e.kind == EventKind::FileChange)
            .unwrap();
        let fc = fc_event.file_change.as_ref().unwrap();
        let after_hash = fc.after_hash.as_ref().unwrap();
        assert_eq!(
            target.blobs().get(after_hash).unwrap(),
            b"created content\n"
        );
    }

    #[test]
    fn store_import_into_the_same_store_as_the_source_dedupes_blobs() {
        // Importing back into the store the bundle was exported from is a
        // legitimate use (e.g. "restore from an archived bundle") and must
        // not corrupt the source session's blobs (content-addressing makes
        // re-`put`ting identical content a no-op).
        let (_tmp, store, sid) = fixture();
        let bytes = export(&store, &sid, "v", None).unwrap();
        let bundle = parse(&bytes).unwrap();
        let created = store.import(&bundle).unwrap();
        assert_ne!(created.id, sid);
        // The original session is untouched.
        let original_still_ok = store.get_session(&sid).unwrap();
        assert_eq!(original_still_ok.imported_from, None);
    }
}
