//! Reading, merging, and validating configuration.

use std::path::PathBuf;

use super::Config;

/// A configuration problem an operator must fix. It carries a message, not a
/// source error, because every cause is a file the operator owns.
#[derive(Debug)]
pub struct ConfigError(String);

impl ConfigError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        ConfigError(message.into())
    }
    /// The operator-facing message.
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// Load configuration from `path` (or the configured default), apply defaults
/// and environment overrides, and validate the result.
pub fn load(path: Option<PathBuf>) -> Result<Config, ConfigError> {
    let explicit = path.or_else(|| std::env::var_os("CONFIG").map(PathBuf::from));
    let home = home()?;
    let mut config = read(
        Some(explicit.clone().unwrap_or(home.join("settings.json"))),
        explicit.is_some(),
    )?;
    config.home = home;
    apply_env(&mut config);
    config.validate()?;
    Ok(config)
}

/// Only an absent implicit default file means built-in defaults; parse errors
/// report locations without quoting potentially sensitive setting values.
fn read(path: Option<PathBuf>, explicit: bool) -> Result<Config, ConfigError> {
    let Some(path) = path else {
        return Ok(Config::default());
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if !explicit && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Config::default())
        }
        Err(error) => {
            return Err(ConfigError::new(format!(
                "read {}: {error}",
                path.display()
            )))
        }
    };
    let mut deserializer = serde_json::Deserializer::from_str(&text);
    let config = serde_path_to_error::deserialize(&mut deserializer).map_err(|error| {
        let field = super::engines::safe_field_path(error.path());
        ConfigError::new(format!(
            "parse {}: invalid settings at line {}, column {} (field {field})",
            path.display(),
            error.inner().line(),
            error.inner().column()
        ))
    })?;
    deserializer.end().map_err(|error| {
        ConfigError::new(format!(
            "parse {}: trailing data at line {}, column {}",
            path.display(),
            error.line(),
            error.column()
        ))
    })?;
    Ok(config)
}

/// Expand only a leading home component, not shell variables or other users' homes.
fn expand_home(path: &std::path::Path, home: Option<PathBuf>) -> Result<PathBuf, ConfigError> {
    match path.strip_prefix("~") {
        Ok(rest) => home
            .map(|home| home.join(rest))
            .ok_or_else(|| ConfigError::new("path uses ~ but the user home is not set")),
        Err(_) => Ok(path.to_path_buf()),
    }
}

/// Resolve Search's root without consulting the working directory or settings path.
pub fn home() -> Result<PathBuf, ConfigError> {
    resolve_home(std::env::var_os("SEARCH_HOME"), user_home())
}

/// Windows uses its native user profile even when Git Bash supplies a Unix-style HOME.
fn user_home() -> Option<std::ffi::OsString> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").filter(|value| !value.is_empty())
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
    }
}

/// The implicit settings file belongs to Search home; CONFIG is handled by load.
pub fn settings_path() -> Result<PathBuf, ConfigError> {
    Ok(home()?.join("settings.json"))
}

/// Resolve explicit or conventional home, failing closed for absent/relative roots.
fn resolve_home(
    search_home: Option<std::ffi::OsString>,
    user_home: Option<std::ffi::OsString>,
) -> Result<PathBuf, ConfigError> {
    let path = match search_home {
        Some(value) if value.is_empty() => {
            return Err(ConfigError::new("SEARCH_HOME must not be empty"))
        }
        Some(value) => expand_home(&PathBuf::from(value), user_home.map(PathBuf::from))?,
        None => {
            PathBuf::from(user_home.filter(|value| !value.is_empty()).ok_or_else(|| {
                ConfigError::new("set SEARCH_HOME to an absolute path or set the user home (HOME on Unix, USERPROFILE on Windows)")
            })?)
            .join(".search")
        }
    };
    if !path.is_absolute() {
        return Err(ConfigError::new(
            "SEARCH_HOME/user home must resolve to an absolute path",
        ));
    }
    Ok(path)
}

/// ADDRESS overrides the listener configured in the settings file.
fn apply_env(config: &mut Config) {
    if let Ok(address) = std::env::var("ADDRESS") {
        if !address.is_empty() {
            config.address = address;
        }
    }
}

