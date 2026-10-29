//! Built-in defaults used by Serde for omitted fields, never explicit zero values.

use std::time::Duration;

use super::{
    Config, EngineSettings, FetchSettings, RemoteSettings, SearchSettings, DEFAULT_ADDRESS,
};

/// Seconds a search engine may take before it is abandoned.
pub const DEFAULT_ENGINE_SECS: u64 = 5;
/// Seconds the whole fan-out may take.
pub const DEFAULT_OVERALL_SECS: u64 = 8;
/// Seconds a fetched page is reused from memory.
pub const DEFAULT_FETCH_CACHE_SECS: u64 = 600;
/// The most bytes read from one response: a memory and work bound, not a
/// content budget. Large documentation and encyclopedia pages exceed 1 MiB.
pub const DEFAULT_MAX_BYTES: u64 = 16 << 20;
/// Redirects followed.
pub const DEFAULT_MAX_REDIRECTS: usize = 5;
/// Fetches in flight at once.
pub const DEFAULT_MAX_CONCURRENCY: usize = 8;
impl Default for Config {
    fn default() -> Self {
        Config {
            notes: Default::default(),
            address: DEFAULT_ADDRESS.into(),
            home: super::home().unwrap_or_default(),
            user_agent: format!(
                "search/{} (+https://github.com/fschrhunt/search)",
                crate::VERSION
            ),
            search: SearchSettings::default(),
            fetch: FetchSettings::default(),
            engines: EngineSettings::default(),
            remote: RemoteSettings::default(),
            research: super::ResearchSettings::default(),
            crawl: super::CrawlSettings::default(),
            processing: super::ProcessingSettings::default(),
        }
    }
}

impl Default for SearchSettings {
    fn default() -> Self {
        SearchSettings {
            max_results: 10,
            cache_ttl: 60_000,
            cache_bytes: 8 << 20,
            engine_timeout: secs(DEFAULT_ENGINE_SECS),
            timeout: secs(DEFAULT_OVERALL_SECS),
        }
    }
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            use_engines: vec![super::DEFAULT_ENGINE.into()],
            config: Default::default(),
        }
    }
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self {
            timeout: secs(120),
            max_response_bytes: 64 << 20,
        }
    }
}

impl Default for FetchSettings {
    fn default() -> Self {
        FetchSettings {
            timeout: secs(15),
            max_response_bytes: DEFAULT_MAX_BYTES,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            cache_ttl: secs(DEFAULT_FETCH_CACHE_SECS),
            cache_bytes: 32 << 20,
            allow_private_networks: false,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
        }
    }
}

fn secs(n: u64) -> u64 {
    Duration::from_secs(n).as_millis() as u64
}
