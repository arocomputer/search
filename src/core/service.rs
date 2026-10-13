//! The in-process search engine over live engines and guarded fetching, and
//! the query and answer types every frontend exchanges with it.

use crate::core::config::Config;
use crate::core::engines::{self, EngineState};
use crate::core::fetch::{Fetcher, Page};

/// One discovered page, normalized across engines.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Link {
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Distinct passages supplied by engines, preserved through fusion.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snippets: Vec<String>,
    /// Last update reported upstream, separate from publication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
    /// The publication date as the engine reports it, verbatim,
    /// for grounding. `None` when the engine gives no date.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    /// Every engine that returned this URL, for transparency. A list, not a
    /// joined string: a display concern must not decide a dedup comparison.
    pub engines: Vec<String>,
    /// Reciprocal-rank-fusion score; higher is better.
    pub score: f64,
}

/// Requested upstream constraints; unsupported fields are reported per engine.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueryFilters {
    pub freshness: Option<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub include_domains: Vec<String>,
    pub exclude_domains: Vec<String>,
    pub language: Option<String>,
    pub region: Option<String>,
    pub category: Option<String>,
    pub safe: Option<bool>,
    pub exact_phrase: Option<String>,
}

impl QueryFilters {
    /// Present constraints as stable field/value pairs for adapter mappings.
    pub fn values(&self) -> std::collections::BTreeMap<String, Vec<String>> {
        let mut out = std::collections::BTreeMap::new();
        for (name, value) in [
            ("freshness", &self.freshness),
            ("start_date", &self.start_date),
            ("end_date", &self.end_date),
            ("language", &self.language),
            ("region", &self.region),
            ("category", &self.category),
            ("exact_phrase", &self.exact_phrase),
        ] {
            if let Some(value) = value {
                out.insert(name.into(), vec![value.clone()]);
            }
        }
        for (name, values) in [
            ("include_domains", &self.include_domains),
            ("exclude_domains", &self.exclude_domains),
        ] {
            if !values.is_empty() {
                out.insert(name.into(), values.clone());
            }
        }
        if let Some(value) = self.safe {
            out.insert("safe".into(), vec![value.to_string()]);
        }
        out
    }

    /// Validate bounded text, DNS names, and real ISO calendar dates without echoing input.
    pub fn validate(&self) -> Result<(), &'static str> {
        let text = [
            &self.freshness,
            &self.start_date,
            &self.end_date,
            &self.language,
            &self.region,
            &self.category,
            &self.exact_phrase,
        ];
        if text
            .into_iter()
            .flatten()
            .any(|v| v.trim().is_empty() || v.len() > 2048 || v.chars().any(char::is_control))
        {
            return Err(
                "filter values must be nonempty text of at most 2048 bytes without controls",
            );
        }
        for domains in [&self.include_domains, &self.exclude_domains] {
            if domains.len() > 32 {
                return Err("provide at most 32 domains per filter");
            }
            if domains.iter().any(|v| !valid_domain(v)) {
                return Err(
                    "domain filters must contain DNS names without schemes, ports or paths",
                );
            }
        }
        for date in [&self.start_date, &self.end_date].into_iter().flatten() {
            if !valid_date(date) {
                return Err("date filters must be valid YYYY-MM-DD calendar dates");
            }
        }
        if let (Some(start), Some(end)) = (&self.start_date, &self.end_date) {
            if start > end {
                return Err("start_date must not follow end_date");
            }
        }
        Ok(())
    }
}

/// Accept DNS labels only; URL parsing alone also accepts paths and credentials.
fn valid_domain(value: &str) -> bool {
    value.len() <= 253
        && value.contains('.')
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        && value.parse::<std::net::IpAddr>().is_err()
}

