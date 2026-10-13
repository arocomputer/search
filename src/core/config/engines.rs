//! Engine definitions: built-in presets, selection checks, and resolving one
//! engine ID to a validated adapter, without reading credentials or contacting
//! endpoints.
//!
//! A preset ID merges its `engines.config` entry shallowly over the compiled-in
//! adapter and may not change its `type`; any other ID must be configured with a
//! complete adapter. Errors name the engine and field, never configured values.

use std::path::Path;

use serde_json::{json, Map, Value};

use super::{Adapter, ConfigError, EngineSettings};

/// The engine selected when settings omit `engines.use`.
pub const DEFAULT_ENGINE: &str = "mwmbl";

/// A built-in engine compiled into the binary.
pub struct Preset {
    pub id: &'static str,
    pub description: &'static str,
    /// Adapter fields the operator must supply before the preset can run.
    pub required: &'static [&'static str],
    adapter: fn() -> Value,
}

/// Every built-in engine, in display order.
pub const PRESETS: &[Preset] = &[
    Preset {
        id: "mwmbl",
        description: "Community-crawled web index; keyless API, subject to service limits",
        required: &[],
        adapter: || {
            json!({
                "type": "http",
                "url": "https://api.mwmbl.org/search/",
                "query_param": "s",
                "results_pointer": "",
                "title_pointer": "/title",
                "url_pointer": "/url",
                "snippet_pointer": "/extract",
                "text_part_pointer": "/value"
            })
        },
    },
    Preset {
        id: "searxng",
        description: "Your SearXNG instance's full /search URL; it must allow JSON output",
        required: &["url"],
        adapter: || {
            json!({
                "type": "http",
                "url": "",
                "query_param": "q",
                "params": {"format": "json"},
                "results_pointer": "/results",
                "title_pointer": "/title",
                "url_pointer": "/url",
                "snippet_pointer": "/content",
                "filter_params": {"freshness":"time_range", "language":"language", "category":"categories"},
                "published_pointer": "/publishedDate"
            })
        },
    },
];

impl Preset {
    /// The preset with this ID, if any.
    pub fn find(id: &str) -> Option<&'static Preset> {
        PRESETS.iter().find(|preset| preset.id == id)
    }

    /// The compiled-in adapter object, before any settings entry is merged.
    pub fn adapter(&self) -> Value {
        (self.adapter)()
    }
}

