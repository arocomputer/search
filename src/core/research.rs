//! Deterministic, bounded evidence assembly over live discovery and guarded
//! fetching. Core returns provenance and incomplete coverage, never synthesis.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::core::config::ResearchSettings;
use crate::core::engines::{fuse_answers, EngineState, EngineStatus};
use crate::core::service::{Answer, Link, Query, QueryFilters, Search};
use crate::core::text::{select, Passage};

/// Absolute request safety ceilings, independent of operator defaults.
pub const MAX_SOURCES: usize = 10;
pub const MAX_SOURCE_CHARACTERS: usize = 50_000;
pub const MAX_TOTAL_CHARACTERS: usize = 200_000;
pub const MAX_PER_HOST: usize = 10;
pub const MAX_ATTEMPTS: usize = 100;

/// Bounds on one research call. Zero retains the operator's configured default.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResearchOptions {
    pub sources: usize,
    pub source_characters: usize,
    pub total_characters: usize,
    pub max_per_host: usize,
    pub engines: Vec<String>,
    pub filters: QueryFilters,
    pub max_attempts: usize,
    /// Research-wide deadline in milliseconds; clamped to operator timeout.
    pub timeout: u64,
    /// Ephemeral session key, namespaced by the authenticated calling surface.
    pub session: Option<String>,
}

impl ResearchOptions {
    /// Clamp safety ceilings without resolving zeros before reaching core.
    pub fn resolved(mut self) -> Self {
        self.sources = self.sources.min(MAX_SOURCES);
        self.source_characters = self.source_characters.min(MAX_SOURCE_CHARACTERS);
        self.total_characters = self.total_characters.min(MAX_TOTAL_CHARACTERS);
        self.max_per_host = self.max_per_host.min(MAX_PER_HOST);
        self.max_attempts = self.max_attempts.min(MAX_ATTEMPTS);
        self
    }

    /// Resolve operator defaults and work ceilings at the executing service.
    pub(crate) fn configured(mut self, settings: &ResearchSettings) -> Self {
        for (value, default) in [
            (&mut self.sources, settings.sources),
            (&mut self.source_characters, settings.source_characters),
            (&mut self.total_characters, settings.total_characters),
            (&mut self.max_per_host, settings.max_per_host),
            (&mut self.max_attempts, settings.max_attempts),
        ] {
            *value = if *value == 0 {
                default
            } else {
                (*value).min(default)
            };
        }
        self.timeout = if self.timeout == 0 {
            settings.timeout
        } else {
            self.timeout.min(settings.timeout)
        };
        self.resolved()
    }
}

/// A fetched source, including download limits and character ranges. Failed
/// attempts remain here with their errors but do not consume useful-source slots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<i64>,
    pub engines: Vec<String>,
    pub passages: Vec<Passage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// True only when the download itself was cut, not passage selection.
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub returned_characters: usize,
    /// Available clean characters in this download; not an estimate of a cut body.
    #[serde(default)]
    pub available_characters: usize,
    /// Clipped passages carry their own continuation. This resumes a plain read
    /// after the last returned passage when more downloaded text follows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Visited candidates partition into read, failed, skipped, duplicate and
/// previously_seen. Attempted excludes identities rejected before downloading;
/// unvisited candidates were never considered for fetching.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Coverage {
    pub candidates: usize,
    /// Compatibility name for attempted, not a preselected batch.
    pub selected: usize,
    pub attempted: usize,
    /// Successful downloads, including duplicates and unusable text.
    pub fetched: usize,
    /// Useful distinct sources actually returned.
    pub read: usize,
    pub failed: usize,
    /// Downloads excluded by final-host diversity; unusable text counts as failed.
    pub skipped: usize,
    pub duplicate: usize,
    pub previously_seen: usize,
    /// Candidates never visited because a budget, target or deadline stopped work.
    pub unvisited: usize,
    /// Returned sources with omitted text or a truncated download.
    pub incomplete: usize,
}

/// An auditable evidence packet. Edge processors may attach optional output;
/// core always leaves processing fields empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Packet {
    pub queries: Vec<String>,
    pub sources: Vec<Evidence>,
    pub engines: Vec<EngineState>,
    pub coverage: Coverage,
    pub characters: usize,
    pub duration_ms: u64,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub incomplete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesis: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processing_errors: Vec<String>,
}

/// Only useful evidence is remembered. Both sessions and each session's FIFO
/// identities are bounded; no downloaded text or persistent corpus is retained.
#[derive(Default)]
pub(super) struct SessionStore {
    entries: HashMap<String, Session>,
}

#[derive(Clone)]
struct Session {
    touched: Instant,
    seen: Vec<(String, String, String)>,
}

impl SessionStore {
    /// Expire idle state and snapshot the caller's identities without holding a
    /// lock across network work. Concurrent requests merge their useful evidence.
    fn snapshot(&mut self, key: Option<&str>, settings: &ResearchSettings) -> Session {
        let now = Instant::now();
        self.entries.retain(|_, entry| {
            now.duration_since(entry.touched) < Duration::from_millis(settings.session_ttl)
        });
        let mut session = key
            .and_then(|key| self.entries.get(key))
            .cloned()
            .unwrap_or(Session {
                touched: now,
                seen: Vec::new(),
            });
        session.touched = now;
        if let Some(entry) = key.and_then(|key| self.entries.get_mut(key)) {
            entry.touched = now;
        }
        session
    }