impl Config {
    /// Whether the configured address is loopback. Accepts `host:port`, a bare
    /// `:port` (which binds every interface and is therefore NOT loopback), and
    /// `localhost:port`.
    pub fn is_loopback(&self) -> bool {
        match self.address.rsplit_once(':') {
            Some((host, _)) => match host {
                "" => false,
                "localhost" => true,
                host => host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<std::net::IpAddr>()
                    .map(|ip| ip.is_loopback())
                    // A hostname that is not an IP literal: treat as non-loopback,
                    // the frontend must secure every listener.
                    .unwrap_or(false),
            },
            None => false,
        }
    }

    /// Reject malformed listeners, unusable bounds and malformed engine selection;
    /// engines themselves resolve when a pool opens.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.home.is_absolute() {
            return Err(ConfigError::new(
                "home must be absolute; set SEARCH_HOME or the user home",
            ));
        }
        if !valid_address(&self.address) {
            return Err(ConfigError::new("address must be host:port"));
        }
        if self.fetch.max_concurrency > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(ConfigError::new(
                "fetch.max_concurrency exceeds runtime capacity",
            ));
        }
        for (name, value) in [
            ("search.max_results", self.search.max_results as u64),
            ("search.engine_timeout", self.search.engine_timeout),
            ("search.timeout", self.search.timeout),
            ("fetch.timeout", self.fetch.timeout),
            ("fetch.max_response_bytes", self.fetch.max_response_bytes),
            ("fetch.max_concurrency", self.fetch.max_concurrency as u64),
            ("remote.timeout", self.remote.timeout),
            (
                "remote.max_response_bytes",
                self.remote.max_response_bytes as u64,
            ),
        ] {
            if value == 0 {
                return Err(ConfigError::new(format!(
                    "{name} must be greater than zero"
                )));
            }
        }
        self.engines.validate().map_err(ConfigError::new)?;
        self.validate_machine()?;
        Ok(())
    }
}

