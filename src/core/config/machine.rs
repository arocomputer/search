//! Operator defaults and ceilings for evidence assembly, collection reading,
//! and explicitly configured edge processing. Durations are milliseconds.

use super::{Adapter, Command, Config, ConfigError};

/// Defaults for research calls and bounded machine reads. Request options may
/// lower these budgets; the research module applies shared safety ceilings.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResearchSettings {
    pub sources: usize,
    pub source_characters: usize,
    pub total_characters: usize,
    pub max_per_host: usize,
    pub max_attempts: usize,
    pub timeout: u64,
    pub session_ttl: u64,
    pub session_entries: usize,
    pub read_characters: usize,
}

impl Default for ResearchSettings {
    fn default() -> Self {
        Self {
            sources: 5,
            source_characters: 4_000,
            total_characters: 12_000,
            max_per_host: 2,
            max_attempts: 20,
            timeout: 30_000,
            session_ttl: 600_000,
            session_entries: 128,
            read_characters: 6_000,
        }
    }
}

/// Operator ceilings on a same-origin collection traversal. Missing robots
/// files permit traversal; failed or unreadable robots files do not.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CrawlSettings {
    pub max_pages: usize,
    pub max_depth: usize,
    pub max_bytes: u64,
    pub timeout: u64,
    pub delay: u64,
    /// Bounded shared politeness state across concurrent collection calls.
    pub max_hosts: usize,
    pub host_ttl: u64,
}

impl Default for CrawlSettings {
    fn default() -> Self {
        Self {
            max_pages: 20,
            max_depth: 2,
            max_bytes: 16 << 20,
            timeout: 30_000,
            delay: 100,
            max_hosts: 128,
            host_ttl: 600_000,
        }
    }
}

/// Optional trusted programs at the client edge. No processing program is
/// enabled by default, and research never executes one without explicit opt-in.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProcessingSettings {
    pub reranker: Option<Command>,
    pub synthesizer: Option<Command>,
    pub timeout: u64,
    pub max_output_bytes: u64,
    /// Shared child-process admission across all clients of this service.
    pub max_concurrency: usize,
}

impl Default for ProcessingSettings {
    fn default() -> Self {
        Self {
            reranker: None,
            synthesizer: None,
            timeout: 15_000,
            max_output_bytes: 1 << 20,
            max_concurrency: 2,
        }
    }
}

impl Config {
    /// Validate machine budgets and optional programs without executing them or
    /// accessing their credentials. Zero cache/session TTL disables reuse.
    pub(super) fn validate_machine(&self) -> Result<(), ConfigError> {
        if self.processing.max_concurrency > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(ConfigError::new(
                "processing.max_concurrency exceeds runtime capacity",
            ));
        }
        if self.research.session_entries > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(ConfigError::new(
                "research.session_entries exceeds transport capacity",
            ));
        }
        for (name, milliseconds) in [
            ("research.timeout", self.research.timeout),
            ("crawl.timeout", self.crawl.timeout),
            ("crawl.delay", self.crawl.delay),
            ("processing.timeout", self.processing.timeout),
        ] {
            if std::time::Instant::now()
                .checked_add(std::time::Duration::from_millis(milliseconds))
                .is_none()
            {
                return Err(ConfigError::new(format!(
                    "{name} exceeds the supported clock range"
                )));
            }
        }
        for (name, value) in [
            ("research.sources", self.research.sources as u64),
            (
                "research.source_characters",
                self.research.source_characters as u64,
            ),
            (
                "research.total_characters",
                self.research.total_characters as u64,
            ),
            ("research.max_per_host", self.research.max_per_host as u64),
            ("research.max_attempts", self.research.max_attempts as u64),
            ("research.timeout", self.research.timeout),
            (
                "research.session_entries",
                self.research.session_entries as u64,
            ),
            (
                "research.read_characters",
                self.research.read_characters as u64,
            ),
            ("crawl.max_pages", self.crawl.max_pages as u64),
            ("crawl.max_bytes", self.crawl.max_bytes),
            ("crawl.timeout", self.crawl.timeout),
            ("crawl.max_hosts", self.crawl.max_hosts as u64),
            ("processing.timeout", self.processing.timeout),
            (
                "processing.max_output_bytes",
                self.processing.max_output_bytes,
            ),
            (
                "processing.max_concurrency",
                self.processing.max_concurrency as u64,
            ),
        ] {
            if value == 0 {
                return Err(ConfigError::new(format!(
                    "{name} must be greater than zero"
                )));
            }
        }
        for (name, command) in [
            ("processing.reranker", self.processing.reranker.as_ref()),
            (
                "processing.synthesizer",
                self.processing.synthesizer.as_ref(),
            ),
        ] {
            if let Some(command) = command {
                Adapter::Command(command.clone())
                    .validate()
                    .map_err(|error| ConfigError::new(format!("{name}: {error}")))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Processing remains opt-in even when its object is explicitly present.
    #[test]
    fn omitted_processors_never_run() {
        let settings: ProcessingSettings = serde_json::from_str("{}").unwrap();
        assert!(settings.reranker.is_none());
        assert!(settings.synthesizer.is_none());
    }

    /// Invalid machine bounds fail at startup rather than silently resolving to
    /// another value. A zero traversal depth is a legitimate root-only crawl.
    #[test]
    fn machine_bounds_are_validated_as_operator_settings() {
        for settings in [
            r#"{"research":{"max_attempts":0}}"#,
            r#"{"crawl":{"max_bytes":0}}"#,
            r#"{"processing":{"timeout":0}}"#,
            r#"{"processing":{"synthesizer":{"command":"./untrusted"}}}"#,
        ] {
            let config: Config = serde_json::from_str(settings).unwrap();
            assert!(config.validate_machine().is_err(), "{settings}");
        }
        let config: Config = serde_json::from_str(r#"{"crawl":{"max_depth":0}}"#).unwrap();
        assert!(config.validate_machine().is_ok());
    }
}
