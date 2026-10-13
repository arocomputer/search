//! The fan-out: run every selected engine concurrently under its own
//! deadline, then merge the answers into one ranking.
//!
//! Failure is interpretive, not fatal: an engine that errors is reported in its
//! `EngineState` beside the results, so an empty result set is never mistaken
//! for a broken one.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::task::JoinSet;

use super::{Engine, EngineError, EngineState, EngineStatus, FailureCause, Found, Ranked};
use crate::core::config::{Config, ConfigError, SearchSettings};
use crate::core::{Answer, Link, Query};

/// Owns enabled engine transports and query bounds.
pub struct Pool {
    engines: Vec<Arc<dyn Engine>>,
    settings: SearchSettings,
    cache: std::sync::Mutex<super::cache::Cache>,
}

impl Pool {
    /// Resolve each selected engine from settings and build its transport
    /// without reading credentials or running engine code.
    pub fn new(config: &Config) -> Result<Self, ConfigError> {
        config.validate()?;
        let mut engines: Vec<Arc<dyn Engine>> = Vec::new();
        if config.engines.enabled {
            for id in &config.engines.use_engines {
                let settings = config.engines.adapter(id)?;
                let adapter =
                    super::adapter::Adapter::new(id.clone(), settings, &config.user_agent)
                        .map_err(|_| {
                            ConfigError::new(format!("engine {id}: HTTP client failed"))
                        })?;
                engines.push(Arc::new(adapter));
            }
        }
        Ok(Self {
            engines,
            settings: config.search.clone(),
            cache: Default::default(),
        })
    }

    /// The enabled engine names, for status output.
    pub fn names(&self) -> Vec<String> {
        self.engines.iter().map(|p| p.name().to_owned()).collect()
    }

    /// The server-side ceiling for one query.
    pub fn overall_timeout(&self) -> Duration {
        self.settings.timeout()
    }

    /// Run the query across every selected engine and merge the answers.
    pub async fn search(&self, query: Query) -> Answer {
        let started = Instant::now();
        if let Err(error) = query.filters.validate() {
            return Answer {
                query: query.text,
                results: Vec::new(),
                engines: vec![failed("query", EngineStatus::Error, error, 0)],
                duration_ms: 0,
            };
        }
        let key = serde_json::to_string(&query).unwrap_or_default();
        let ttl = Duration::from_millis(self.settings.cache_ttl);
        if !ttl.is_zero() && self.settings.cache_bytes > 0 {
            if let Ok(mut cache) = self.cache.lock() {
                if let Some(mut answer) = cache.get(&key, ttl) {
                    answer.duration_ms = started.elapsed().as_millis() as u64;
                    return answer;
                }
            }
        }
        let limit = self.settings.result_limit(query.limit);
        let selected: Vec<Arc<dyn Engine>> = self.select(&query.engines);
        let per_engine_time = self.settings.engine_timeout();

        let mut set: JoinSet<(EngineState, Vec<Ranked>)> = JoinSet::new();
        let mut tasks = HashMap::new();
        for (index, engine) in selected.iter().enumerate() {
            let engine = Arc::clone(engine);
            let request = query.clone();
            let task =
                set.spawn(async move { run_engine(engine, request, limit, per_engine_time).await });
            tasks.insert(task.id(), index);
        }

        // Ordered by engine index so output is stable regardless of finish order.
        let mut collected: Vec<Option<(EngineState, Vec<Ranked>)>> =
            (0..selected.len()).map(|_| None).collect();
        let completed = tokio::time::timeout(self.settings.timeout(), async {
            while let Some(joined) = set.join_next_with_id().await {
                let (id, outcome) = match joined {
                    Ok((id, outcome)) => (id, Some(outcome)),
                    // A panicking engine must not take the query down; it is
                    // reported under its own name as a failed engine.
                    Err(error) => (error.id(), None),
                };
                // Task IDs map to indexes produced by `enumerate` over
                // `selected`, so lookups succeed; `get` keeps that provable.
                let Some(&index) = tasks.get(&id) else {
                    continue;
                };
                let (Some(slot), Some(engine)) = (collected.get_mut(index), selected.get(index))
                else {
                    continue;
                };
                *slot = Some(outcome.unwrap_or_else(|| {
                    let mut state =
                        failed(engine.name(), EngineStatus::Error, "engine task failed", 0);
                    state.unsupported_filters = engine.unsupported_filters(&query.filters);
                    (state, Vec::new())
                }));
            }
        })
        .await
        .is_ok();

        if !completed {
            set.abort_all();
            while set.join_next().await.is_some() {}
            for (index, engine) in selected.iter().enumerate() {
                if let Some(slot) = collected.get_mut(index) {
                    if slot.is_none() {
                        let mut state = failed(
                            engine.name(),
                            EngineStatus::Timeout,
                            "engine exceeded the overall search deadline",
                            started.elapsed().as_millis() as u64,
                        );
                        state.unsupported_filters = engine.unsupported_filters(&query.filters);
                        *slot = Some((state, Vec::new()));
                    }
                }
            }
        }

        let mut states: Vec<EngineState> = query
            .engines
            .iter()
            .filter(|name| {
                !self
                    .engines
                    .iter()
                    .any(|engine| engine.name() == name.as_str())
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|name| {
                let mut state = failed(name, EngineStatus::Error, "engine is not enabled", 0);
                state.unsupported_filters = query.filters.values().into_keys().collect();
                state
            })
            .collect();
        let mut merged = Vec::new();
        for slot in collected.into_iter().flatten() {
            let (state, results) = slot;
            states.push(state);
            merged.extend(results);
        }
        states.sort_by(|a, b| a.name.cmp(&b.name));

        let results = fuse(&merged, limit);
        let answer = Answer {
            query: query.text,
            results,
            engines: states,
            duration_ms: started.elapsed().as_millis() as u64,
        };
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(key, &answer, ttl, self.settings.cache_bytes);
        }
        answer
    }

