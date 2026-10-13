//! The configuration shape and its invariants.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// The default listener: loopback only. Exposing search beyond loopback is a
/// deliberate act; the frontend provides paired HTTPS authentication.
pub const DEFAULT_ADDRESS: &str = "127.0.0.1:8642";

/// Inert service configuration; opening a pool resolves engines and runtime resources.
/// Settings are trusted operator configuration: they can define executables.
/// Duration values are milliseconds.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Optional human explanations, ignored by the engine.
    pub notes: BTreeMap<String, String>,
    /// Listen address, `host:port`.
    pub address: String,
    /// Search home (settings and trust), resolved independently
    /// of the settings file.
    #[serde(skip)]
    pub home: PathBuf,
    /// The user agent sent by page fetching and HTTP engines.
    pub user_agent: String,
    pub search: SearchSettings,
    pub fetch: FetchSettings,
    pub engines: EngineSettings,
    pub remote: RemoteSettings,
    pub research: super::ResearchSettings,
    pub crawl: super::CrawlSettings,
    pub processing: super::ProcessingSettings,
}

/// Bounds on one search query. Milliseconds on disk; durations in memory.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchSettings {
    /// Results returned at most.
    pub max_results: usize,
    /// Milliseconds one search engine may take before it is abandoned.
    pub engine_timeout: u64,
    /// Milliseconds successful answers remain reusable; zero disables caching.
    pub cache_ttl: u64,
    /// Maximum serialized cache payload plus keys; zero disables caching.
    pub cache_bytes: usize,
    /// Milliseconds allowed for the whole query.
    pub timeout: u64,
}

/// Bounds on the fetcher.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FetchSettings {
    pub timeout: u64,
    /// Maximum bytes read from any one response.
    pub max_response_bytes: u64,
    /// The most redirects followed, each re-checked.
    pub max_redirects: usize,
    /// Milliseconds a fetched page is reused; zero disables storage.
    pub cache_ttl: u64,
    /// Approximate cached payload and entry bytes; zero disables storage.
    pub cache_bytes: usize,
    /// Refuse only for tests and air-gapped mirrors: disabling the guard makes
    /// the fetcher able to reach private addresses.
    pub allow_private_networks: bool,
    /// How many fetches run at once.
    pub max_concurrency: usize,
}

impl SearchSettings {
    /// Use the configured maximum for a zero request and cap every explicit request.
    pub fn result_limit(&self, requested: usize) -> usize {
        let maximum = self.max_results;
        if requested == 0 {
            maximum
        } else {
            requested.min(maximum)
        }
    }
    pub fn engine_timeout(&self) -> Duration {
        Duration::from_millis(self.engine_timeout)
    }
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout)
    }
}

impl FetchSettings {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout)
    }
    pub fn cache_ttl(&self) -> Duration {
        Duration::from_millis(self.cache_ttl)
    }
}

/// Which engines run, and how each is defined.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineSettings {
    /// Enable live web engines. False returns no search results.
    pub enabled: bool,
    /// Selected engine IDs; an empty list disables all live engines.
    #[serde(rename = "use")]
    pub use_engines: Vec<String>,
    /// Per-engine adapter objects: shallow overrides of a built-in preset, or a
    /// complete `http`/`command` adapter for any other ID.
    pub config: BTreeMap<String, serde_json::Value>,
}

/// Limits on calls made to the selected paired host.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteSettings {
    /// Overall request deadline in milliseconds.
    pub timeout: u64,
    /// Maximum JSON response bytes from the paired host.
    pub max_response_bytes: usize,
}

impl RemoteSettings {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout)
    }
}

/// A configured engine adapter. Credentials are resolved only at query time.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)] // Inert operator settings keep direct, ergonomic transport fields.
pub enum Adapter {
    Http(Http),
    Command(Command),
}

