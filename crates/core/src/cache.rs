//! In-memory cache of finished LSP tool responses for one project.
//!
//! The cache does not touch the filesystem or the Java process. Callers reserve a
//! single-flight slot, compute outside this lock, then publish or fail the slot.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::oneshot;

use crate::error::ServiceError;

const MAX_READY_ENTRIES: usize = 100;
const MAX_ENTRY_BYTES: usize = 256 * 1024;
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const ENTRY_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CacheKey {
    pub tool: &'static str,
    pub path: String,
    pub line: Option<u32>,
    pub character: Option<u32>,
    pub params: String,
    pub content_hash: u64,
}

impl CacheKey {
    pub(crate) fn new(
        tool: &'static str,
        path: &str,
        line: Option<u32>,
        character: Option<u32>,
        params: &str,
        content_hash: u64,
    ) -> Self {
        Self {
            tool,
            path: normalize_rel_path(path),
            line,
            character,
            params: params.to_string(),
            content_hash,
        }
    }
}

pub(crate) fn normalize_rel_path(path: &str) -> String {
    path.trim().replace('\\', "/")
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CacheStats {
    pub hit: u64,
    pub miss: u64,
    pub wait: u64,
    pub skip_not_ready: u64,
    pub skip_too_large: u64,
    pub eviction: u64,
}

#[derive(Clone)]
pub(crate) struct ResponseCache {
    inner: Arc<Mutex<CacheInner>>,
}

#[derive(Default)]
struct CacheInner {
    ready: HashMap<CacheKey, ReadyEntry>,
    inflight: HashMap<CacheKey, Flight>,
    total_bytes: usize,
    tick: u64,
    next_generation: u64,
    stats: CacheStats,
}

struct ReadyEntry {
    value: Value,
    bytes: usize,
    last_used: Instant,
    seq: u64,
    stored_at: Instant,
}

struct Flight {
    generation: u64,
    waiters: Vec<oneshot::Sender<WaitOutcome>>,
}

enum WaitOutcome {
    Ready(Value),
    Failed(String),
    Retry,
}

enum Begin {
    Hit(Value),
    Wait(oneshot::Receiver<WaitOutcome>),
    Owner(FlightOwner),
}

struct FlightOwner {
    inner: Arc<Mutex<CacheInner>>,
    key: CacheKey,
    generation: u64,
    armed: bool,
}

impl ResponseCache {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(CacheInner::default())),
        }
    }

    pub(crate) async fn get_or_compute<F, Fut>(
        &self,
        key: CacheKey,
        compute: F,
    ) -> Result<Value, ServiceError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<Value, ServiceError>>,
    {
        loop {
            match self.begin(&key) {
                Begin::Hit(value) => return Ok(value),
                Begin::Wait(rx) => match rx.await {
                    Ok(WaitOutcome::Ready(value)) => return Ok(value),
                    Ok(WaitOutcome::Failed(message)) => {
                        return Err(ServiceError::Internal(message));
                    }
                    Ok(WaitOutcome::Retry) | Err(_) => continue,
                },
                Begin::Owner(owner) => {
                    let result = compute().await;
                    owner.settle(&result);
                    return result;
                }
            }
        }
    }

    pub(crate) fn note_skip_not_ready(&self, tool: &str) {
        let mut guard = lock(&self.inner);
        guard.stats.skip_not_ready += 1;
        tracing::debug!(event = "skip_not_ready", tool, "lsp response cache");
    }

    /// Drops ready entries and wakes single-flight waiters so they can retry.
    pub(crate) fn clear(&self, reason: &'static str) {
        let mut guard = lock(&self.inner);
        let entries = guard.ready.len();
        let inflight = guard.inflight.len();
        for (_, flight) in guard.inflight.drain() {
            for waiter in flight.waiters {
                let _ = waiter.send(WaitOutcome::Retry);
            }
        }
        guard.ready.clear();
        guard.total_bytes = 0;
        if entries > 0 || inflight > 0 {
            tracing::debug!(
                event = "clear",
                reason,
                entries,
                inflight,
                "lsp response cache"
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn ready_len(&self) -> usize {
        lock(&self.inner).ready.len()
    }

    #[cfg(test)]
    pub(crate) fn inflight_len(&self) -> usize {
        lock(&self.inner).inflight.len()
    }

    #[cfg(test)]
    pub(crate) fn total_bytes(&self) -> usize {
        lock(&self.inner).total_bytes
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> CacheStats {
        lock(&self.inner).stats.clone()
    }

    #[cfg(test)]
    pub(crate) fn tool_names(&self) -> Vec<&'static str> {
        lock(&self.inner).ready.keys().map(|key| key.tool).collect()
    }

    fn begin(&self, key: &CacheKey) -> Begin {
        let mut guard = lock(&self.inner);
        let now = Instant::now();
        if guard
            .ready
            .get(key)
            .is_some_and(|entry| now.saturating_duration_since(entry.stored_at) >= ENTRY_TTL)
        {
            if let Some(entry) = guard.ready.remove(key) {
                guard.total_bytes = guard.total_bytes.saturating_sub(entry.bytes);
            }
        }

        if guard.ready.contains_key(key) {
            guard.tick += 1;
            let seq = guard.tick;
            guard.stats.hit += 1;
            let entry = guard.ready.get_mut(key).expect("entry");
            entry.last_used = now;
            entry.seq = seq;
            let value = entry.value.clone();
            tracing::debug!(
                event = "hit",
                tool = key.tool,
                path = %key.path,
                "lsp response cache"
            );
            return Begin::Hit(value);
        }

        if let Some(flight) = guard.inflight.get_mut(key) {
            let (tx, rx) = oneshot::channel();
            flight.waiters.push(tx);
            guard.stats.wait += 1;
            tracing::debug!(
                event = "wait",
                tool = key.tool,
                path = %key.path,
                "lsp response cache"
            );
            return Begin::Wait(rx);
        }

        guard.next_generation += 1;
        let generation = guard.next_generation;
        guard.inflight.insert(
            key.clone(),
            Flight {
                generation,
                waiters: Vec::new(),
            },
        );
        guard.stats.miss += 1;
        tracing::debug!(
            event = "miss",
            tool = key.tool,
            path = %key.path,
            "lsp response cache"
        );
        Begin::Owner(FlightOwner {
            inner: Arc::clone(&self.inner),
            key: key.clone(),
            generation,
            armed: true,
        })
    }
}

impl FlightOwner {
    fn settle(mut self, result: &Result<Value, ServiceError>) {
        self.publish(result);
    }

    fn publish(&mut self, result: &Result<Value, ServiceError>) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let mut guard = lock(&self.inner);
        match result {
            Ok(value) => guard.publish_ok(&self.key, self.generation, value),
            Err(err) => guard.publish_err(&self.key, self.generation, &err.to_string()),
        }
    }
}

impl Drop for FlightOwner {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let mut guard = lock(&self.inner);
        guard.publish_retry(&self.key, self.generation);
    }
}