    /// Restrict to the requested names, preserving pool order. A request for
    /// names that match nothing returns no engines rather than silently
    /// running them all — the caller sees the empty result and its cause.
    fn select(&self, names: &[String]) -> Vec<Arc<dyn Engine>> {
        if names.is_empty() {
            return self.engines.clone();
        }
        self.engines
            .iter()
            .filter(|p| names.iter().any(|n| n == p.name()))
            .cloned()
            .collect()
    }
}

/// The state of an engine that produced no results.
fn failed(name: &str, status: EngineStatus, error: &str, elapsed_ms: u64) -> EngineState {
    EngineState {
        name: name.into(),
        status,
        count: 0,
        skipped: 0,
        unsupported_filters: Vec::new(),
        error: Some(error.into()),
        elapsed_ms,
    }
}

/// Run one engine under its deadline and classify the outcome. Each result is
/// stamped with its rank in this engine's own ordering, which is what fusion
/// scores on — an engine's rank-1 must count as rank-1 no matter where its
/// results land in the merged concatenation.
async fn run_engine(
    engine: Arc<dyn Engine>,
    query: Query,
    limit: usize,
    budget: Duration,
) -> (EngineState, Vec<Ranked>) {
    let name = engine.name().to_string();
    let started = Instant::now();
    let requested_filters = query.filters.values();
    let mut unsupported_filters = engine.unsupported_filters(&query.filters);
    let outcome = tokio::time::timeout(budget, engine.search_query(query, limit)).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    let (
        status,
        Found {
            links,
            skipped,
            unsupported_filters: reported,
        },
        error,
    ) = match outcome {
        Ok(Ok(found)) => (EngineStatus::Ok, found, None),
        Ok(Err(error)) => {
            let status = match error.cause {
                FailureCause::Timeout => EngineStatus::Timeout,
                _ => EngineStatus::Error,
            };
            (status, Found::default(), Some(error.message))
        }
        Err(_) => (
            EngineStatus::Timeout,
            Found::default(),
            Some(EngineError::network("engine exceeded its deadline").message),
        ),
    };
    for field in reported {
        if requested_filters.contains_key(&field) && !unsupported_filters.contains(&field) {
            unsupported_filters.push(field);
        }
    }
    unsupported_filters.sort();
    // Stamp each link with its rank in this engine's own ordering; fusion
    // scores on that, not on where it lands in the merged concatenation.
    let results: Vec<Ranked> = links
        .into_iter()
        .enumerate()
        .map(|(rank, link)| Ranked {
            link,
            rank: rank + 1,
        })
        .collect();

    let state = EngineState {
        name,
        status,
        count: results.len(),
        skipped,
        unsupported_filters,
        error,
        elapsed_ms,
    };
    (state, results)
}

