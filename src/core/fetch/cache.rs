//! A bounded, ephemeral cache of fetch results, sparing repeated upstream reads.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::Page;

/// Clear all entries when inserting beyond this count or the byte budget.
const MAX_ENTRIES: usize = 2048;

/// Recent successful pages keyed by requested URL, bounded by bytes and count.
/// Errors are never stored, so a transient failure is retried on the next call.
pub(super) struct Cache {
    ttl: Duration,
    max_bytes: usize,
    state: Mutex<State>,
}

/// Entries and their approximate retained bytes, updated under one lock.
#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    bytes: usize,
}

/// A cached page and the byte charge removed on replacement or expiry.
struct Entry {
    page: Page,
    at: Instant,
    bytes: usize,
}

/// Account for cloned string payloads and inline entry/key storage, without
/// serialization. Hash-table slack and allocator overhead are not included.
fn entry_bytes(key: &str, page: &Page) -> usize {
    [
        key,
        &page.url,
        &page.content_type,
        &page.text,
        page.final_url.as_deref().unwrap_or_default(),
        page.title.as_deref().unwrap_or_default(),
        page.byline.as_deref().unwrap_or_default(),
        page.published.as_deref().unwrap_or_default(),
        page.site.as_deref().unwrap_or_default(),
        page.redirect.as_deref().unwrap_or_default(),
        page.error.as_deref().unwrap_or_default(),
    ]
    .iter()
    .fold(std::mem::size_of::<(String, Entry)>(), |bytes, text| {
        bytes.saturating_add(text.len())
    })
    .saturating_add(page.links.iter().fold(0usize, |bytes, link| {
        bytes
            .saturating_add(std::mem::size_of::<String>())
            .saturating_add(link.len())
    }))
}

impl Cache {
    /// Zero TTL or byte budget disables storage.
    pub(super) fn new(ttl: Duration, max_bytes: usize) -> Self {
        Self {
            ttl,
            max_bytes,
            state: Mutex::new(State::default()),
        }
    }