/// Whether `address` is a bindable `host:port`, accepting an empty host (`:8642`)
/// and a bracketed IPv6 literal.
fn valid_address(address: &str) -> bool {
    let Some((host, port)) = address.rsplit_once(':') else {
        return false;
    };
    if port.is_empty() || port.parse::<u16>().is_err() {
        return false;
    }
    if host.is_empty() || host == "localhost" {
        return true;
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<std::net::IpAddr>().is_ok() || is_hostname(bare)
}

/// A conservative hostname check: letters, digits, dots, and hyphens only. A
/// name that passes still resolves at bind time; this only keeps validation from
/// rejecting a legitimate name like `search.internal.example`.
fn is_hostname(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Invalid concurrency is a configuration error, not a semaphore panic when
    /// the new shared fetch/extraction scheduler is constructed.
    #[test]
    fn excessive_fetch_concurrency_is_rejected_before_open() {
        let mut config = Config {
            home: std::env::temp_dir().canonicalize().unwrap(),
            ..Config::default()
        };
        config.fetch.max_concurrency = tokio::sync::Semaphore::MAX_PERMITS + 1;
        assert_eq!(
            config.validate().unwrap_err().message(),
            "fetch.max_concurrency exceeds runtime capacity"
        );
    }

    #[test]
    fn parse_diagnostics_identify_fields_without_quoting_values() {
        let path = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("search-invalid-{}", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            r#"{"fetch":{"max_response_bytes":"credential-secret"}}"#,
        )
        .unwrap();
        let error = read(Some(path.clone()), true).unwrap_err().to_string();
        std::fs::remove_file(path).unwrap();
        assert!(error.contains("invalid settings at line"));
        assert!(!error.contains("credential-secret"));
        assert!(error.contains("fetch.max_response_bytes"));
    }

    #[test]
    fn explicit_missing_settings_fail_instead_of_enabling_defaults() {
        let path = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("search-missing-{}", uuid::Uuid::new_v4()));
        assert!(read(Some(path.clone()), true).is_err());
        assert!(read(Some(path), false).is_ok());
    }

    #[test]
    fn home_expansion_is_limited_to_a_leading_component() {
        let home = Some(PathBuf::from("/home/example"));
        assert_eq!(
            expand_home(std::path::Path::new("~/.local/share/search"), home.clone()).unwrap(),
            PathBuf::from("/home/example/.local/share/search")
        );
        assert_eq!(
            expand_home(std::path::Path::new("~other/search"), home).unwrap(),
            PathBuf::from("~other/search")
        );
        assert!(expand_home(std::path::Path::new("~/search"), None).is_err());
    }

    #[test]
    fn explicit_zero_switches_survive_deserialization() {
        let config: Config =
            serde_json::from_str(r#"{"fetch":{"cache_ttl":0,"max_redirects":0}}"#).unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(config.fetch.cache_ttl, 0);
        assert_eq!(config.fetch.max_redirects, 0);
        let bad: Config = serde_json::from_str(r#"{"fetch":{"timeout":0}}"#).unwrap();
        assert!(bad.validate().is_err());
    }

    /// The parsed loopback check must not mistake `:8642` — which binds every
    /// interface — for loopback.
    #[test]
    fn all_interfaces_is_not_loopback() {
        for address in [":8642", "0.0.0.0:8642", "[::]:8642", "100.1.2.3:8642"] {
            let config = Config {
                address: address.into(),
                ..Config::default()
            };
            assert!(!config.is_loopback(), "{address} should not be loopback");
        }
        for address in ["127.0.0.1:8642", "[::1]:8642", "localhost:8642"] {
            let config = Config {
                address: address.into(),
                ..Config::default()
            };
            assert!(config.is_loopback(), "{address} should be loopback");
        }
    }

    #[test]
    fn malformed_address_is_rejected() {
        let config = Config {
            address: "not-an-address".into(),
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn settings_json_uses_clean_names() {
        let config = serde_json::from_str::<Config>(
            r#"{"notes":{"search.timeout":"milliseconds"},"search":{"timeout":5000},"engines":{"use":["mwmbl"]}}"#,
        )
        .ok();
        assert_eq!(
            config.map(|c| (c.search.timeout, c.engines.use_engines,)),
            Some((5000, vec!["mwmbl".into()]))
        );
    }

    #[test]
    fn live_engines_can_be_disabled_and_remote_timeout_is_configurable() {
        let config: Config =
            serde_json::from_str(r#"{"engines":{"enabled":false},"remote":{"timeout":45000}}"#)
                .unwrap();
        assert!(!config.engines.enabled);
        assert_eq!(config.remote.timeout(), std::time::Duration::from_secs(45));
        assert!(config.validate().is_ok());
    }
    #[test]
    fn home_resolution_is_explicit_absolute_and_fails_closed() {
        let root = std::env::temp_dir().canonicalize().unwrap();
        let user = Some(root.join("operator").into_os_string());
        assert_eq!(
            resolve_home(None, user.clone()).unwrap(),
            root.join("operator/.search")
        );
        assert_eq!(
            resolve_home(Some("~/private-search".into()), user.clone()).unwrap(),
            root.join("operator/private-search")
        );
        assert_eq!(
            resolve_home(Some(root.join("search").into_os_string()), None).unwrap(),
            root.join("search")
        );
        for root in ["", "relative", "~other/search"] {
            assert!(resolve_home(Some(root.into()), user.clone()).is_err());
        }
        assert!(resolve_home(None, None).is_err());
        assert!(resolve_home(None, Some("relative".into())).is_err());
        assert!(resolve_home(Some("~/search".into()), None).is_err());
    }

    #[test]
    fn explicit_settings_file_does_not_relocate_home() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("search-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("settings.json");
        std::fs::write(&path, "{}").unwrap();
        let config = load(Some(path)).unwrap();
        assert_eq!(config.home, home().unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }
    /// Machine settings reject unusable ceilings while zero depth/delay remain useful.
    #[test]
    fn machine_bounds_and_processor_configuration_validate() {
        for (section, field) in [
            ("research", "sources"),
            ("research", "source_characters"),
            ("research", "total_characters"),
            ("research", "max_per_host"),
            ("research", "max_attempts"),
            ("research", "timeout"),
            ("research", "session_entries"),
            ("research", "read_characters"),
            ("crawl", "max_pages"),
            ("crawl", "max_bytes"),
            ("crawl", "timeout"),
            ("processing", "timeout"),
            ("processing", "max_output_bytes"),
        ] {
            let mut input = serde_json::json!({});
            input[section] = serde_json::json!({field: 0});
            let config: Config = serde_json::from_value(input).unwrap();
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(&format!("{section}.{field}")), "{error}");
        }
        let config: Config = serde_json::from_value(
            serde_json::json!({"crawl":{"max_depth":0,"delay":0}, "research":{"session_ttl":0}}),
        )
        .unwrap();
        config.validate().unwrap();
        let config: Config = serde_json::from_value(
            serde_json::json!({"processing":{"reranker":{"command":"./secret"}}}),
        )
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("processing.reranker"));
        assert!(!error.contains("secret"));
    }
}
