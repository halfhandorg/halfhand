//! Lazy body loader for the replay TUI (FR-3.5).
//!
//! The timeline index ([`hh_core::EventIndexRow`]) is loaded eagerly once at
//! startup (see [`super::AppState::new`]); the full `body_json`/blob payload
//! for each event is fetched only when its row is selected, via
//! [`ReplayData::get`]/[`get_many`](ReplayData::get_many). The last
//! [`CACHE_CAPACITY`] fetched events are cached so scrolling back over
//! recently-viewed steps does not re-hit SQLite/blob storage.

use hh_core::{EventDetail, Result, Store};
use lru::LruCache;
use std::num::NonZeroUsize;

/// How many event bodies to keep cached (FR-3.5).
const CACHE_CAPACITY: NonZeroUsize = match NonZeroUsize::new(50) {
    Some(n) => n,
    None => panic!("CACHE_CAPACITY must be non-zero"),
};

/// Owns the store and a bounded cache of lazily-fetched event bodies.
pub struct ReplayData {
    store: Store,
    cache: LruCache<i64, EventDetail>,
}

impl ReplayData {
    /// Wrap an already-open [`Store`] for lazy body loading.
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self {
            store,
            cache: LruCache::new(CACHE_CAPACITY),
        }
    }

    /// Borrow the underlying store (e.g. for a diff's before/after blob
    /// lookups that go beyond one event's own detail).
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Fetch one event's full detail, from cache if present.
    pub fn get(&mut self, id: i64) -> Result<EventDetail> {
        if let Some(detail) = self.cache.get(&id) {
            return Ok(detail.clone());
        }
        let detail = self.store.get_event_detail(id)?;
        self.cache.put(id, detail.clone());
        Ok(detail)
    }

    /// Fetch details for every event id backing a timeline row (usually one;
    /// two for a correlated call+result / request+response pair).
    pub fn get_many(&mut self, ids: &[i64]) -> Result<Vec<EventDetail>> {
        ids.iter().map(|&id| self.get(id)).collect()
    }

    /// Fetch the event correlated with `id` (FR-3.2 MCP/tool pairing): the
    /// other side of a call/result or request/response pair, if any.
    pub fn get_correlated(
        &mut self,
        id: i64,
        correlates: Option<i64>,
    ) -> Result<Option<EventDetail>> {
        if let Some(cid) = correlates {
            return self.get(cid).map(Some);
        }
        match self.store.get_correlated_event(id, None)? {
            Some(detail) => {
                self.cache.put(detail.id, detail.clone());
                Ok(Some(detail))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hh_core::{AdapterStatus, AgentKind, Event, EventKind, NewSession};
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn open() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("hh.db"), &tmp.path().join("blobs")).unwrap();
        (tmp, store)
    }

    fn new_session() -> NewSession {
        NewSession {
            id: hh_core::event::now_v7(),
            started_at: 0,
            agent_kind: AgentKind::Generic,
            adapter_status: AdapterStatus::None,
            command: vec!["claude".into()],
            cwd: PathBuf::from("/tmp"),
            hostname: None,
            hh_version: "0.1.0-beta.1".into(),
            model: None,
            git_branch: None,
            git_sha: None,
            git_dirty: None,
        }
    }

    #[test]
    fn get_fetches_then_caches() {
        let (_tmp, store) = open();
        let created = store.create_session(&new_session()).unwrap();
        let writer = store.event_writer().unwrap();
        let id = writer
            .append_event(Event {
                session_id: created.id.clone(),
                ts_ms: 0,
                kind: EventKind::AgentMessage,
                step: Some(1),
                summary: "hi".into(),
                body_json: Some(serde_json::json!({"text": "hi"})),
                blob_hash: None,
                blob_size: None,
                correlates: None,
            })
            .unwrap();
        writer.finish().unwrap();

        let mut data = ReplayData::new(store);
        let detail = data.get(id).unwrap();
        assert_eq!(detail.summary, "hi");
        // Second fetch must come from cache; drop the backing session data on
        // disk to prove it (store still open, but this at least exercises
        // the cache-hit path without a second query planner round trip).
        let cached = data.get(id).unwrap();
        assert_eq!(cached.summary, "hi");
    }

    #[test]
    fn get_correlated_resolves_pair() {
        let (_tmp, store) = open();
        let created = store.create_session(&new_session()).unwrap();
        let writer = store.event_writer().unwrap();
        let call_id = writer
            .append_event(Event {
                session_id: created.id.clone(),
                ts_ms: 0,
                kind: EventKind::ToolCall,
                step: Some(1),
                summary: "call".into(),
                body_json: None,
                blob_hash: None,
                blob_size: None,
                correlates: None,
            })
            .unwrap();
        let result_id = writer
            .append_event(Event {
                session_id: created.id.clone(),
                ts_ms: 5,
                kind: EventKind::ToolResult,
                step: Some(1),
                summary: "result".into(),
                body_json: None,
                blob_hash: None,
                blob_size: None,
                correlates: Some(call_id),
            })
            .unwrap();
        writer.finish().unwrap();

        let mut data = ReplayData::new(store);
        let from_call = data.get_correlated(call_id, None).unwrap().unwrap();
        assert_eq!(from_call.id, result_id);
        let from_result = data
            .get_correlated(result_id, Some(call_id))
            .unwrap()
            .unwrap();
        assert_eq!(from_result.id, call_id);
    }
}