    /// Clone a fresh page; remove expired entries and their byte charges.
    pub(super) fn get(&self, key: &str) -> Option<Page> {
        if self.ttl.is_zero() || self.max_bytes == 0 {
            return None;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = state.entries.get(key) {
            if entry.at.elapsed() <= self.ttl {
                return Some(entry.page.clone());
            }
        }
        if let Some(entry) = state.entries.remove(key) {
            state.bytes -= entry.bytes;
        }
        None
    }

    /// Store a fitting page, clearing all entries if either bound would be exceeded.
    /// Replacement removes the previous charge even when the new page is oversized.
    pub(super) fn put(&self, key: &str, page: &Page) {
        if self.ttl.is_zero() || self.max_bytes == 0 {
            return;
        }
        let bytes = entry_bytes(key, page);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = state.entries.remove(key) {
            state.bytes -= entry.bytes;
        }
        if bytes > self.max_bytes {
            return;
        }
        if state.entries.len() >= MAX_ENTRIES || state.bytes > self.max_bytes - bytes {
            state.entries.clear();
            state.bytes = 0;
        }
        state.entries.insert(
            key.to_string(),
            Entry {
                page: page.clone(),
                at: Instant::now(),
                bytes,
            },
        );
        state.bytes += bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetched() -> Page {
        Page {
            url: "https://example.com".into(),
            fetched_at: Some(1_700_000_000),
            final_url: None,
            status: 200,
            content_type: "text/html".into(),
            title: None,
            byline: None,
            published: None,
            site: None,
            text: "body".into(),
            links: Vec::new(),
            truncated: None,
            redirect: None,
            error: None,
        }
    }

    #[test]
    fn expiry_removes_the_byte_charge() {
        let page = fetched();
        let cache = Cache::new(Duration::from_secs(60), entry_bytes("k", &page));
        cache.put("k", &page);
        assert_eq!(
            cache.get("k").and_then(|page| page.fetched_at),
            page.fetched_at
        );
        let mut state = cache.state.lock().unwrap();
        assert_eq!(state.bytes, cache.max_bytes);
        state.entries.get_mut("k").unwrap().at = Instant::now() - Duration::from_secs(61);
        drop(state);
        assert!(cache.get("k").is_none());
        assert_eq!(cache.state.lock().unwrap().bytes, 0);
        cache.put("j", &page);
        assert!(cache.get("j").is_some());
    }

    #[test]
    fn zero_ttl_or_byte_budget_disables_storage() {
        for (ttl, bytes) in [(Duration::ZERO, 4096), (Duration::from_secs(60), 0)] {
            let cache = Cache::new(ttl, bytes);
            cache.put("k", &fetched());
            assert!(cache.get("k").is_none());
            let state = cache.state.lock().unwrap();
            assert!(state.entries.is_empty());
            assert_eq!(state.bytes, 0);
        }
    }

    #[test]
    fn byte_cap_clears_all_only_when_exceeded() {
        let page = fetched();
        let charge = entry_bytes("a", &page);
        let cache = Cache::new(Duration::from_secs(60), charge * 2);
        cache.put("a", &page);
        cache.put("b", &page);
        assert_eq!(cache.state.lock().unwrap().bytes, charge * 2);
        assert!(cache.get("a").is_some());
        assert!(cache.get("b").is_some());
        cache.put("c", &page);
        assert!(cache.get("a").is_none());
        assert!(cache.get("b").is_none());
        assert!(cache.get("c").is_some());
        assert_eq!(cache.state.lock().unwrap().bytes, charge);
    }

    #[test]
    fn replacement_subtracts_the_old_charge() {
        let page = fetched();
        let mut larger = page.clone();
        larger.text.push_str(" more text");
        let cache = Cache::new(Duration::from_secs(60), entry_bytes("a", &larger) * 2);
        cache.put("a", &page);
        cache.put("b", &page);
        for replacement in [&larger, &page] {
            cache.put("a", replacement);
            assert!(cache.get("b").is_some());
            assert_eq!(cache.get("a").unwrap().text, replacement.text);
            assert_eq!(
                cache.state.lock().unwrap().bytes,
                entry_bytes("a", replacement) + entry_bytes("b", &page)
            );
        }
    }

    #[test]
    fn oversized_payloads_skip_storage_without_clearing_other_entries() {
        let page = fetched();
        let cache = Cache::new(Duration::from_secs(60), entry_bytes("a", &page));
        cache.put("a", &page);
        let mut oversized = page.clone();
        oversized.text.push('!');
        cache.put("b", &oversized);
        assert!(cache.get("b").is_none());
        assert!(cache.get("a").is_some());
        cache.put("a", &oversized);
        assert!(cache.get("a").is_none());
        assert_eq!(cache.state.lock().unwrap().bytes, 0);
        for oversized in [
            Page {
                title: Some("metadata".into()),
                ..page.clone()
            },
            Page {
                url: format!("{}extra", page.url),
                ..page.clone()
            },
        ] {
            cache.put("a", &oversized);
            assert!(cache.get("a").is_none());
        }
        cache.put("long-key", &page);
        assert!(cache.get("long-key").is_none());
        assert_eq!(cache.state.lock().unwrap().bytes, 0);
    }

    #[test]
    fn entry_cap_clears_all_even_with_a_large_byte_budget() {
        let page = fetched();
        let cache = Cache::new(Duration::from_secs(60), usize::MAX);
        for key in 0..MAX_ENTRIES {
            cache.put(&key.to_string(), &page);
        }
        cache.put("0", &page);
        assert_eq!(cache.state.lock().unwrap().entries.len(), MAX_ENTRIES);
        cache.put("overflow", &page);
        let state = cache.state.lock().unwrap();
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.bytes, entry_bytes("overflow", &page));
    }
}
