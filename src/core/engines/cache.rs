//! Bounded transient successful answers, keyed by the entire request.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::core::Answer;

/// FIFO eviction by serialized payload plus key bytes; expired entries never return.
#[derive(Default)]
pub(super) struct Cache {
    entries: VecDeque<Entry>,
    bytes: usize,
}

struct Entry {
    key: String,
    answer: Answer,
    inserted: Instant,
    bytes: usize,
}

impl Cache {
    /// Remove expired entries before every lookup or insertion.
    fn expire(&mut self, ttl: Duration) {
        while self
            .entries
            .front()
            .is_some_and(|entry| entry.inserted.elapsed() >= ttl)
        {
            if let Some(entry) = self.entries.pop_front() {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
            }
        }
    }

    /// Return an owned answer only while its TTL is live.
    pub(super) fn get(&mut self, key: &str, ttl: Duration) -> Option<Answer> {
        self.expire(ttl);
        self.entries
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.answer.clone())
    }

    /// Store only fully successful answers that fit the configured byte budget.
    pub(super) fn insert(&mut self, key: String, answer: &Answer, ttl: Duration, cap: usize) {
        self.expire(ttl);
        if ttl.is_zero()
            || cap == 0
            || answer
                .engines
                .iter()
                .any(|state| state.status != super::EngineStatus::Ok || state.error.is_some())
        {
            return;
        }
        let Ok(payload) = serde_json::to_vec(answer) else {
            return;
        };
        let bytes = payload
            .len()
            .saturating_add(key.len())
            .saturating_add(std::mem::size_of::<Entry>());
        if bytes > cap {
            return;
        }
        // Concurrent identical queries may complete twice; keep the latest result.
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            if let Some(old) = self.entries.remove(index) {
                self.bytes = self.bytes.saturating_sub(old.bytes);
            }
        }
        while self.bytes.saturating_add(bytes) > cap {
            if let Some(old) = self.entries.pop_front() {
                self.bytes = self.bytes.saturating_sub(old.bytes);
            } else {
                break;
            }
        }
        self.bytes += bytes;
        self.entries.push_back(Entry {
            key,
            answer: answer.clone(),
            inserted: Instant::now(),
            bytes,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte eviction includes keys; oversized answers and disabled caches store nothing.
    #[test]
    fn cache_evicts_to_byte_budget_and_rejects_oversized_entries() {
        let answer = Answer {
            query: "q".into(),
            results: Vec::new(),
            engines: Vec::new(),
            duration_ms: 0,
        };
        let ttl = Duration::from_secs(60);
        let size = serde_json::to_vec(&answer).unwrap().len() + 1 + std::mem::size_of::<Entry>();
        let mut cache = Cache::default();
        cache.insert("a".into(), &answer, ttl, size);
        cache.insert("b".into(), &answer, ttl, size);
        assert!(cache.get("a", ttl).is_none());
        assert!(cache.get("b", ttl).is_some());
        assert_eq!(cache.bytes, size);
        cache.insert("c".repeat(size), &answer, ttl, size);
        assert!(cache.get(&"c".repeat(size), ttl).is_none());
        assert!(cache.get("b", ttl).is_some());
        cache.insert("d".into(), &answer, Duration::ZERO, size);
        assert!(cache.get("d", ttl).is_none());
        cache.insert("e".into(), &answer, ttl, 0);
        assert!(cache.get("e", ttl).is_none());
    }
}