/// Merge engine answers with reciprocal-rank fusion: each engine votes
/// `1/(k + rank)`, so a URL several independent engines rank well rises above
/// one that only a single engine liked. Each normalized URL receives at most
/// one vote per locally stamped engine, using that engine's best rank.
fn fuse(results: &[Ranked], limit: usize) -> Vec<Link> {
    const K: f64 = 10.0;
    use std::collections::HashMap;

    struct Aggregate {
        link: Link,
        score: f64,
        ranks: Vec<(String, usize)>,
        order: usize,
    }

    let mut seen: HashMap<String, Aggregate> = HashMap::new();
    let mut order = 0usize;
    for ranked in results {
        let key = normalize_url(&ranked.link.url);
        if key.is_empty() {
            continue;
        }
        // The rank comes from the engine's own ordering, stamped before the
        // merge — not from this concatenation's position, which would penalize
        // whichever engine happened to be appended later.
        let rank = ranked.rank.max(1);
        let existing = seen.entry(key).or_insert_with(|| {
            let aggregate = Aggregate {
                link: ranked.link.clone(),
                score: 0.0,
                ranks: Vec::new(),
                order,
            };
            order += 1;
            aggregate
        });
        let incoming = &ranked.link;
        merge_passages(&mut existing.link, incoming);
        if existing.link.snippet.is_none() {
            existing.link.snippet = incoming.snippet.clone();
        }
        if existing.link.published.is_none() {
            existing.link.published = incoming.published.clone();
        }
        if existing.link.title.is_empty() && !incoming.title.is_empty() {
            existing.link.title = incoming.title.clone();
        }
        for engine in &incoming.engines {
            if let Some((_, best)) = existing.ranks.iter_mut().find(|(name, _)| name == engine) {
                *best = (*best).min(rank);
            } else {
                existing.ranks.push((engine.clone(), rank));
            }
            if !existing.link.engines.contains(engine) {
                existing.link.engines.push(engine.clone());
            }
        }
    }

    let mut out: Vec<Aggregate> = seen.into_values().collect();
    for aggregate in &mut out {
        aggregate.score = aggregate
            .ranks
            .iter()
            .map(|(_, rank)| 1.0 / (K + *rank as f64))
            .sum();
    }
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.order.cmp(&b.order))
    });
    out.into_iter()
        .take(limit)
        .map(|mut aggregate| {
            aggregate.link.score = aggregate.score;
            aggregate.link
        })
        .collect()
}

/// Keep distinct snippets and update metadata when several sources discover one URL.
fn merge_passages(target: &mut Link, incoming: &Link) {
    for snippet in target
        .snippet
        .clone()
        .iter()
        .chain(incoming.snippet.iter())
        .chain(incoming.snippets.iter())
    {
        if !target.snippets.contains(snippet) {
            target.snippets.push(snippet.clone());
        }
    }
    if target.updated.is_none() {
        target.updated = incoming.updated.clone();
    }
}