/// GET-only JSON search, with independent private-network permission.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Http {
    pub url: String,
    #[serde(default = "default_query_param")]
    pub query_param: String,
    #[serde(default)]
    pub limit_param: Option<String>,
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    /// Filter field to query parameter; lists become repeated parameters.
    #[serde(default)]
    pub filter_params: BTreeMap<String, String>,
    /// Upper bound on a Retry-After delay in milliseconds; zero disables retries.
    #[serde(default = "default_retry_after")]
    pub retry_after_max: u64,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Environment variables supply whole header values, without interpolation.
    #[serde(default)]
    pub header_env: BTreeMap<String, String>,
    #[serde(default = "default_results_pointer")]
    pub results_pointer: String,
    #[serde(default = "default_title_pointer")]
    pub title_pointer: String,
    #[serde(default = "default_url_pointer")]
    pub url_pointer: String,
    #[serde(default = "default_snippet_pointer")]
    pub snippet_pointer: String,
    /// Optional publication/last-updated date field; omitted when the engine
    /// reports no date.
    #[serde(default = "default_published_pointer")]
    pub published_pointer: String,
    #[serde(default = "default_snippets_pointer")]
    pub snippets_pointer: String,
    #[serde(default = "default_updated_pointer")]
    pub updated_pointer: String,
    /// Join array-valued titles/snippets by this per-part JSON pointer, when configured.
    #[serde(default)]
    pub text_part_pointer: Option<String>,
    #[serde(default = "default_adapter_cap")]
    pub max_response_bytes: u64,
    #[serde(default)]
    pub allow_private_networks: bool,
}

/// A directly spawned executable speaking one JSON request and response per query.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    /// An absolute executable path, or a bare name looked up on `PATH`.
    pub command: String,
    /// Protocol 2 carries filters; select 1 explicitly for legacy executables.
    #[serde(default = "default_command_version")]
    pub version: u8,
    /// JSON data passed in the request, never interpolated into arguments.
    #[serde(default)]
    pub config: BTreeMap<String, serde_json::Value>,
    /// Explicitly inherited credential environment variable names.
    #[serde(default)]
    pub env: Vec<String>,
    /// Absolute working directory; resolution defaults it to the command's
    /// parent directory when `command` is absolute, otherwise private scratch.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Private scratch directory exported as TMPDIR, TMP, and TEMP to the child.
    #[serde(default = "default_command_temp_dir")]
    pub temp_dir: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_adapter_cap")]
    pub max_response_bytes: u64,
}

/// Group command-engine scratch files beneath the canonical OS temporary
/// directory, in a per-user directory so shared hosts cannot collide or lock
/// each other out of it.
fn default_command_temp_dir() -> PathBuf {
    let base = std::env::temp_dir();
    let base = base.canonicalize().unwrap_or(base);
    #[cfg(unix)]
    {
        // SAFETY: geteuid takes no arguments and does not dereference memory.
        let uid = unsafe { libc::geteuid() };
        base.join(format!("search-{uid}"))
    }
    #[cfg(not(unix))]
    {
        base.join("search")
    }
}

/// The default bound on adapter output, independent of page-fetch limits.
fn default_adapter_cap() -> u64 {
    1 << 20
}
/// Default query parameter and JSON locations match the command response shape.
fn default_query_param() -> String {
    "q".into()
}
/// Locate the array in the response object.
fn default_results_pointer() -> String {
    "/results".into()
}
/// Locate a row's title.
fn default_title_pointer() -> String {
    "/title".into()
}
/// Locate a row's URL.
fn default_url_pointer() -> String {
    "/url".into()
}
/// Locate a row's optional snippet.
fn default_snippet_pointer() -> String {
    "/snippet".into()
}
/// Locate a row's optional publication or last-updated date.
fn default_published_pointer() -> String {
    "/published".into()
}

/// Current filter-aware command protocol.
fn default_command_version() -> u8 {
    2
}
/// Maximum upstream-requested pause before one retry.
fn default_retry_after() -> u64 {
    1000
}
/// Ordered upstream passages.
fn default_snippets_pointer() -> String {
    "/snippets".into()
}
/// Last update metadata.
fn default_updated_pointer() -> String {
    "/updated".into()
}