impl CacheInner {
    fn publish_ok(&mut self, key: &CacheKey, generation: u64, value: &Value) {
        let Some(flight) = self.take_flight(key, generation) else {
            return;
        };
        self.try_store(key.clone(), value.clone());
        for waiter in flight.waiters {
            let _ = waiter.send(WaitOutcome::Ready(value.clone()));
        }
    }

    fn publish_err(&mut self, key: &CacheKey, generation: u64, message: &str) {
        let Some(flight) = self.take_flight(key, generation) else {
            return;
        };
        for waiter in flight.waiters {
            let _ = waiter.send(WaitOutcome::Failed(message.to_string()));
        }
    }

    fn publish_retry(&mut self, key: &CacheKey, generation: u64) {
        let Some(flight) = self.take_flight(key, generation) else {
            return;
        };
        for waiter in flight.waiters {
            let _ = waiter.send(WaitOutcome::Retry);
        }
    }

    fn take_flight(&mut self, key: &CacheKey, generation: u64) -> Option<Flight> {
        match self.inflight.get(key) {
            Some(flight) if flight.generation == generation => self.inflight.remove(key),
            _ => None,
        }
    }

    fn try_store(&mut self, key: CacheKey, value: Value) {
        let bytes = match serde_json::to_vec(&value) {
            Ok(raw) => raw.len(),
            Err(_) => {
                self.note_too_large(&key, 0);
                return;
            }
        };
        if bytes > MAX_ENTRY_BYTES {
            self.note_too_large(&key, bytes);
            return;
        }
        self.evict_until_fits(bytes);
        self.tick += 1;
        let now = Instant::now();
        self.total_bytes += bytes;
        self.ready.insert(
            key,
            ReadyEntry {
                value,
                bytes,
                last_used: now,
                seq: self.tick,
                stored_at: now,
            },
        );
    }