    /// Merge identities under the configured TTL and entry bound. Oldest
    /// identities and least recently touched sessions are evicted first.
    fn commit(
        &mut self,
        key: &str,
        seen: Vec<(String, String, String)>,
        settings: &ResearchSettings,
    ) {
        if settings.session_entries == 0 || settings.session_ttl == 0 || seen.is_empty() {
            return;
        }
        self.snapshot(Some(key), settings);
        if !self.entries.contains_key(key) && self.entries.len() >= settings.session_entries {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        let entry = self.entries.entry(key.to_owned()).or_insert(Session {
            touched: Instant::now(),
            seen: Vec::new(),
        });
        entry.touched = Instant::now();
        for identity in seen {
            if !entry.seen.contains(&identity) {
                entry.seen.push(identity);
            }
        }
        let excess = entry.seen.len().saturating_sub(settings.session_entries);
        entry.seen.drain(..excess);
    }
}

impl Search {
    /// Fan out distinct queries under one deadline, then fetch replacements in fused
    /// order until useful evidence, character, attempt or time bounds are met.
    pub async fn research(&self, queries: Vec<String>, options: ResearchOptions) -> Packet {
        let settings = &self.config().research;
        let options = options.configured(settings);
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(options.timeout);
        let mut pending = FuturesUnordered::new();
        let mut distinct_queries = HashSet::new();
        for (index, text) in queries.iter().enumerate() {
            if !distinct_queries.insert(text) {
                continue;
            }
            let query = Query {
                text: text.clone(),
                limit: options.max_attempts,
                engines: options.engines.clone(),
                filters: options.filters.clone(),
            };
            pending.push(async move { (index, self.pool.search(query).await) });
        }
        let mut answers = Vec::new();
        let mut timed_out = false;
        while !pending.is_empty() {
            match tokio::time::timeout_at(deadline, pending.next()).await {
                Ok(Some(answer)) => answers.push(answer),
                Ok(None) => break,
                Err(_) => {
                    timed_out = true;
                    break;
                }
            }
        }
        drop(pending);
        answers.sort_by_key(|(index, _)| *index);
        let answers: Vec<Answer> = answers.into_iter().map(|(_, answer)| answer).collect();
        let candidates = fuse_answers(&answers);
        let mut engines = merge_engine_states(&answers);
        if timed_out {
            for name in self
                .pool
                .names()
                .into_iter()
                .filter(|name| options.engines.is_empty() || options.engines.contains(name))
            {
                if let Some(state) = engines.iter_mut().find(|state| state.name == name) {
                    state.status = EngineStatus::Timeout;
                    let error = state.error.get_or_insert_with(String::new);
                    if !error.is_empty() {
                        error.push_str("; ");
                    }
                    error.push_str("research deadline exceeded during discovery");
                } else {
                    engines.push(EngineState {
                        name,
                        status: EngineStatus::Timeout,
                        count: 0,
                        skipped: 0,
                        elapsed_ms: options.timeout,
                        error: Some("research deadline exceeded during discovery".into()),
                        unsupported_filters: Vec::new(),
                    });
                }
            }
            engines.sort_by(|a, b| a.name.cmp(&b.name));
        }
        let previous = match tokio::time::timeout_at(deadline, self.research_sessions.lock()).await
        {
            Ok(mut sessions) => sessions.snapshot(options.session.as_deref(), settings),
            Err(_) => {
                timed_out = true;
                Session {
                    touched: Instant::now(),
                    seen: Vec::new(),
                }
            }
        };
        let mut assembly = Assembly::new(options, queries.join(" "), candidates.len(), previous);
        for link in &candidates {
            if assembly.finished() || timed_out {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            if !assembly.visit(link) {
                continue;
            }
            assembly.coverage.attempted += 1;
            match tokio::time::timeout_at(deadline, self.fetcher.fetch(&link.url)).await {
                Ok(Ok(page)) => {
                    let source = link.clone();
                    if let Err(error) = assembly
                        .work(&self.fetcher, deadline, move |state| {
                            state.accept(&source, page)
                        })
                        .await
                    {
                        assembly.coverage.fetched += 1;
                        timed_out |= tokio::time::Instant::now() >= deadline;
                        assembly.failure(link, error.into());
                        if timed_out {
                            break;
                        }
                    }
                }
                Ok(Err(error)) => assembly.failure(link, error.message()),
                Err(_) => {
                    timed_out = true;
                    assembly.failure(link, "research deadline exceeded during fetch".into());
                    break;
                }
            }
        }
        if let Some(key) = assembly.options.session.as_deref() {
            // Commit only identities present in the returned partial packet;
            // abandoned workers own a clone and cannot mutate session state.
            match tokio::time::timeout_at(deadline, self.research_sessions.lock()).await {
                Ok(mut sessions) => sessions.commit(key, assembly.remembered.clone(), settings),
                Err(_) => timed_out = true,
            }
        }
        assembly.coverage.selected = assembly.coverage.attempted;
        assembly.coverage.unvisited = candidates.len().saturating_sub(assembly.visited);
        timed_out |= tokio::time::Instant::now() >= deadline;
        let incomplete = timed_out
            || assembly.coverage.read < assembly.options.sources
            || assembly.coverage.incomplete > 0
            || assembly.coverage.failed > 0
            || assembly.coverage.skipped > 0
            || assembly.coverage.unvisited > 0
            || engines.iter().any(|state| {
                state.status != EngineStatus::Ok
                    || state.skipped > 0
                    || !state.unsupported_filters.is_empty()
            });
        Packet {
            queries,
            sources: assembly.sources,
            engines,
            coverage: assembly.coverage,
            characters: assembly.characters,
            duration_ms: started.elapsed().as_millis() as u64,
            timed_out,
            incomplete,
            synthesis: None,
            processing_errors: Vec::new(),
        }
    }
}

/// Pure assembly state makes useful-source replacement and budget accounting
/// testable without a network. Diversity tracks final hosts of useful evidence.
#[derive(Clone)]
struct Assembly {
    options: ResearchOptions,
    intent: String,
    previous: Session,
    coverage: Coverage,
    visited: usize,
    characters: usize,
    sources: Vec<Evidence>,
    hosts: HashMap<String, usize>,
    urls: HashSet<String>,
    hashes: HashSet<String>,
    remembered: Vec<(String, String, String)>,
}

impl Assembly {
    /// Start a call with a session snapshot; failed attempts never modify it.
    fn new(options: ResearchOptions, intent: String, candidates: usize, previous: Session) -> Self {
        Self {
            options,
            intent,
            previous,
            coverage: Coverage {
                candidates,
                ..Coverage::default()
            },
            visited: 0,
            characters: 0,
            sources: Vec::new(),
            hosts: HashMap::new(),
            urls: HashSet::new(),
            hashes: HashSet::new(),
            remembered: Vec::new(),
        }
    }