/// Check Gregorian calendar dates, including leap years; lexical order then equals date order.
fn valid_date(value: &str) -> bool {
    let mut parts = value.split('-');
    let (Some(y), Some(m), Some(d), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if y.len() != 4
        || m.len() != 2
        || d.len() != 2
        || ![y, m, d]
            .iter()
            .all(|v| v.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    let (Ok(y), Ok(m), Ok(d)) = (y.parse::<u32>(), m.parse::<u32>(), d.parse::<u32>()) else {
        return false;
    };
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    };
    y > 0 && d > 0 && d <= days
}

/// The search text, result limit, and optional engine selection.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Query {
    pub text: String,
    pub limit: usize,
    /// Restrict to these engine names; empty means every enabled engine.
    pub engines: Vec<String>,
    #[serde(default)]
    pub filters: QueryFilters,
}

/// The normalized answer the caller receives.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Answer {
    pub query: String,
    pub results: Vec<Link>,
    pub engines: Vec<EngineState>,
    pub duration_ms: u64,
}

/// The configured search engine used in-process by applications and adapters.
pub struct Search {
    pub(super) pool: engines::Pool,
    pub(super) fetcher: Fetcher,
    config: Config,
    pub(super) research_sessions: tokio::sync::Mutex<crate::core::research::SessionStore>,
    /// Admission shared by edge processors; core never starts a processor.
    pub(crate) processing_permits: tokio::sync::Semaphore,
}

impl Search {
    /// Resolve the selected engines from settings and build live engines and
    /// fetching. Opening writes nothing to disk.
    pub fn open(config: Config) -> Result<Self, SearchError> {
        let pool = engines::Pool::new(&config)
            .map_err(|error| SearchError::Settings(error.to_string()))?;
        let fetcher =
            Fetcher::new(config.fetch.clone(), &config.user_agent).map_err(SearchError::Client)?;
        Ok(Search {
            processing_permits: tokio::sync::Semaphore::new(config.processing.max_concurrency),
            pool,
            fetcher,
            research_sessions: tokio::sync::Mutex::new(
                crate::core::research::SessionStore::default(),
            ),
            config,
        })
    }

    /// Discover live results across the selected engines.
    pub async fn search(&self, query: Query) -> Answer {
        self.pool.search(query).await
    }

    /// Fetch one or more URLs, preserving order and caching recent answers in memory.
    pub async fn fetch(&self, urls: &[String]) -> Vec<Page> {
        self.fetcher.fetch_many(urls).await
    }

    /// The enabled engine names, for status output.
    pub fn engine_names(&self) -> Vec<String> {
        self.pool.names()
    }

    /// The running configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The configured deadline for a complete engine query.
    pub fn overall_timeout(&self) -> std::time::Duration {
        self.pool.overall_timeout()
    }
}

/// Why the search engine could not start.
#[derive(Debug)]
pub enum SearchError {
    Settings(String),
    Client(reqwest::Error),
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchError::Settings(error) => write!(f, "settings: {error}"),
            SearchError::Client(error) => write!(f, "build guarded HTTP client: {error}"),
        }
    }
}