    fn note_too_large(&mut self, key: &CacheKey, bytes: usize) {
        self.stats.skip_too_large += 1;
        tracing::debug!(
            event = "skip_too_large",
            tool = key.tool,
            path = %key.path,
            bytes,
            "lsp response cache"
        );
    }

    fn evict_until_fits(&mut self, incoming: usize) {
        while !self.ready.is_empty()
            && (self.ready.len() >= MAX_READY_ENTRIES
                || self.total_bytes + incoming > MAX_TOTAL_BYTES)
        {
            self.evict_one();
        }
    }

    fn evict_one(&mut self) {
        let Some(victim) = self
            .ready
            .iter()
            .min_by_key(|(_, entry)| (entry.last_used, entry.seq))
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        if let Some(entry) = self.ready.remove(&victim) {
            self.total_bytes = self.total_bytes.saturating_sub(entry.bytes);
            self.stats.eviction += 1;
            tracing::debug!(
                event = "eviction",
                tool = victim.tool,
                path = %victim.path,
                "lsp response cache"
            );
        }
    }
}

fn lock(inner: &Mutex<CacheInner>) -> MutexGuard<'_, CacheInner> {
    inner.lock().unwrap_or_else(|err| err.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn key(tool: &'static str, path: &str) -> CacheKey {
        CacheKey::new(tool, path, None, None, "", 1)
    }

    fn value_of_len(chars: usize) -> Value {
        Value::String("x".repeat(chars))
    }

    #[test]
    fn normalizes_path_separators() {
        assert_eq!(
            CacheKey::new("symbols", r"src\mod.bsl", None, None, "", 7),
            CacheKey::new("symbols", "src/mod.bsl", None, None, "", 7)
        );
    }

    #[tokio::test]
    async fn hit_miss_and_lru_touch() {
        let cache = ResponseCache::new();
        let calls = AtomicUsize::new(0);
        let compute = || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(json!({"n": 1})) }
        };

        let first = cache
            .get_or_compute(key("symbols", "a.bsl"), compute)
            .await
            .unwrap();
        let second = cache
            .get_or_compute(key("symbols", "a.bsl"), compute)
            .await
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(cache.stats().miss, 1);
        assert_eq!(cache.stats().hit, 1);

        for index in 0..(MAX_READY_ENTRIES - 1) {
            let path = format!("f{index}.bsl");
            cache
                .get_or_compute(key("symbols", &path), || async { Ok(json!(index)) })
                .await
                .unwrap();
        }
        assert_eq!(cache.ready_len(), MAX_READY_ENTRIES);
        let touched = cache
            .get_or_compute(key("symbols", "a.bsl"), || async {
                panic!("touched entry must stay cached")
            })
            .await
            .unwrap();
        assert_eq!(touched, json!({"n": 1}));
        cache
            .get_or_compute(key("symbols", "overflow.bsl"), || async {
                Ok(json!("new"))
            })
            .await
            .unwrap();

        assert_eq!(cache.ready_len(), MAX_READY_ENTRIES);
        assert!(cache.stats().eviction >= 1);
        let again = cache
            .get_or_compute(key("symbols", "a.bsl"), || async {
                panic!("touched entry must stay cached")
            })
            .await
            .unwrap();
        assert_eq!(again, json!({"n": 1}));
        let oldest = cache
            .get_or_compute(key("symbols", "f0.bsl"), || async {
                Ok(json!("recomputed"))
            })
            .await
            .unwrap();
        assert_eq!(oldest, json!("recomputed"));
    }

    #[tokio::test]
    async fn expired_entry_is_not_returned() {
        let cache = ResponseCache::new();
        cache
            .get_or_compute(key("diagnostics", "a.bsl"), || async { Ok(json!(1)) })
            .await
            .unwrap();
        {
            let mut guard = lock(&cache.inner);
            let entry = guard.ready.values_mut().next().unwrap();
            entry.stored_at = Instant::now()
                .checked_sub(ENTRY_TTL + Duration::from_secs(1))
                .expect("clock");
        }
        let calls = AtomicUsize::new(0);
        let value = cache
            .get_or_compute(key("diagnostics", "a.bsl"), || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(json!(2)) }
            })
            .await
            .unwrap();
        assert_eq!(value, json!(2));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(cache.ready_len(), 1);
    }

    #[tokio::test]
    async fn entry_count_evicts_least_recently_used() {
        let cache = ResponseCache::new();
        for index in 0..MAX_READY_ENTRIES {
            cache
                .get_or_compute(key("references", &format!("{index}.bsl")), || async {
                    Ok(json!(index))
                })
                .await
                .unwrap();
        }
        cache
            .get_or_compute(key("references", "extra.bsl"), || async {
                Ok(json!("extra"))
            })
            .await
            .unwrap();
        assert_eq!(cache.ready_len(), MAX_READY_ENTRIES);
        assert!(cache.stats().eviction >= 1);
        let recomputed = cache
            .get_or_compute(key("references", "0.bsl"), || async { Ok(json!("gone")) })
            .await
            .unwrap();
        assert_eq!(recomputed, json!("gone"));
    }

    #[tokio::test]
    async fn total_budget_evicts_least_recently_used() {
        let cache = ResponseCache::new();
        let chunk = value_of_len(200_000);
        let chunk_bytes = serde_json::to_vec(&chunk).unwrap().len();
        assert!(chunk_bytes <= MAX_ENTRY_BYTES);
        let mut stored = 0usize;
        while cache.total_bytes() + chunk_bytes <= MAX_TOTAL_BYTES {
            let path = format!("b{stored}.bsl");
            cache
                .get_or_compute(key("call_graph", &path), || {
                    let chunk = chunk.clone();
                    async move { Ok(chunk) }
                })
                .await
                .unwrap();
            stored += 1;
            assert!(stored < MAX_READY_ENTRIES);
        }
        cache
            .get_or_compute(key("call_graph", "budget-extra.bsl"), || {
                let chunk = chunk.clone();
                async move { Ok(chunk) }
            })
            .await
            .unwrap();
        assert!(cache.total_bytes() <= MAX_TOTAL_BYTES);
        assert!(cache.stats().eviction >= 1);
        assert!(cache.ready_len() < stored + 1);
    }

    #[tokio::test]
    async fn oversized_response_is_not_stored_but_waiters_receive_it() {
        let cache = ResponseCache::new();
        let huge = value_of_len(MAX_ENTRY_BYTES);
        assert!(serde_json::to_vec(&huge).unwrap().len() > MAX_ENTRY_BYTES);
        let cache_key = key("workspace_symbols", "");

        let entered = Arc::new(tokio::sync::Notify::new());
        let go = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let owner = tokio::spawn({
            let cache = cache.clone();
            let entered = Arc::clone(&entered);
            let go = Arc::clone(&go);
            let huge = huge.clone();
            let cache_key = cache_key.clone();
            async move {
                cache
                    .get_or_compute(cache_key, || {
                        let entered = Arc::clone(&entered);
                        let go = Arc::clone(&go);
                        let huge = huge.clone();
                        async move {
                            entered.notify_one();
                            while !go.load(Ordering::SeqCst) {
                                tokio::task::yield_now().await;
                            }
                            Ok(huge)
                        }
                    })
                    .await
                    .unwrap()
            }
        });
        entered.notified().await;
        let waiter = tokio::spawn({
            let cache = cache.clone();
            let cache_key = cache_key.clone();
            async move {
                cache
                    .get_or_compute(cache_key, || async { panic!("waiter must not compute") })
                    .await
                    .unwrap()
            }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while cache.stats().wait < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("waiter did not join");
        go.store(true, Ordering::SeqCst);
        let owner_value = owner.await.unwrap();
        let waiter_value = waiter.await.unwrap();
        assert_eq!(owner_value, huge);
        assert_eq!(waiter_value, huge);
        assert_eq!(cache.ready_len(), 0);
        assert_eq!(cache.inflight_len(), 0);
        assert!(cache.stats().skip_too_large >= 1);
    }

    #[tokio::test]
    async fn parallel_same_key_computes_once() {
        let cache = ResponseCache::new();
        let cache_key = key("diagnostics", "same.bsl");
        let computations = Arc::new(AtomicUsize::new(0));
        let go = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            let cache_key = cache_key.clone();
            let computations = Arc::clone(&computations);
            let go = Arc::clone(&go);
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_compute(cache_key, || {
                        let computations = Arc::clone(&computations);
                        let go = Arc::clone(&go);
                        async move {
                            computations.fetch_add(1, Ordering::SeqCst);
                            while !go.load(Ordering::SeqCst) {
                                tokio::task::yield_now().await;
                            }
                            Ok(json!({"shared": true}))
                        }
                    })
                    .await
                    .unwrap()
            }));
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while cache.stats().wait < 7 || computations.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("single-flight did not form");
        go.store(true, Ordering::SeqCst);
        for task in tasks {
            assert_eq!(task.await.unwrap(), json!({"shared": true}));
        }
        assert_eq!(computations.load(Ordering::SeqCst), 1);
        assert_eq!(cache.ready_len(), 1);
        assert_eq!(cache.inflight_len(), 0);
        assert_eq!(cache.stats().miss, 1);
        assert!(cache.stats().wait >= 7);
    }

    #[tokio::test]
    async fn owner_error_wakes_waiters_and_is_not_cached() {
        let cache = ResponseCache::new();
        let cache_key = key("definition", "a.bsl");
        let entered = Arc::new(tokio::sync::Notify::new());
        let go = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let owner = tokio::spawn({
            let cache = cache.clone();
            let entered = Arc::clone(&entered);
            let go = Arc::clone(&go);
            let cache_key = cache_key.clone();
            async move {
                cache
                    .get_or_compute(cache_key, || {
                        let entered = Arc::clone(&entered);
                        let go = Arc::clone(&go);
                        async move {
                            entered.notify_one();
                            while !go.load(Ordering::SeqCst) {
                                tokio::task::yield_now().await;
                            }
                            Err(ServiceError::Internal("boom".to_string()))
                        }
                    })
                    .await
            }
        });
        entered.notified().await;
        let waiter = tokio::spawn({
            let cache = cache.clone();
            let cache_key = cache_key.clone();
            async move {
                cache
                    .get_or_compute(cache_key, || async {
                        panic!("waiter must observe the owner error")
                    })
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while cache.stats().wait < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("waiter did not join");
        go.store(true, Ordering::SeqCst);
        let owner_err = owner.await.unwrap().unwrap_err().to_string();
        let waiter_err = waiter.await.unwrap().unwrap_err().to_string();
        assert!(owner_err.contains("boom"));
        assert!(waiter_err.contains("boom"));
        assert_eq!(cache.ready_len(), 0);
        assert_eq!(cache.inflight_len(), 0);

        let calls = AtomicUsize::new(0);
        cache
            .get_or_compute(cache_key, || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(json!(1)) }
            })
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn owner_cancel_wakes_waiters_without_leaving_inflight() {
        let cache = ResponseCache::new();
        let cache_key = key("outgoing_calls", "a.bsl");
        let entered = Arc::new(tokio::sync::Notify::new());
        let computations = Arc::new(AtomicUsize::new(0));
        let owner = tokio::spawn({
            let cache = cache.clone();
            let entered = Arc::clone(&entered);
            let computations = Arc::clone(&computations);
            let cache_key = cache_key.clone();
            async move {
                cache
                    .get_or_compute(cache_key, || {
                        let entered = Arc::clone(&entered);
                        let computations = Arc::clone(&computations);
                        async move {
                            computations.fetch_add(1, Ordering::SeqCst);
                            entered.notify_one();
                            std::future::pending::<()>().await;
                            Ok(json!("owner"))
                        }
                    })
                    .await
            }
        });
        entered.notified().await;
        let waiter = tokio::spawn({
            let cache = cache.clone();
            let computations = Arc::clone(&computations);
            let cache_key = cache_key.clone();
            async move {
                cache
                    .get_or_compute(cache_key, || {
                        let computations = Arc::clone(&computations);
                        async move {
                            computations.fetch_add(1, Ordering::SeqCst);
                            Ok(json!("waiter"))
                        }
                    })
                    .await
                    .unwrap()
            }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while cache.stats().wait < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("waiter did not join");
        owner.abort();
        let _ = owner.await;
        let value = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("waiter hung after owner cancel")
            .unwrap();
        assert_eq!(value, json!("waiter"));
        assert_eq!(cache.inflight_len(), 0);
        assert_eq!(computations.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn clear_drops_ready_entries_and_wakes_waiters() {
        let cache = ResponseCache::new();
        cache
            .get_or_compute(key("complexity", "a.bsl"), || async { Ok(json!(1)) })
            .await
            .unwrap();
        let cache_key = key("complexity", "b.bsl");
        let owner = match cache.begin(&cache_key) {
            Begin::Owner(owner) => owner,
            _ => panic!("expected owner"),
        };
        let rx = match cache.begin(&cache_key) {
            Begin::Wait(rx) => rx,
            _ => panic!("expected waiter"),
        };
        cache.clear("test");
        assert!(matches!(rx.await.unwrap(), WaitOutcome::Retry));
        owner.settle(&Ok(json!({"stale": true})));
        assert_eq!(cache.ready_len(), 0);
        assert_eq!(cache.inflight_len(), 0);
        assert_eq!(cache.total_bytes(), 0);
    }
}