    /// Run expensive hashing and selection on a bounded blocking worker. The
    /// fetcher retains its concurrency permit even if the deadline abandons it;
    /// only a timely completed clone can replace the caller's partial evidence.
    async fn work<F>(
        &mut self,
        fetcher: &crate::core::fetch::Fetcher,
        deadline: tokio::time::Instant,
        work: F,
    ) -> Result<(), &'static str>
    where
        F: FnOnce(&mut Self) + Send + 'static,
    {
        if tokio::time::Instant::now() >= deadline {
            return Err("research deadline exceeded during assembly");
        }
        let mut next = self.clone();
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("research deadline exceeded during assembly");
        }
        let result = tokio::time::timeout_at(
            deadline,
            fetcher.crawl_work(remaining, move || {
                work(&mut next);
                next
            }),
        )
        .await;
        if tokio::time::Instant::now() >= deadline {
            return Err("research deadline exceeded during assembly");
        }
        match result {
            Ok(Ok(next)) => {
                *self = next;
                Ok(())
            }
            _ => Err("research assembly worker failed"),
        }
    }

    /// A spent character budget stops before another request is issued.
    fn finished(&self) -> bool {
        self.characters >= self.options.total_characters
            || self.coverage.read >= self.options.sources
            || self.coverage.attempted >= self.options.max_attempts
    }

    /// Exclude known identities before fetching. Host diversity is checked only
    /// after redirects resolve, so an origin cannot hide a useful destination.
    fn visit(&mut self, link: &Link) -> bool {
        self.visited += 1;
        let url = identity_url(&link.url);
        if self
            .previous
            .seen
            .iter()
            .any(|(a, b, _)| a == &url || b == &url)
        {
            self.coverage.previously_seen += 1;
            return false;
        }
        if self.urls.contains(&url) {
            self.coverage.duplicate += 1;
            return false;
        }
        true
    }

    /// Keep failed-source provenance while leaving useful-source slots free.
    fn failure(&mut self, link: &Link, error: String) {
        self.coverage.failed += 1;
        self.sources.push(Evidence {
            url: link.url.clone(),
            final_url: None,
            title: Some(link.title.clone()).filter(|title| !title.is_empty()),
            published: link.published.clone(),
            updated: link.updated.clone(),
            fetched_at: None,
            engines: link.engines.clone(),
            passages: Vec::new(),
            content_hash: None,
            truncated: false,
            returned_characters: 0,
            available_characters: 0,
            next_offset: None,
            error: Some(error),
        });
    }