impl std::error::Error for SearchError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wire names track engines while rankings and status values remain stable.
    #[test]
    fn query_and_answer_use_engine_fields() {
        let query: Query = serde_json::from_value(serde_json::json!({
            "text": "query", "limit": 3, "engines": ["fixture"]
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(query).unwrap(),
            serde_json::json!({"text": "query", "limit": 3, "engines": ["fixture"], "filters": serde_json::to_value(QueryFilters::default()).unwrap()})
        );
        let state = |skipped| EngineState {
            name: "fixture".into(),
            status: engines::EngineStatus::Ok,
            count: 1,
            skipped,
            unsupported_filters: Vec::new(),
            error: None,
            elapsed_ms: 1,
        };
        let answer = Answer {
            query: "query".into(),
            results: vec![Link {
                title: "Title".into(),
                url: "https://example.com/".into(),
                snippet: None,
                snippets: Vec::new(),
                updated: None,
                published: None,
                engines: vec!["fixture".into()],
                score: 1.0 / 61.0,
            }],
            engines: vec![state(0), state(2)],
            duration_ms: 1,
        };
        assert_eq!(
            serde_json::to_value(answer).unwrap(),
            serde_json::json!({
                "query": "query", "results": [{"title":"Title", "url":"https://example.com/", "engines":["fixture"], "score":1.0/61.0}],
                "engines":[
                    {"name":"fixture", "status":"ok", "count":1, "elapsed_ms":1},
                    {"name":"fixture", "status":"ok", "count":1, "skipped":2, "elapsed_ms":1}
                ],
                "duration_ms":1
            })
        );
    }

    /// Opening the default engines must not create anything on disk.
    #[test]
    fn default_open_leaves_disk_untouched() {
        let dir = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("search-diskless-{}", uuid::Uuid::new_v4()));
        let config = Config {
            home: dir.join("home"),
            ..Default::default()
        };
        let service = Search::open(config).unwrap();
        assert_eq!(
            service.engine_names(),
            [crate::core::config::DEFAULT_ENGINE]
        );
        assert!(!dir.exists());
    }

    /// A selected engine without settings fails by name instead of being skipped.
    #[test]
    fn unconfigured_selected_engine_fails_open() {
        let mut config = Config::default();
        config.engines.use_engines = vec!["missing-fixture".into()];
        let error = Search::open(config).err().unwrap().to_string();
        assert!(error.contains("engine missing-fixture"), "{error}");
        assert!(error.contains("engines.config.missing-fixture"), "{error}");
    }

    /// A command engine defined only in settings runs from its configured
    /// working directory and receives its settings `config` object.
    #[tokio::test]
    async fn settings_defined_command_engine_runs_from_its_cwd() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("search-settings-engine-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(
            root.join("runner.py"),
            r#"
import sys,json
request=json.load(sys.stdin)
print(json.dumps({'results':[{'title':request['config']['title'],'url':'https://example.com/'}]}))
"#,
        )
        .unwrap();
        let mut config: Config = serde_json::from_value(serde_json::json!({
            "search": {"engine_timeout": 5000},
            "engines": {"use": ["fixture"], "config": {"fixture": {
                "type": "command",
                "command": if cfg!(windows) { "python.exe" } else { "python3" },
                "args": ["runner.py"],
                "cwd": root,
                "config": {"title": "configured"}
            }}}
        }))
        .unwrap();
        config.home = root.join("home");
        let service = Search::open(config).unwrap();
        let response = service
            .search(Query {
                text: "query".into(),
                ..Default::default()
            })
            .await;
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(response.engines[0].status, engines::EngineStatus::Ok);
        assert_eq!(response.results[0].title, "configured");
        assert_eq!(response.results[0].engines, ["fixture"]);
    }

    /// Invalid constraints fail before any engine work; Gregorian dates include leap years.
    #[test]
    fn filters_validate_bounds_domains_and_calendar_dates() {
        for valid in ["2000-02-29", "2024-02-29", "2024-12-31"] {
            assert!(QueryFilters {
                start_date: Some(valid.into()),
                ..Default::default()
            }
            .validate()
            .is_ok());
        }
        for invalid in [
            "1900-02-29",
            "2023-02-29",
            "2024-04-31",
            "2024-00-01",
            "2024-1-01",
            "0000-01-01",
        ] {
            assert!(
                QueryFilters {
                    start_date: Some(invalid.into()),
                    ..Default::default()
                }
                .validate()
                .is_err(),
                "{invalid}"
            );
        }
        for invalid in [
            "https://example.com",
            "example.com/path",
            "example.com:80",
            "a..com",
            "-a.com",
            "127.0.0.1",
        ] {
            assert!(
                QueryFilters {
                    include_domains: vec![invalid.into()],
                    ..Default::default()
                }
                .validate()
                .is_err(),
                "{invalid}"
            );
        }
        assert!(QueryFilters {
            start_date: Some("2024-03-01".into()),
            end_date: Some("2024-02-29".into()),
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(QueryFilters {
            exact_phrase: Some("x".repeat(2049)),
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(QueryFilters {
            exclude_domains: vec!["example.com".into(); 33],
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(
            serde_json::from_value::<QueryFilters>(serde_json::json!({"unknown":true})).is_err()
        );
    }
}