/// Engine IDs are bounded, lowercase identifiers.
pub fn valid_engine_id(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

impl EngineSettings {
    /// Validate selection and entry shapes only; engines are resolved when a
    /// pool opens, after remote selection.
    pub(super) fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::BTreeSet::new();
        for name in &self.use_engines {
            if !valid_engine_id(name) {
                return Err("engines.use contains an invalid ID; use lowercase letters, digits, - or _ (max 64)".into());
            }
            if !seen.insert(name) {
                return Err(format!("engines.use repeats engine {name}"));
            }
        }
        for (name, value) in &self.config {
            if !valid_engine_id(name) {
                return Err("engines.config contains an invalid engine ID".into());
            }
            if !value.is_object() {
                return Err(format!("engines.config.{name} must be an object"));
            }
        }
        Ok(())
    }

    /// The raw adapter object for `id`: its entry merged over a preset, or the
    /// entry alone for a custom engine. Checks the transport type, not fields.
    pub fn definition(&self, id: &str) -> Result<Map<String, Value>, ConfigError> {
        if !valid_engine_id(id) {
            return Err(ConfigError::new(
                "invalid engine ID; use lowercase letters, digits, - or _ (max 64)",
            ));
        }
        let error = |message: String| ConfigError::new(format!("engine {id}: {message}"));
        let entry = match self.config.get(id) {
            None => None,
            Some(value) => Some(
                value
                    .as_object()
                    .ok_or_else(|| error(format!("engines.config.{id} must be an object")))?,
            ),
        };
        let Some(preset) = Preset::find(id) else {
            let entry = entry.ok_or_else(|| {
                error(format!(
                    "not configured; define engines.config.{id} with type http or command"
                ))
            })?;
            if !matches!(
                entry.get("type").and_then(Value::as_str),
                Some("http" | "command")
            ) {
                return Err(error(format!(
                    "engines.config.{id}.type must be http or command"
                )));
            }
            return Ok(entry.clone());
        };
        let mut merged = match preset.adapter() {
            Value::Object(object) => object,
            _ => return Err(error("built-in adapter is not an object".into())),
        };
        if let Some(entry) = entry {
            if entry
                .get("type")
                .is_some_and(|value| Some(value) != merged.get("type"))
            {
                return Err(error("type cannot change for a built-in engine".into()));
            }
            merged.extend(entry.clone());
        }
        Ok(merged)
    }

    /// Resolve `id` to a complete, validated adapter. Command engines get an
    /// absolute working directory: `cwd`, else the parent of an absolute
    /// `command`, else the engine's private scratch directory, which is created
    /// before every run and so always exists.
    pub fn adapter(&self, id: &str) -> Result<Adapter, ConfigError> {
        let merged = self.definition(id)?;
        let error = |message: &str| ConfigError::new(format!("engine {id}: {message}"));
        for field in Preset::find(id).map_or(&[][..], |preset| preset.required) {
            if merged.get(*field).is_none_or(|value| match value {
                Value::Null => true,
                Value::String(value) => value.trim().is_empty(),
                Value::Array(value) => value.is_empty(),
                Value::Object(value) => value.is_empty(),
                _ => false,
            }) {
                return Err(error(&format!(
                    "required field {field} is missing; run search configure {id} {field} {}",
                    field.to_ascii_uppercase()
                )));
            }
        }
        let mut adapter: Adapter = serde_path_to_error::deserialize(Value::Object(merged))
            .map_err(|failure| {
                let field = safe_field_path(failure.path());
                error(&format!(
                    "invalid adapter field {field}; check engines.config.{id}"
                ))
            })?;
        if let Adapter::Command(command) = &mut adapter {
            if command.cwd.is_none() {
                let program = Path::new(&command.command);
                command.cwd = Some(match program.parent() {
                    Some(parent) if program.is_absolute() => parent.to_path_buf(),
                    _ => command.temp_dir.clone(),
                });
            }
        }
        adapter.validate().map_err(error)?;
        Ok(adapter)
    }
}

impl Adapter {
    /// Check a merged transport without reading credentials or running engine code.
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Adapter::Command(settings) => {
                if !matches!(settings.version, 1 | 2) {
                    return Err("version must be 1 (legacy) or 2 (filters)");
                }
                if settings.command.trim().is_empty() || settings.command.contains('\0') {
                    return Err("command must name an executable");
                }
                if !std::path::Path::new(&settings.command).is_absolute()
                    && settings.command.contains(['/', '\\'])
                {
                    return Err(
                        "command must be an absolute path or a bare executable name on PATH",
                    );
                }
                if settings.cwd.as_ref().is_some_and(|cwd| !cwd.is_absolute()) {
                    return Err("cwd must be absolute");
                }
                if settings.args.iter().any(|arg| arg.contains('\0')) {
                    return Err("args contains NUL");
                }
                if settings.env.iter().any(|name| !valid_env(name)) {
                    return Err("command env must contain valid environment variable names");
                }
                if !settings.temp_dir.is_absolute() {
                    return Err("temp_dir must be absolute");
                }
                if settings.max_response_bytes == 0 {
                    return Err("max_response_bytes must be positive");
                }
            }
            Adapter::Http(settings) => {
                if settings.query_param.is_empty()
                    || settings
                        .limit_param
                        .as_ref()
                        .is_some_and(|param| param.is_empty() || param == &settings.query_param)
                {
                    return Err("query_param and limit_param must be nonempty and distinct");
                }
                let mut params = std::collections::BTreeSet::new();
                params.insert(settings.query_param.as_str());
                if let Some(param) = &settings.limit_param {
                    params.insert(param.as_str());
                }
                for (field, param) in &settings.filter_params {
                    if ![
                        "freshness",
                        "start_date",
                        "end_date",
                        "include_domains",
                        "exclude_domains",
                        "language",
                        "region",
                        "category",
                        "safe",
                        "exact_phrase",
                    ]
                    .contains(&field.as_str())
                        || param.is_empty()
                        || param.chars().any(char::is_control)
                        || !params.insert(param.as_str())
                    {
                        return Err(
                            "filter_params requires known filters and distinct nonempty parameters",
                        );
                    }
                }
                if http_url(&settings.url).is_none() {
                    return Err("url must be http(s) without credentials or fragment");
                }
                if settings.max_response_bytes == 0 {
                    return Err("max_response_bytes must be positive");
                }
                for pointer in [
                    &settings.results_pointer,
                    &settings.title_pointer,
                    &settings.url_pointer,
                    &settings.snippet_pointer,
                    &settings.published_pointer,
                    &settings.snippets_pointer,
                    &settings.updated_pointer,
                ] {
                    if !valid_pointer(pointer) {
                        return Err("results_pointer/title_pointer/url_pointer/snippet_pointer/published_pointer/snippets_pointer/updated_pointer must be valid JSON pointers");
                    }
                }
                if settings
                    .text_part_pointer
                    .as_deref()
                    .is_some_and(|pointer| !valid_pointer(pointer))
                {
                    return Err("text_part_pointer must be a valid JSON pointer");
                }
                for name in settings.headers.keys().chain(settings.header_env.keys()) {
                    if reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err() {
                        return Err("headers/header_env must use valid HTTP header names");
                    }
                }
                for value in settings.headers.values() {
                    if reqwest::header::HeaderValue::from_str(value).is_err() {
                        return Err("headers contains an invalid HTTP header value");
                    }
                }
                for name in settings.header_env.values() {
                    if !valid_env(name) {
                        return Err("header_env must reference valid environment variable names");
                    }
                }
            }
        }
        Ok(())
    }
}