/// Merge the per-query answers of a research fan-out into one ranked list.
/// Each answer's results are already fused and ordered, so their position is
/// the rank this merge votes with — the same reciprocal-rank formula [`fuse`]
/// uses within one query, so agreement across queries raises a source.
pub(crate) fn fuse_answers(answers: &[Answer]) -> Vec<Link> {
    const K: f64 = 10.0;

    struct Aggregate {
        link: Link,
        score: f64,
        order: usize,
    }

    let mut seen: HashMap<String, Aggregate> = HashMap::new();
    let mut order = 0usize;
    for answer in answers {
        for (position, link) in answer.results.iter().enumerate() {
            let key = normalize_url(&link.url);
            if key.is_empty() {
                continue;
            }
            let aggregate = seen.entry(key).or_insert_with(|| {
                let aggregate = Aggregate {
                    link: link.clone(),
                    score: 0.0,
                    order,
                };
                order += 1;
                aggregate
            });
            aggregate.score += 1.0 / (K + (position + 1) as f64);
            merge_passages(&mut aggregate.link, link);
            if aggregate.link.snippet.is_none() {
                aggregate.link.snippet = link.snippet.clone();
            }
            if aggregate.link.published.is_none() {
                aggregate.link.published = link.published.clone();
            }
            if aggregate.link.title.is_empty() && !link.title.is_empty() {
                aggregate.link.title = link.title.clone();
            }
            for engine in &link.engines {
                if !aggregate.link.engines.contains(engine) {
                    aggregate.link.engines.push(engine.clone());
                }
            }
        }
    }

    let mut out: Vec<Aggregate> = seen.into_values().collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.order.cmp(&b.order))
    });
    out.into_iter()
        .map(|mut aggregate| {
            aggregate.link.score = aggregate.score;
            aggregate.link
        })
        .collect()
}