    /// Accept useful distinct content under both character bounds, preserving
    /// partial downloads and page dates. Hashes cover clean text before selection.
    /// Production callers must run this CPU work through `Assembly::work`.
    fn accept(&mut self, link: &Link, page: crate::core::fetch::Page) {
        self.coverage.fetched += 1;
        if page.text.trim().is_empty() {
            self.failure(
                link,
                page.error
                    .unwrap_or_else(|| "no useful text in response".into()),
            );
            return;
        }
        let original = identity_url(&link.url);
        let final_url = identity_url(page.final_url.as_deref().unwrap_or(&page.url));
        let hash = Sha256::digest(page.text.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if self
            .previous
            .seen
            .iter()
            .any(|(a, b, h)| a == &final_url || b == &final_url || h == &hash)
        {
            self.coverage.previously_seen += 1;
            return;
        }
        if self.urls.contains(&final_url) || self.hashes.contains(&hash) {
            self.coverage.duplicate += 1;
            return;
        }
        let host = host(&final_url);
        if self.hosts.get(&host).copied().unwrap_or(0) >= self.options.max_per_host {
            self.coverage.skipped += 1;
            return;
        }
        let available_characters = page.text.chars().count();
        let passages = select(
            &page.text,
            &self.intent,
            self.options.source_characters.min(
                self.options
                    .total_characters
                    .saturating_sub(self.characters),
            ),
        );
        let returned_characters = passages
            .iter()
            .map(|p| p.text.chars().count())
            .sum::<usize>();
        if returned_characters == 0 {
            self.failure(link, "no useful text in response".into());
            return;
        }
        let next_offset = passages
            .last()
            .map(|p| p.end_offset)
            .filter(|end| *end < available_characters);
        let truncated = page.truncated == Some(true);
        if truncated || page.error.is_some() || returned_characters < available_characters {
            self.coverage.incomplete += 1;
        }
        self.coverage.read += 1;
        self.characters += returned_characters;
        *self.hosts.entry(host).or_default() += 1;
        self.urls.insert(original.clone());
        self.urls.insert(final_url.clone());
        self.hashes.insert(hash.clone());
        self.remembered.push((original, final_url, hash.clone()));
        self.sources.push(Evidence {
            url: link.url.clone(),
            final_url: page.final_url,
            title: page
                .title
                .filter(|title| !title.is_empty())
                .or_else(|| Some(link.title.clone()).filter(|title| !title.is_empty())),
            published: link
                .published
                .clone()
                .filter(|date| !date.trim().is_empty())
                .or(page.published),
            updated: link.updated.clone(),
            fetched_at: page.fetched_at,
            engines: link.engines.clone(),
            passages,
            content_hash: Some(hash),
            truncated,
            returned_characters,
            available_characters,
            next_offset,
            error: page.error,
        });
    }
}

/// Canonical URL identities ignore fragments, which do not select a new body.
fn identity_url(raw: &str) -> String {
    url::Url::parse(raw)
        .map(|mut url| {
            url.set_fragment(None);
            url.to_string()
        })
        .unwrap_or_else(|_| raw.to_owned())
}

/// Diversity uses the final destination's normalized host, never its path.
fn host(raw: &str) -> String {
    url::Url::parse(raw)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default()
}

/// One `EngineState` per engine across the fan-out, counting every query's
/// results and reporting the worst status so a failure is never hidden behind a
/// later success.
fn merge_engine_states(answers: &[Answer]) -> Vec<EngineState> {
    let mut merged: Vec<EngineState> = Vec::new();
    for answer in answers {
        for state in &answer.engines {
            match merged.iter_mut().find(|engine| engine.name == state.name) {
                Some(existing) => {
                    existing.count += state.count;
                    existing.skipped += state.skipped;
                    existing.elapsed_ms = existing.elapsed_ms.max(state.elapsed_ms);
                    for filter in &state.unsupported_filters {
                        if !existing.unsupported_filters.contains(filter) {
                            existing.unsupported_filters.push(filter.clone());
                        }
                    }
                    if state.status == EngineStatus::Timeout
                        || (state.status == EngineStatus::Error
                            && existing.status == EngineStatus::Ok)
                    {
                        existing.status = state.status;
                    }
                    if let Some(error) = &state.error {
                        if let Some(previous) = &mut existing.error {
                            if !previous.split("; ").any(|message| message == error) {
                                previous.push_str("; ");
                                previous.push_str(error);
                            }
                        } else {
                            existing.error = Some(error.clone());
                        }
                    }
                }
                None => merged.push(state.clone()),
            }
        }
    }
    merged.sort_by(|a, b| a.name.cmp(&b.name));
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Config;
    use serde_json::json;
    use std::sync::Arc;

    /// Pure fixture assembly uses configured nonzero bounds without discovery.
    fn assembly() -> Assembly {
        Assembly::new(
            ResearchOptions {
                sources: 3,
                source_characters: 100,
                total_characters: 300,
                max_per_host: 1,
                max_attempts: 5,
                ..Default::default()
            },
            "keyword".into(),
            5,
            Session {
                touched: Instant::now(),
                seen: Vec::new(),
            },
        )
    }

    fn link(url: &str) -> Link {
        Link {
            title: "Fixture".into(),
            url: url.into(),
            snippet: None,
            snippets: Vec::new(),
            published: None,
            updated: Some("2026-02-03".into()),
            engines: vec!["fixture".into()],
            score: 1.0,
        }
    }

    fn page(url: &str, text: &str) -> crate::core::fetch::Page {
        crate::core::fetch::Page {
            url: url.into(),
            final_url: None,
            fetched_at: Some(1),
            status: 200,
            content_type: "text/plain".into(),
            title: None,
            byline: None,
            published: Some("2026-01-02".into()),
            site: None,
            text: text.into(),
            links: Vec::new(),
            truncated: None,
            redirect: None,
            error: None,
        }
    }

    /// A failed source leaves both source and host slots available for replacement.
    #[test]
    fn failed_sources_do_not_consume_diversity_or_useful_slots() {
        let mut state = assembly();
        let failed = link("https://a.test/missing");
        assert!(state.visit(&failed));
        state.coverage.attempted += 1;
        state.failure(&failed, "HTTP 404".into());
        let replacement = link("https://a.test/useful");
        assert!(state.visit(&replacement));
        state.coverage.attempted += 1;
        state.accept(&replacement, page(&replacement.url, "keyword evidence"));
        assert_eq!(state.coverage.failed, 1);
        assert_eq!(state.coverage.read, 1);
        assert_eq!(state.sources[0].error.as_deref(), Some("HTTP 404"));
        assert_eq!(state.remembered.len(), 1);
        let third = link("https://a.test/third");
        assert!(state.visit(&third));
        state.accept(&third, page(&third.url, "keyword third evidence"));
        assert_eq!(state.coverage.skipped, 1);
    }

    /// Diversity belongs to the final host: an already full discovery host can
    /// still redirect to a distinct useful destination.
    #[test]
    fn full_origin_does_not_exclude_distinct_redirect_destination() {
        let mut state = assembly();
        let first = link("https://a.test/first");
        state.accept(&first, page(&first.url, "keyword first evidence"));
        let redirect = link("https://a.test/redirect");
        assert!(state.visit(&redirect));
        let mut fetched = page(&redirect.url, "keyword distinct evidence");
        fetched.final_url = Some("https://b.test/article".into());
        state.accept(&redirect, fetched);
        assert_eq!(state.coverage.read, 2);
        assert_eq!(state.coverage.skipped, 0);
    }

    /// A completed but unusable download counts as fetched and failed, and
    /// leaves character and useful-source budgets available for replacements.
    #[test]
    fn unusable_download_is_counted_without_consuming_evidence_budget() {
        let mut state = assembly();
        let source = link("https://a.test/empty");
        state.accept(&source, page(&source.url, "  \n "));
        assert_eq!(state.coverage.fetched, 1);
        assert_eq!(state.coverage.failed, 1);
        assert_eq!(state.coverage.read, 0);
        assert_eq!(state.characters, 0);
        assert!(state.remembered.is_empty());
    }

    /// Redirect aliases and exact clean-body mirrors contribute only one source;
    /// duplicate bodies do not reserve a host's diversity allowance.
    #[test]
    fn aliases_and_body_duplicates_are_deduplicated_before_budgeting() {
        let mut state = assembly();
        let first = link("https://alias.test/start");
        let mut fetched = page(&first.url, "keyword evidence");
        fetched.final_url = Some("https://a.test/final".into());
        state.accept(&first, fetched);
        assert!(!state.visit(&link("https://a.test/final#section")));
        let mirror = link("https://b.test/mirror");
        state.accept(&mirror, page(&mirror.url, "keyword evidence"));
        assert_eq!(state.coverage.duplicate, 2);
        assert!(state.visit(&link("https://b.test/new")));
        assert_eq!(state.characters, "keyword evidence".chars().count());
        assert_eq!(state.coverage.read, 1);
        assert_eq!(state.sources[0].content_hash.as_ref().unwrap().len(), 64);
    }

    /// Download truncation remains explicit even if every downloaded character
    /// fits; page dates fill missing engine dates, and partial errors are kept.
    #[test]
    fn page_dates_and_partial_download_provenance_survive() {
        let mut state = assembly();
        let source = link("https://a.test/article");
        let mut fetched = page(&source.url, "keyword partial");
        fetched.truncated = Some(true);
        fetched.error = Some("connection ended early".into());
        state.accept(&source, fetched);
        let evidence = &state.sources[0];
        assert_eq!(evidence.published.as_deref(), Some("2026-01-02"));
        assert_eq!(evidence.updated.as_deref(), Some("2026-02-03"));
        assert!(evidence.truncated);
        assert_eq!(evidence.returned_characters, evidence.available_characters);
        assert_eq!(evidence.error.as_deref(), Some("connection ended early"));
        assert_eq!(state.coverage.incomplete, 1);
        assert_eq!(state.coverage.read, 1);
    }

    /// Explicit character bounds expose the available size and continuation and
    /// stop the assembly before any subsequent source can be attempted.
    #[test]
    fn spent_budget_and_attempt_ceiling_stop_work_honestly() {
        let mut state = assembly();
        state.options.total_characters = 3;
        let source = link("https://a.test/article");
        state.accept(&source, page(&source.url, "关键词 evidence"));
        let evidence = &state.sources[0];
        assert_eq!(state.characters, 3);
        assert_eq!(evidence.returned_characters, 3);
        assert_eq!(evidence.available_characters, 12);
        assert_eq!(evidence.next_offset, Some(3));
        assert_eq!(evidence.passages[0].next_offset, Some(3));
        assert!(state.finished());
        let mut attempts = assembly();
        attempts.coverage.attempted = attempts.options.max_attempts;
        assert!(attempts.finished());
    }

    /// Operator settings, rather than client constants, resolve all zero fields.
    #[test]
    fn request_defaults_resolve_only_at_core() {
        assert_eq!(ResearchOptions::default().resolved().sources, 0);
        let settings = ResearchSettings {
            sources: 2,
            source_characters: 9,
            total_characters: 17,
            max_per_host: 1,
            max_attempts: 4,
            timeout: 123,
            ..Default::default()
        };
        let options = ResearchOptions::default().configured(&settings);
        assert_eq!(
            (
                options.sources,
                options.source_characters,
                options.total_characters,
                options.max_per_host,
                options.max_attempts,
                options.timeout
            ),
            (2, 9, 17, 1, 4, 123)
        );
        assert_eq!(
            ResearchOptions {
                timeout: 1000,
                sources: 100,
                ..Default::default()
            }
            .configured(&settings)
            .timeout,
            123
        );
    }

    /// Sessions isolate namespaced identities, expire idle state and bound both
    /// the number of sessions and each session's retained identities.
    #[test]
    fn ephemeral_sessions_are_isolated_expiring_and_bounded() {
        let settings = ResearchSettings {
            session_entries: 2,
            session_ttl: 1000,
            ..Default::default()
        };
        let mut store = SessionStore::default();
        let identity = ("url".into(), "final".into(), "hash".into());
        store.commit("device-a/session", vec![identity.clone()], &settings);
        assert_eq!(
            store.snapshot(Some("device-a/session"), &settings).seen,
            vec![identity]
        );
        assert!(store
            .snapshot(Some("device-b/session"), &settings)
            .seen
            .is_empty());
        store.entries.get_mut("device-a/session").unwrap().touched =
            Instant::now() - Duration::from_secs(2);
        assert!(store
            .snapshot(Some("device-a/session"), &settings)
            .seen
            .is_empty());
        store.commit(
            "a",
            vec![
                ("1".into(), "1".into(), "1".into()),
                ("2".into(), "2".into(), "2".into()),
                ("3".into(), "3".into(), "3".into()),
            ],
            &settings,
        );
        assert_eq!(store.snapshot(Some("a"), &settings).seen.len(), 2);
        store.commit("b", vec![("b".into(), "b".into(), "b".into())], &settings);
        store.commit("c", vec![("c".into(), "c".into(), "c".into())], &settings);
        assert_eq!(store.entries.len(), 2);
        assert!(store.snapshot(Some("a"), &settings).seen.is_empty());
    }

    /// Session URL and body matches are distinguished from within-call duplicates;
    /// evidence rejected by a budget is never committed to seen state.
    #[test]
    fn session_seen_state_remembers_only_useful_evidence() {
        let mut prior = assembly();
        let source = link("https://a.test/article");
        prior.accept(&source, page(&source.url, "keyword evidence"));
        let mut next = assembly();
        next.previous.seen = prior.remembered;
        assert!(!next.visit(&source));
        let mirror = link("https://b.test/mirror");
        next.accept(&mirror, page(&mirror.url, "keyword evidence"));
        assert_eq!(next.coverage.previously_seen, 2);
        assert!(next.remembered.is_empty());
        assert_eq!(next.characters, 0);
    }

    /// Equal inputs choose equal useful passages, hashes and character bounds.
    #[test]
    fn useful_assembly_is_deterministic() {
        let run = || {
            let mut state = assembly();
            let source = link("https://a.test/article");
            state.accept(
                &source,
                page(
                    &source.url,
                    "Intro.\n\nkeyword matching evidence.\n\nkeyword another paragraph.",
                ),
            );
            serde_json::to_value(state.sources).unwrap()
        };
        assert_eq!(run(), run());
    }

    /// Fan-out retains every distinct engine failure and unsupported filter,
    /// with timeout severity stable across query completion order.
    #[test]
    fn engine_merge_keeps_errors_and_unsupported_filters() {
        let state = |status, error: &str, filter: &str| EngineState {
            name: "fixture".into(),
            status,
            error: Some(error.into()),
            count: 0,
            skipped: 0,
            elapsed_ms: 1,
            unsupported_filters: vec![filter.into()],
        };
        let answer = |state| Answer {
            query: "q".into(),
            results: Vec::new(),
            engines: vec![state],
            duration_ms: 1,
        };
        let merged = merge_engine_states(&[
            answer(state(EngineStatus::Timeout, "deadline", "freshness")),
            answer(state(EngineStatus::Error, "failed", "language")),
        ]);
        assert_eq!(merged[0].status, EngineStatus::Timeout);
        assert_eq!(merged[0].error.as_deref(), Some("deadline; failed"));
        assert_eq!(merged[0].unsupported_filters, ["freshness", "language"]);
    }

    /// A channel-gated selection stays running beyond the deadline without
    /// blocking a single-thread runtime. No elapsed-time or workload-speed
    /// assertions: the gate is released only after timeout and heartbeat checks.
    #[tokio::test(flavor = "current_thread")]
    async fn abandoned_selection_preserves_partial_evidence_and_its_permit() {
        let mut settings = Config::default().fetch;
        settings.max_concurrency = 1;
        let fetcher = Arc::new(crate::core::fetch::Fetcher::new(settings, "fixture").unwrap());
        let mut partial = assembly();
        let first = link("https://a.test/first");
        partial.accept(&first, page(&first.url, "keyword prior evidence"));
        let prior = serde_json::to_value(&partial.sources).unwrap();
        let remembered = partial.remembered.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let running_fetcher = Arc::clone(&fetcher);
        let task = tokio::spawn(async move {
            let outcome = partial
                .work(
                    &running_fetcher,
                    tokio::time::Instant::now() + Duration::from_secs(5),
                    move |next| {
                        entered_tx.send(()).unwrap();
                        // Watchdog prevents a regressed synchronous implementation from
                        // hanging the test suite; correctness never depends on its speed.
                        release_rx.recv_timeout(Duration::from_secs(30)).unwrap();
                        let source = link("https://b.test/late");
                        next.accept(
                            &source,
                            page(
                                &source.url,
                                &format!("keyword {}", "details ".repeat(10000)),
                            ),
                        );
                        let _ = finished_tx.send(());
                    },
                )
                .await;
            (partial, outcome)
        });
        entered_rx.await.unwrap();
        let heartbeat = tokio::spawn(async { "runtime responsive" });
        assert_eq!(heartbeat.await.unwrap(), "runtime responsive");
        let (partial, outcome) = task.await.unwrap();
        assert_eq!(outcome, Err("research deadline exceeded during assembly"));
        assert_eq!(serde_json::to_value(&partial.sources).unwrap(), prior);
        assert_eq!(partial.remembered, remembered);
        assert!(
            fetcher.crawl_work(Duration::ZERO, || ()).await.is_err(),
            "abandoned selection must still retain the sole permit"
        );
        release_tx.send(()).unwrap();
        finished_rx.await.unwrap();
        assert!(fetcher
            .crawl_work(Duration::from_secs(5), || ())
            .await
            .is_ok());
    }

    /// Serve fixed HTML routes on a loopback port; unknown paths are 404.
    async fn serve(routes: Vec<(&'static str, String)>) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let routes = Arc::new(routes);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let routes = Arc::clone(&routes);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => request.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let request = String::from_utf8_lossy(&request);
                    let path = request.split_whitespace().nth(1).unwrap_or("/");
                    if path == "/c"
                        && routes
                            .iter()
                            .any(|(route, body)| *route == "/c" && body == "slow")
                    {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                    let (status, body) = match routes.iter().find(|(route, _)| *route == path) {
                        Some((_, body)) => ("200 OK", body.clone()),
                        None => ("404 Not Found", String::new()),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body.as_bytes()).await;
                });
            }
        });
        port
    }

    /// A page whose article mentions `term`, long enough to score as content.
    fn article(term: &str) -> String {
        format!(
            "<html><head><title>{term}</title></head><body><article><h1>{term}</h1>\
             <p>The {term} rules are enforced at compile time, which is the whole point \
             of the {term} model in this language.</p>{}</article></body></html>",
            "<p>Filler paragraph with enough words to score as content, and a clause, \
             to matter to the scorer.</p>"
                .repeat(2)
        )
    }

    /// A command engine returning three loopback results on one host, each dated.
    fn engine_script(port: u16) -> String {
        format!(
            r#"
import json,sys
json.load(sys.stdin)
print(json.dumps({{"results":[
 {{"title":"Alpha","url":"http://127.0.0.1:{port}/a","published":"2026-01-02"}},
 {{"title":"Beta","url":"http://127.0.0.1:{port}/b","published":"2026-02-03"}},
 {{"title":"Gamma","url":"http://127.0.0.1:{port}/c","published":"2026-03-04"}}
]}}))
"#
        )
    }

    fn service(home: &std::path::Path, script: &str) -> Search {
        let mut config: Config = serde_json::from_value(json!({
            "engines": {"use": ["fixture"], "config": {"fixture": {
                "type": "command",
                "command": if cfg!(windows) { "python.exe" } else { "python3" },
                "args": ["-c", script],
                "cwd": home
            }}},
            "fetch": {"allow_private_networks": true, "max_response_bytes": 1048576}
        }))
        .unwrap();
        config.home = home.to_path_buf();
        Search::open(config).unwrap()
    }

    fn home() -> std::path::PathBuf {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("search-research-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        root
    }

    /// A packet reads distinct sources, keeps their dates, and returns the
    /// passages that match the intent, all under the configured budget.
    #[tokio::test]
    async fn a_packet_reads_diverse_sources_with_passages_and_dates() {
        let home = home();
        let port = serve(vec![
            ("/a", article("ownership")),
            ("/b", article("ownership beta")),
            ("/c", article("ownership gamma")),
        ])
        .await;
        let service = service(&home, &engine_script(port));
        let packet = service
            .research(
                vec!["ownership".into()],
                ResearchOptions {
                    sources: 3,
                    ..ResearchOptions::default()
                },
            )
            .await;
        std::fs::remove_dir_all(&home).unwrap();

        // Three candidates on one host, but only `max_per_host` are kept.
        assert_eq!(packet.coverage.candidates, 3);
        assert_eq!(packet.coverage.selected, 3);
        assert_eq!(packet.coverage.read, 2);
        assert_eq!(packet.coverage.failed, 0);
        assert_eq!(packet.coverage.skipped, 1);
        assert_eq!(packet.sources.len(), 2);
        assert!(packet.characters > 0);
        for source in &packet.sources {
            assert!(source.published.is_some(), "date is kept");
            assert!(
                source
                    .passages
                    .iter()
                    .any(|passage| passage.text.contains("ownership")),
                "passages match the intent"
            );
        }
    }

    /// Repeating an intent cannot create extra fusion votes or multiply engine
    /// work; the packet still echoes the caller's requested query list.
    #[tokio::test]
    async fn duplicate_queries_share_one_discovery() {
        let home = home();
        let port = serve(vec![("/a", article("ownership"))]).await;
        let service = service(&home, &engine_script(port));
        let packet = service
            .research(
                vec!["ownership".into(), "ownership".into()],
                ResearchOptions {
                    sources: 1,
                    ..Default::default()
                },
            )
            .await;
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(packet.engines[0].count, 3);
        assert_eq!(packet.coverage.read, 1);
        assert_eq!(packet.queries, ["ownership", "ownership"]);
    }

    /// A source whose fetch fails is reported as failed, not silently dropped.
    #[tokio::test]
    async fn a_failed_source_is_counted_in_coverage() {
        let home = home();
        let port = serve(vec![("/a", article("ownership"))]).await;
        let service = service(&home, &engine_script(port));
        let packet = service
            .research(vec!["ownership".into()], ResearchOptions::default())
            .await;
        std::fs::remove_dir_all(&home).unwrap();

        assert_eq!(packet.coverage.read, 1);
        assert_eq!(packet.coverage.failed, 2);
        let failed = packet
            .sources
            .iter()
            .find(|source| source.error.is_some())
            .expect("a failed source stays in the packet");
        assert_eq!(failed.error.as_deref(), Some("HTTP 404"));
        assert!(failed.passages.is_empty());
    }

    /// The total budget stops fetching immediately; later candidates remain
    /// unvisited, rather than being falsely counted as skipped downloads.
    #[tokio::test]
    async fn the_total_budget_bounds_the_packet() {
        let home = home();
        let port = serve(vec![
            ("/a", article("ownership")),
            ("/b", article("ownership beta")),
        ])
        .await;
        let service = service(&home, &engine_script(port));
        let packet = service
            .research(
                vec!["ownership".into()],
                ResearchOptions {
                    total_characters: 1,
                    ..ResearchOptions::default()
                },
            )
            .await;
        std::fs::remove_dir_all(&home).unwrap();

        assert_eq!(packet.coverage.read, 1);
        assert_eq!(packet.coverage.skipped, 0);
        assert_eq!(packet.coverage.attempted, 1);
        assert_eq!(packet.coverage.unvisited, 2);
        assert_eq!(packet.characters, 1);
    }
    /// The first failed download is replaced by later useful evidence; a full
    /// source slot is independent of failures and host diversity counts.
    #[tokio::test]
    async fn failed_downloads_are_replaced_in_the_live_pipeline() {
        let home = home();
        let port = serve(vec![
            ("/b", article("ownership beta")),
            ("/c", article("ownership gamma")),
        ])
        .await;
        let service = service(&home, &engine_script(port));
        let packet = service
            .research(
                vec!["ownership".into()],
                ResearchOptions {
                    sources: 2,
                    ..Default::default()
                },
            )
            .await;
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(
            (
                packet.coverage.attempted,
                packet.coverage.read,
                packet.coverage.failed
            ),
            (3, 2, 1)
        );
        assert_eq!(packet.coverage.unvisited, 0);
        assert!(packet.incomplete);
        assert!(packet
            .sources
            .iter()
            .any(|source| source.error.as_deref() == Some("HTTP 404")));
    }

    /// One research deadline covers discovery and reading, preserving the first
    /// useful page and the failed in-flight source when a later fetch stalls.
    #[tokio::test]
    async fn a_deadline_returns_useful_partial_evidence() {
        let home = home();
        let port = serve(vec![
            ("/b", article("ownership beta")),
            ("/c", "slow".into()),
        ])
        .await;
        let service = service(&home, &engine_script(port));
        let packet = service
            .research(
                vec!["ownership".into()],
                ResearchOptions {
                    timeout: 200,
                    ..Default::default()
                },
            )
            .await;
        std::fs::remove_dir_all(&home).unwrap();
        assert!(packet.timed_out);
        assert!(packet.incomplete);
        assert_eq!(packet.coverage.read, 1);
        assert!(packet.characters > 0);
        assert!(packet.sources.iter().any(
            |source| source.error.as_deref() == Some("research deadline exceeded during fetch")
        ));
        assert!(packet.duration_ms < 500);
    }
    /// Research forwards the complete filter object to configured discovery;
    /// a session excludes useful prior URLs without crossing caller namespaces.
    #[tokio::test]
    async fn query_filters_and_session_identity_reach_the_live_pipeline() {
        let home = home();
        let port = serve(vec![
            ("/a", article("ownership alpha")),
            ("/b", article("ownership beta")),
            ("/c", article("ownership gamma")),
        ])
        .await;
        let script = engine_script(port).replace(
            "json.load(sys.stdin)",
            "request=json.load(sys.stdin)\nassert request['filters']['exact_phrase']=='ownership'",
        );
        let service = service(&home, &script);
        let options = ResearchOptions {
            sources: 1,
            filters: QueryFilters {
                exact_phrase: Some("ownership".into()),
                ..Default::default()
            },
            session: Some("device-a/session".into()),
            ..Default::default()
        };
        let first = service
            .research(vec!["ownership".into()], options.clone())
            .await;
        let second = service
            .research(vec!["ownership".into()], options.clone())
            .await;
        let other = service
            .research(
                vec!["ownership".into()],
                ResearchOptions {
                    session: Some("device-b/session".into()),
                    ..options
                },
            )
            .await;
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(first.coverage.read, 1);
        assert_eq!(second.coverage.previously_seen, 1);
        assert_ne!(first.sources[0].url, second.sources[0].url);
        assert_eq!(first.sources[0].url, other.sources[0].url);
        assert_eq!(other.coverage.previously_seen, 0);
    }
    /// A real byte-limited response reports a truncated download in evidence,
    /// even though passage selection remains within the available clean text.
    #[tokio::test]
    async fn download_byte_limits_are_visible_in_the_packet() {
        let home = home();
        let port = serve(vec![("/a", article("ownership").repeat(30))]).await;
        let initial = service(&home, &engine_script(port));
        let mut config = initial.config().clone();
        config.fetch.max_response_bytes = 256;
        let service = Search::open(config).unwrap();
        let packet = service
            .research(
                vec!["ownership".into()],
                ResearchOptions {
                    sources: 1,
                    ..Default::default()
                },
            )
            .await;
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(packet.coverage.read, 1);
        assert!(packet.sources[0].truncated);
        assert!(packet.incomplete);
        assert_eq!(packet.coverage.incomplete, 1);
        assert!(packet.sources[0].returned_characters <= packet.sources[0].available_characters);
    }
}
