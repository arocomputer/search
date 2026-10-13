//! The service's settings: its home and listener, how long a query may run,
//! and which search engines are enabled and how each is defined.
//!
//! Omitted fields use safe defaults; explicit values are validated, not replaced.
//! Credential values stay in the environment or frontend storage; settings
//! reference only the variable names needed by engine adapters. Settings are
//! trusted operator configuration: a command engine entry runs an executable.

mod defaults;
pub(crate) mod engines;
mod load;
mod machine;
mod settings;

pub use machine::{CrawlSettings, ProcessingSettings, ResearchSettings};

pub use engines::{valid_engine_id, Preset, DEFAULT_ENGINE, PRESETS};
pub use settings::{
    Adapter, Command, Config, EngineSettings, FetchSettings, Http, RemoteSettings, SearchSettings,
    DEFAULT_ADDRESS,
};

/// Load and validate configuration from `path`, falling back to `CONFIG`
/// and then home/settings.json. Environment overrides are applied last.
pub use load::load;
pub use load::{home, settings_path, ConfigError};