/// Normalize a URL for deduplication: lowercased scheme and host, no fragment,
/// no tracking parameters. Preserve path slashes and the raw encoding of meaningful
/// query parameters; servers may distinguish them. It never drops a meaningful query:
/// `ref` stays, because it selects a branch or tag on code hosts.
pub(crate) fn normalize_url(raw: &str) -> String {
    let Ok(mut url) = url::Url::parse(raw.trim()) else {
        return String::new();
    };
    if url.scheme() != "http" && url.scheme() != "https" {
        return String::new();
    }
    url.set_fragment(None);
    let tracking = [
        "utm_source",
        "utm_medium",
        "utm_campaign",
        "utm_term",
        "utm_content",
        "fbclid",
        "gclid",
        "mc_cid",
        "mc_eid",
    ];
    if let Some(query) = url.query() {
        let kept: Vec<&str> = query
            .split('&')
            .filter(|part| {
                !url::form_urlencoded::parse(part.as_bytes())
                    .next()
                    .is_some_and(|(key, _)| tracking.contains(&key.as_ref()))
            })
            .collect();
        let filtered = kept.join("&");
        if filtered != query {
            url.set_query((!filtered.is_empty()).then_some(filtered.as_str()));
        }
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    impl Pool {
        /// Build fixture transports without going through settings resolution.
        pub(in crate::core::engines) fn from_adapters(
            adapters: Vec<(&str, crate::core::config::Adapter)>,
            settings: SearchSettings,
        ) -> Self {
            Self {
                engines: adapters
                    .into_iter()
                    .map(|(name, settings)| {
                        Arc::new(
                            crate::core::engines::adapter::Adapter::new(
                                name.into(),
                                settings,
                                "search-test",
                            )
                            .unwrap(),
                        ) as Arc<dyn Engine>
                    })
                    .collect(),
                settings,
                cache: Default::default(),
            }
        }
    }

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestEngine {
        limit: Arc<AtomicUsize>,
        delay: Duration,
        count: usize,
    }

    impl Engine for TestEngine {
        fn name(&self) -> &str {
            "test"
        }

        fn search(&self, _query: String, limit: usize) -> super::super::EngineFuture {
            self.limit.store(limit, Ordering::SeqCst);
            let delay = self.delay;
            let count = self.count;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                Ok(Found {
                    links: (0..count)
                        .map(|i| Link {
                            title: format!("result {i}"),
                            url: format!("https://example.com/{i}"),
                            snippet: None,
                            snippets: Vec::new(),
                            updated: None,
                            published: None,
                            engines: vec!["test".into()],
                            score: 0.0,
                        })
                        .collect(),
                    skipped: 0,
                    unsupported_filters: Vec::new(),
                })
            })
        }
    }

    fn pool(engine: TestEngine, settings: SearchSettings) -> Pool {
        Pool {
            engines: vec![Arc::new(engine)],
            settings,
            cache: Default::default(),
        }
    }

    struct PanickingEngine;

    impl Engine for PanickingEngine {
        fn name(&self) -> &str {
            "fragile"
        }

        fn search(&self, _query: String, _limit: usize) -> super::super::EngineFuture {
            Box::pin(async { panic!("engine bug") })
        }
    }

    /// A panicking engine is reported under its own name, beside healthy engines.
    #[tokio::test]
    async fn panicking_engine_is_reported_by_name() {
        let mut pool = pool(
            TestEngine {
                limit: Arc::new(AtomicUsize::new(0)),
                delay: Duration::ZERO,
                count: 1,
            },
            SearchSettings::default(),
        );
        pool.engines.push(Arc::new(PanickingEngine));
        let answer = pool.search(Query::default()).await;
        assert_eq!(answer.results.len(), 1);
        let fragile = answer.engines.iter().find(|e| e.name == "fragile").unwrap();
        assert_eq!(fragile.status, EngineStatus::Error);
        assert_eq!(fragile.error.as_deref(), Some("engine task failed"));
    }

    #[tokio::test]
    async fn configured_result_limit_caps_engine_work_and_output() {
        let asked = Arc::new(AtomicUsize::new(0));
        let settings = SearchSettings {
            max_results: 2,
            ..SearchSettings::default()
        };
        let pool = pool(
            TestEngine {
                limit: Arc::clone(&asked),
                delay: Duration::ZERO,
                count: 5,
            },
            settings,
        );

        let response = pool
            .search(Query {
                text: "test".into(),
                limit: 10,
                engines: Vec::new(),
                ..Default::default()
            })
            .await;

        assert_eq!(asked.load(Ordering::SeqCst), 2);
        assert_eq!(response.results.len(), 2);
    }

    #[tokio::test]
    async fn unknown_engine_selection_is_reported_without_running_other_engines() {
        let asked = Arc::new(AtomicUsize::new(0));
        let pool = pool(
            TestEngine {
                limit: Arc::clone(&asked),
                delay: Duration::ZERO,
                count: 1,
            },
            SearchSettings::default(),
        );
        let answer = pool
            .search(Query {
                text: "query".into(),
                engines: vec!["missing".into()],
                ..Default::default()
            })
            .await;
        assert_eq!(asked.load(Ordering::SeqCst), 0);
        assert!(answer.results.is_empty());
        assert_eq!(answer.engines[0].name, "missing");
        assert_eq!(answer.engines[0].status, EngineStatus::Error);
    }

    #[tokio::test]
    async fn unavailable_selections_are_deduplicated_without_suppressing_enabled_engines() {
        let asked = Arc::new(AtomicUsize::new(0));
        let pool = pool(
            TestEngine {
                limit: Arc::clone(&asked),
                delay: Duration::ZERO,
                count: 1,
            },
            SearchSettings::default(),
        );
        let answer = pool
            .search(Query {
                text: "query".into(),
                engines: vec!["missing".into(), "test".into(), "missing".into()],
                ..Default::default()
            })
            .await;
        assert_eq!(
            asked.load(Ordering::SeqCst),
            SearchSettings::default().max_results
        );
        assert_eq!(answer.engines.len(), 2);
        let missing = answer
            .engines
            .iter()
            .find(|engine| engine.name == "missing")
            .unwrap();
        assert_eq!(missing.status, EngineStatus::Error);
        assert_eq!(missing.error.as_deref(), Some("engine is not enabled"));
        assert_eq!(answer.results.len(), 1);
    }

    #[tokio::test]
    async fn overall_deadline_returns_without_waiting_for_engine_deadline() {
        let asked = Arc::new(AtomicUsize::new(0));
        let settings = SearchSettings {
            timeout: 10,
            engine_timeout: 5_000,
            ..SearchSettings::default()
        };
        let pool = pool(
            TestEngine {
                limit: asked,
                delay: Duration::from_secs(5),
                count: 0,
            },
            settings,
        );

        let response = pool
            .search(Query {
                text: "test".into(),
                ..Query::default()
            })
            .await;

        assert_eq!(response.engines.len(), 1);
        assert_eq!(response.engines[0].status, EngineStatus::Timeout);
        assert_eq!(
            response.engines[0].error.as_deref(),
            Some("engine exceeded the overall search deadline")
        );
    }

    fn ranked(url: &str, engine: &'static str, rank: usize) -> Ranked {
        Ranked {
            link: Link {
                title: url.into(),
                url: url.into(),
                snippet: None,
                snippets: Vec::new(),
                updated: None,
                published: None,
                engines: vec![engine.into()],
                score: 0.0,
            },
            rank,
        }
    }

    /// A URL two engines rank highly must beat one only a single engine
    /// liked, and two engines' rank-1s must score the same regardless of which
    /// engine was merged first — the bug where a later engine's rank-1 was
    /// scored as if it were deep in the list.
    #[test]
    fn fusion_prefers_agreement() {
        let merged = vec![
            ranked("https://a.example/x", "brave", 1),
            ranked("https://a.example/x", "wikipedia", 1),
            ranked("https://b.example/y", "brave", 2),
            ranked("https://c.example/z", "mwmbl", 1),
        ];
        let out = fuse(&merged, 10);
        assert_eq!(out.len(), 3, "duplicates collapse");
        assert_eq!(out[0].url, "https://a.example/x");
        assert!(
            out[0].engines.contains(&"brave".into())
                && out[0].engines.contains(&"wikipedia".into()),
            "both engines are named, as a list"
        );
        // The single-vote rank-1 (mwmbl) must outrank the single-vote rank-2.
        let single_rank_one = out
            .iter()
            .position(|r| r.url == "https://c.example/z")
            .unwrap();
        let single_rank_two = out
            .iter()
            .position(|r| r.url == "https://b.example/y")
            .unwrap();
        assert!(
            single_rank_one < single_rank_two,
            "a later engine's rank-1 must not be penalized by merge order"
        );
    }

    /// Rank is taken from the engine's own ordering, so a rank-1 link does
    /// not inherit the concatenation position.
    #[test]
    fn fusion_scores_on_engine_rank_not_position() {
        let merged = vec![
            ranked("https://first.example/a", "brave", 5),
            ranked("https://second.example/b", "wikipedia", 1),
        ];
        let out = fuse(&merged, 10);
        assert_eq!(
            out[0].url, "https://second.example/b",
            "the rank-1 link wins even though it was merged second"
        );
    }

    /// Repeated rows and noisy URL variants cannot manufacture engine agreement.
    #[test]
    fn fusion_counts_one_vote_per_engine_and_normalized_url() {
        for duplicate in [
            "https://b.example/y",
            "https://b.example/y?utm_source=noise#section",
        ] {
            let merged = vec![
                ranked("https://a.example/x", "one", 1),
                ranked("https://b.example/y", "one", 2),
                ranked(duplicate, "one", 3),
            ];
            let out = fuse(&merged, 10);
            assert_eq!(out.len(), 2);
            assert_eq!(out[0].url, "https://a.example/x");
            assert_eq!(out[1].score, 1.0 / 12.0);
            assert_eq!(out[1].engines, ["one"]);
        }
    }

    /// A distinct engine still supplies an independent vote after deduplication.
    #[test]
    fn fusion_independent_engine_adds_vote_after_duplicate_rows() {
        let merged = vec![
            ranked("https://a.example/x", "one", 1),
            ranked("https://b.example/y", "one", 2),
            ranked("https://b.example/y#duplicate", "one", 3),
            ranked("https://b.example/y?utm_source=other", "two", 3),
        ];
        let out = fuse(&merged, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].url, "https://b.example/y");
        assert_eq!(out[0].score, 1.0 / 12.0 + 1.0 / 13.0);
        assert_eq!(out[0].engines, ["one", "two"]);
    }

    /// Later better ranks replace votes, while first-seen order and metadata merge survive.
    #[test]
    fn fusion_keeps_best_rank_and_merges_metadata_on_duplicate_rows() {
        let mut first = ranked("https://b.example/y", "one", 5);
        first.link.title.clear();
        let mut better = ranked("https://b.example/y#section", "one", 1);
        better.link.title = "Filled title".into();
        better.link.snippet = Some("Filled snippet".into());
        let merged = vec![first, ranked("https://a.example/x", "one", 1), better];
        let out = fuse(&merged, 10);
        assert_eq!(out[0].url, "https://b.example/y");
        assert_eq!(out[0].score, out[1].score);
        assert_eq!(out[0].score, 1.0 / 11.0);
        assert_eq!(out[0].title, "Filled title");
        assert_eq!(out[0].snippet.as_deref(), Some("Filled snippet"));
    }

    /// A engine name that is a substring of another must not dedupe as if it
    /// were the same engine — the bug a joined string invited.
    #[test]
    fn engine_names_are_compared_whole() {
        let merged = vec![
            ranked("https://a.example/x", "arxiv", 1),
            ranked("https://a.example/x", "xiv", 2),
        ];
        let out = fuse(&merged, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].engines, vec!["arxiv", "xiv"]);
    }

    /// Tracking parameters and fragments must not fragment a result.
    #[test]
    fn url_normalization_dedupes_noise() {
        assert_eq!(
            normalize_url("https://Example.com/Path?utm_source=news&id=7#frag"),
            normalize_url("https://example.com/Path?id=7")
        );
        assert!(normalize_url("javascript:alert(1)").is_empty());
        assert_ne!(
            normalize_url("https://github.com/o/r/blob/x?ref=main"),
            normalize_url("https://github.com/o/r/blob/x?ref=dev")
        );
    }

    /// Servers can serve distinct bodies at a path and its slash-suffixed form;
    /// discovery must retain both as candidates rather than silently losing one.
    #[test]
    fn fusion_preserves_significant_path_slashes() {
        let out = fuse(
            &[
                ranked("https://example.com/article", "one", 1),
                ranked("https://example.com/article/", "one", 2),
            ],
            10,
        );
        assert_eq!(out.len(), 2);
        assert_ne!(out[0].url, out[1].url);
    }

    /// Removing tracking must preserve meaningful query spelling, including
    /// bare flags and percent encoding used by strict or signed endpoints.
    #[test]
    fn url_normalization_preserves_raw_meaningful_query() {
        assert_eq!(
            normalize_url("https://example.com/?flag&value=a%20b&utm_source=noise"),
            "https://example.com/?flag&value=a%20b",
        );
        assert_ne!(
            normalize_url("https://example.com/?flag"),
            normalize_url("https://example.com/?flag="),
        );
        assert_eq!(
            normalize_url("https://example.com/?"),
            "https://example.com/?"
        );
    }

    /// A URL two queries both rank first beats a top hit from only one query.
    #[test]
    fn fuse_answers_rewards_cross_query_agreement() {
        let answer = |urls: [&str; 2]| Answer {
            query: "q".into(),
            results: urls
                .into_iter()
                .map(|url| ranked(url, "e", 1).link)
                .collect(),
            engines: Vec::new(),
            duration_ms: 0,
        };
        let merged = fuse_answers(&[
            answer(["https://a.example/x", "https://b.example/y"]),
            answer(["https://a.example/x", "https://c.example/z"]),
        ]);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].url, "https://a.example/x");
    }

    /// Cache keys separate every constraint, text, requested limit and engine selection.
    #[tokio::test]
    async fn answer_cache_isolates_full_requests_and_expires() {
        struct Counting(Arc<AtomicUsize>);
        impl Engine for Counting {
            fn name(&self) -> &str {
                "count"
            }
            fn search(&self, _: String, _: usize) -> super::super::EngineFuture {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(Found::default()) })
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let pool = Pool {
            engines: vec![Arc::new(Counting(calls.clone()))],
            settings: SearchSettings {
                cache_ttl: 200,
                ..Default::default()
            },
            cache: Default::default(),
        };
        let base = Query {
            text: "q".into(),
            ..Default::default()
        };
        let invalid = pool
            .search(Query {
                filters: crate::core::service::QueryFilters {
                    start_date: Some("2023-02-29".into()),
                    ..Default::default()
                },
                ..base.clone()
            })
            .await;
        assert_eq!(invalid.engines[0].status, EngineStatus::Error);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        pool.search(base.clone()).await;
        pool.search(base.clone()).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        for variant in [
            Query {
                text: "other".into(),
                ..base.clone()
            },
            Query {
                limit: 1,
                ..base.clone()
            },
            Query {
                engines: vec!["count".into()],
                ..base.clone()
            },
            Query {
                filters: crate::core::service::QueryFilters {
                    safe: Some(false),
                    ..Default::default()
                },
                ..base.clone()
            },
        ] {
            pool.search(variant.clone()).await;
            pool.search(variant).await;
        }
        for filters in [
            serde_json::json!({"freshness":"week"}),
            serde_json::json!({"start_date":"2024-01-01"}),
            serde_json::json!({"end_date":"2024-02-01"}),
            serde_json::json!({"include_domains":["example.com"]}),
            serde_json::json!({"exclude_domains":["example.com"]}),
            serde_json::json!({"language":"en"}),
            serde_json::json!({"region":"GB"}),
            serde_json::json!({"category":"science"}),
            serde_json::json!({"exact_phrase":"a phrase"}),
        ] {
            let variant = Query {
                filters: serde_json::from_value(filters).unwrap(),
                ..base.clone()
            };
            pool.search(variant.clone()).await;
            pool.search(variant).await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 14);
        tokio::time::sleep(Duration::from_millis(210)).await;
        pool.search(base).await;
        assert_eq!(calls.load(Ordering::SeqCst), 15);
    }

    /// Engine failures never become reusable empty answers.
    #[tokio::test]
    async fn answer_cache_never_stores_errors_or_timeouts() {
        struct Failing(Arc<AtomicUsize>);
        impl Engine for Failing {
            fn name(&self) -> &str {
                "failure"
            }
            fn search(&self, _: String, _: usize) -> super::super::EngineFuture {
                let call = self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    if call == 0 {
                        Err(EngineError::rejected("fixture"))
                    } else if call == 1 {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        Ok(Found::default())
                    } else {
                        Ok(Found::default())
                    }
                })
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let pool = Pool {
            engines: vec![Arc::new(Failing(calls.clone()))],
            settings: SearchSettings {
                engine_timeout: 20,
                ..Default::default()
            },
            cache: Default::default(),
        };
        assert_eq!(
            pool.search(Query::default()).await.engines[0].status,
            EngineStatus::Error
        );
        assert_eq!(
            pool.search(Query::default()).await.engines[0].status,
            EngineStatus::Timeout
        );
        assert_eq!(
            pool.search(Query::default()).await.engines[0].status,
            EngineStatus::Ok
        );
        pool.search(Query::default()).await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// Fusion keeps every distinct passage and separates update from publication.
    #[test]
    fn fusion_preserves_multiple_snippets_and_updated() {
        let mut a = ranked("https://example.com", "a", 1);
        a.link.snippet = Some("one".into());
        a.link.snippets = vec!["one".into(), "two".into()];
        let mut b = ranked("https://example.com", "b", 1);
        b.link.snippet = Some("three".into());
        b.link.snippets = vec!["two".into(), "four".into()];
        b.link.updated = Some("2024-03-01".into());
        let result = fuse(&[a, b], 10);
        assert_eq!(result[0].snippets, ["one", "two", "three", "four"]);
        assert_eq!(result[0].updated.as_deref(), Some("2024-03-01"));
        assert!(result[0].published.is_none());
    }
}