/// Parse an absolute HTTP URL with a host, no userinfo, and no fragment.
pub(crate) fn http_url(value: &str) -> Option<url::Url> {
    let url = url::Url::parse(value).ok()?;
    let authority = url
        .as_str()
        .split_once("//")?
        .1
        .split(['/', '?', '#'])
        .next()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || authority.contains('@')
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    Some(url)
}

/// RFC 6901 permits the root pointer and only ~0/~1 escapes.
fn valid_pointer(pointer: &str) -> bool {
    if !pointer.is_empty() && !pointer.starts_with('/') {
        return false;
    }
    let mut chars = pointer.chars();
    while let Some(c) = chars.next() {
        if c == '~' && !matches!(chars.next(), Some('0' | '1')) {
            return false;
        }
    }
    true
}

/// Environment references are names, never assignments or shell expressions.
fn valid_env(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Reflect schema field names only; arbitrary map keys may themselves contain secrets.
pub(crate) fn safe_field_path(path: &serde_path_to_error::Path) -> String {
    let known = [
        "notes",
        "address",
        "user_agent",
        "search",
        "fetch",
        "engines",
        "remote",
        "max_results",
        "engine_timeout",
        "timeout",
        "max_response_bytes",
        "max_redirects",
        "cache_ttl",
        "cache_bytes",
        "allow_private_networks",
        "max_concurrency",
        "enabled",
        "use",
        "config",
        "type",
        "url",
        "query_param",
        "limit_param",
        "params",
        "headers",
        "header_env",
        "results_pointer",
        "title_pointer",
        "url_pointer",
        "snippet_pointer",
        "published_pointer",
        "text_part_pointer",
        "command",
        "args",
        "env",
        "temp_dir",
        "cwd",
        "version",
        "filter_params",
        "retry_after_max",
        "snippets_pointer",
        "updated_pointer",
        "research",
        "crawl",
        "processing",
        "sources",
        "source_characters",
        "total_characters",
        "max_per_host",
        "max_attempts",
        "session_ttl",
        "session_entries",
        "read_characters",
        "max_pages",
        "max_hosts",
        "host_ttl",
        "max_depth",
        "max_bytes",
        "delay",
        "reranker",
        "synthesizer",
        "max_output_bytes",
    ];
    let mut fields = Vec::new();
    for segment in path {
        match segment {
            serde_path_to_error::Segment::Map { key } if known.contains(&key.as_str()) => {
                fields.push(key.as_str())
            }
            _ => break,
        }
    }
    if fields.is_empty() {
        "settings".into()
    } else {
        fields.join(".")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use serde_json::json;

    /// Resolve one engine from an `engines` settings object.
    fn resolve(engines: Value) -> Result<Adapter, ConfigError> {
        let settings: EngineSettings = serde_json::from_value(engines).unwrap();
        settings.adapter("fixture")
    }

    #[test]
    fn validation_does_not_resolve_engines_and_open_errors_hide_values() {
        let mut config: Config = serde_json::from_value(json!({"engines":{
            "use":["missing-engine"],
            "config":{"other":{"url":"invalid-secret-url"}}
        }}))
        .unwrap();
        config.validate().unwrap();
        let mut disabled = config.clone();
        disabled.engines.enabled = false;
        assert!(crate::core::engines::Pool::new(&disabled)
            .unwrap()
            .names()
            .is_empty());
        config.engines.use_engines = vec!["other".into()];
        let error = crate::core::Search::open(config).err().unwrap().to_string();
        assert!(error.contains("engine other"), "{error}");
        assert!(!error.contains("invalid-secret-url"));
    }

    #[test]
    fn selection_is_explicit_and_validated() {
        let defaults: Config = serde_json::from_value(json!({})).unwrap();
        assert_eq!(defaults.engines.use_engines, [DEFAULT_ENGINE]);
        let empty: Config = serde_json::from_value(json!({"engines":{"use":[]}})).unwrap();
        assert!(crate::core::engines::Pool::new(&empty)
            .unwrap()
            .names()
            .is_empty());
        assert!(serde_json::from_value::<Config>(json!({"engines":{"adapters":{}}})).is_err());
        for selection in [
            json!(["../escape"]),
            json!(["Upper"]),
            json!(["fixture", "fixture"]),
        ] {
            let config: Config =
                serde_json::from_value(json!({"engines":{"use":selection}})).unwrap();
            assert!(config.validate().is_err());
        }
        let config: Config =
            serde_json::from_value(json!({"engines":{"config":{"fixture":[]}}})).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn preset_entries_merge_shallowly_and_cannot_change_type() {
        let settings: EngineSettings = serde_json::from_value(json!({"config":{
            "mwmbl":{"query_param":"query","params":{"lang":"en"}},
            "searxng":{"type":"command","command":"tool"}
        }}))
        .unwrap();
        let Adapter::Http(mwmbl) = settings.adapter("mwmbl").unwrap() else {
            panic!("mwmbl is an HTTP preset")
        };
        assert_eq!(mwmbl.query_param, "query");
        assert_eq!(mwmbl.url, "https://api.mwmbl.org/search/");
        assert_eq!(mwmbl.text_part_pointer.as_deref(), Some("/value"));
        let error = settings.adapter("searxng").unwrap_err().to_string();
        assert!(error.contains("type cannot change"), "{error}");
    }

    #[test]
    fn missing_preset_field_names_the_configure_command() {
        let settings = EngineSettings::default();
        let error = settings.adapter("searxng").unwrap_err().to_string();
        assert!(
            error.contains("search configure searxng url URL"),
            "{error}"
        );
        let settings: EngineSettings = serde_json::from_value(
            json!({"config":{"searxng":{"url":"https://searx.example/search"}}}),
        )
        .unwrap();
        assert!(settings.adapter("searxng").is_ok());
    }

    #[test]
    fn custom_engines_need_a_complete_typed_adapter() {
        for entry in [json!({}), json!({"url":"https://example.com/"})] {
            let error = resolve(json!({"config":{"fixture":entry}}))
                .unwrap_err()
                .to_string();
            assert!(error.contains("type must be http or command"), "{error}");
        }
        let error = resolve(json!({})).unwrap_err().to_string();
        assert!(error.contains("not configured"), "{error}");
        assert!(resolve(json!({"config":{"fixture":{"type":"http"}}})).is_err());
        assert!(resolve(
            json!({"config":{"fixture":{"type":"http","url":"https://example.com/"}}})
        )
        .is_ok());
    }

    #[test]
    fn command_cwd_defaults_from_command_or_scratch() {
        let cwd = |entry: Value| match resolve(json!({"config":{"fixture":entry}})) {
            Ok(Adapter::Command(command)) => Ok(command.cwd.unwrap()),
            Ok(Adapter::Http(_)) => panic!("command fixture"),
            Err(error) => Err(error.to_string()),
        };
        let absolute = std::env::temp_dir().canonicalize().unwrap().join("engine");
        assert_eq!(
            cwd(json!({"type":"command","command":absolute})).unwrap(),
            std::env::temp_dir().canonicalize().unwrap()
        );
        assert_eq!(
            cwd(json!({"type":"command","command":"python3","temp_dir":absolute})).unwrap(),
            absolute
        );
        assert_eq!(
            cwd(json!({"type":"command","command":"python3","cwd":absolute})).unwrap(),
            absolute
        );
        assert!(cwd(json!({"type":"command","command":"./run"})).is_err());
        assert!(cwd(json!({"type":"command","command":"tool","cwd":"relative"})).is_err());
    }

    #[test]
    fn transports_reject_bad_bounds_pointers_headers_and_environment_names() {
        for adapter in [
            json!({"type":"command","command":" "}),
            json!({"type":"command","command":"tool","max_response_bytes":0}),
            json!({"type":"command","command":"tool","args":["bad\u{0000}arg"]}),
            json!({"type":"command","command":"tool","env":["TOKEN=secret"]}),
            json!({"type":"http","url":"https://secret:password@example.com/"}),
            json!({"type":"http","url":"https://example.com/#secret"}),
            json!({"type":"http","url":"ftp://example.com/"}),
            json!({"type":"http","url":"https://example.com/","title_pointer":"/bad~2"}),
            json!({"type":"http","url":"https://example.com/","results_pointer":"results"}),
            json!({"type":"http","url":"https://example.com/","headers":{"Bad Header":"secret"}}),
            json!({"type":"http","url":"https://example.com/","header_env":{"Authorization":"BAD=secret"}}),
            json!({"type":"http","url":"https://example.com/","max_response_bytes":0}),
            json!({"type":"http","url":"https://example.com/","query_param":""}),
            json!({"type":"http","url":"https://example.com/","limit_param":"q"}),
            json!({"type":"http","url":"https://example.com/","unknown":"secret"}),
        ] {
            let error = resolve(json!({"config":{"fixture":adapter}}))
                .unwrap_err()
                .to_string();
            assert!(!error.contains("secret"), "{error}");
            assert!(error.contains("engine fixture"), "{error}");
        }
    }

    /// A settings diagnostic names the failing field. This pins the allowlist
    /// that keeps arbitrary (possibly secret) map keys out of the message: a
    /// missing field name silently degrades the path to its parent.
    #[test]
    fn diagnostic_field_paths_name_the_failing_field() {
        for (source, field) in [
            (r#"{"fetch":{"cache_bytes":"x"}}"#, "fetch.cache_bytes"),
            (r#"{"fetch":{"timeout":"x"}}"#, "fetch.timeout"),
            (r#"{"engines":{"use":1}}"#, "engines.use"),
        ] {
            let mut deserializer = serde_json::Deserializer::from_str(source);
            let result: Result<Config, _> = serde_path_to_error::deserialize(&mut deserializer);
            let error = result.expect_err(source);
            assert_eq!(safe_field_path(error.path()), field, "{source}");
        }
    }
    /// Filter mappings cannot overwrite query/limit fields or silently accept typos.
    #[test]
    fn filter_mapping_and_version_validation_is_strict_and_opaque() {
        for fields in [
            json!({"filter_params":{"unknown":"x"}}),
            json!({"filter_params":{"safe":"q"}}),
            json!({"filter_params":{"language":"x","region":"x"}}),
            json!({"filter_params":{"safe":""}}),
            json!({"snippets_pointer":"bad"}),
            json!({"updated_pointer":"/bad~2"}),
        ] {
            let mut adapter = json!({"type":"http","url":"https://example.com"});
            adapter
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            assert!(resolve(json!({"config":{"fixture":adapter}})).is_err());
        }
        for version in [0, 3] {
            assert!(resolve(
                json!({"config":{"fixture":{"type":"command","command":"tool","version":version}}})
            )
            .is_err());
        }
        for version in [1, 2] {
            assert!(resolve(
                json!({"config":{"fixture":{"type":"command","command":"tool","version":version}}})
            )
            .is_ok());
        }
        for (source, expected) in [
            (
                r#"{"processing":{"max_output_bytes":"secret"}}"#,
                "processing.max_output_bytes",
            ),
            (
                r#"{"research":{"session_entries":"secret"}}"#,
                "research.session_entries",
            ),
            (r#"{"crawl":{"max_pages":"secret"}}"#, "crawl.max_pages"),
        ] {
            let mut de = serde_json::Deserializer::from_str(source);
            let error = serde_path_to_error::deserialize::<_, Config>(&mut de).unwrap_err();
            assert_eq!(safe_field_path(error.path()), expected);
        }
    }
}
