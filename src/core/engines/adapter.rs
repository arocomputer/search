//! Configured GET and executable engines, with bounded output and opaque errors.
//!
//! Both transports map an upstream JSON answer into `Found`: invalid rows are
//! skipped and counted, but an answer whose every row is invalid is malformed.

use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{Engine, EngineError, EngineFuture, Found};
use crate::core::config::engines::http_url;
use crate::core::config::{Adapter as Settings, Command, Http};
use crate::core::fetch::guard::{check_host, GuardedResolver};
use crate::core::{Link, Query};

/// An owned configured identifier and its query transport.
pub(super) struct Adapter {
    name: String,
    transport: Transport,
}

/// HTTP owns a reusable guarded client; commands retain only their launch settings.
#[allow(clippy::large_enum_variant)] // One startup transport per engine; boxing adds no useful bound.
enum Transport {
    Http(Http, reqwest::Client),
    Command(Command),
}

impl Adapter {
    /// Build HTTP clients at startup; credentials remain resolved for each query.
    pub(super) fn new(
        name: String,
        settings: Settings,
        user_agent: &str,
    ) -> Result<Self, reqwest::Error> {
        let transport = match settings {
            Settings::Http(settings) => {
                let client = reqwest::Client::builder()
                    .user_agent(user_agent)
                    .redirect(reqwest::redirect::Policy::none())
                    .no_proxy()
                    .dns_resolver(Arc::new(GuardedResolver::new(
                        settings.allow_private_networks,
                    )))
                    .build()?;
                Transport::Http(settings, client)
            }
            Settings::Command(settings) => Transport::Command(settings),
        };
        Ok(Self { name, transport })
    }
}

impl Engine for Adapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn search(&self, query: String, limit: usize) -> EngineFuture {
        self.search_query(
            Query {
                text: query,
                ..Default::default()
            },
            limit,
        )
    }

    fn unsupported_filters(&self, filters: &crate::core::service::QueryFilters) -> Vec<String> {
        filters
            .values()
            .into_keys()
            .filter(|name| match &self.transport {
                Transport::Http(settings, _) => !settings.filter_params.contains_key(name),
                Transport::Command(settings) => settings.version == 1,
            })
            .collect()
    }

    fn search_query(&self, query: Query, limit: usize) -> EngineFuture {
        let name = self.name.clone();
        match &self.transport {
            Transport::Http(settings, client) => {
                let settings = settings.clone();
                let client = client.clone();
                Box::pin(async move {
                    http(
                        &name,
                        &settings,
                        &client,
                        &query.text,
                        &query.filters,
                        limit,
                    )
                    .await
                })
            }
            Transport::Command(settings) => {
                let settings = settings.clone();
                Box::pin(async move { command_query(&name, &settings, &query, limit).await })
            }
        }
    }
}

/// Dial only the configured endpoint; requested filters replace static defaults,
/// and environment values are whole sensitive headers.
async fn http(
    name: &str,
    settings: &Http,
    client: &reqwest::Client,
    query: &str,
    filters: &crate::core::service::QueryFilters,
    limit: usize,
) -> Result<Found, EngineError> {
    let mut url =
        http_url(&settings.url).ok_or_else(|| EngineError::malformed("invalid engine endpoint"))?;
    check_host(
        url.host_str().unwrap_or_default(),
        settings.allow_private_networks,
    )
    .map_err(|_| EngineError::network("engine endpoint refused"))?;
    {
        let requested_filters = filters.values();
        let dynamic = |key: &str| {
            key == settings.query_param
                || settings.limit_param.as_deref() == Some(key)
                || requested_filters.keys().any(|field| {
                    settings
                        .filter_params
                        .get(field)
                        .is_some_and(|param| param == key)
                })
        };
        let mut values: Vec<(String, String)> = url
            .query_pairs()
            .into_owned()
            .filter(|(key, _)| !settings.params.contains_key(key) && !dynamic(key))
            .collect();
        values.extend(
            settings
                .params
                .iter()
                .filter(|(key, _)| !dynamic(key))
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        values.push((settings.query_param.clone(), query.into()));
        if let Some(param) = &settings.limit_param {
            values.push((param.clone(), limit.to_string()));
        }
        for (field, filter_values) in requested_filters {
            if let Some(param) = settings.filter_params.get(&field) {
                values.extend(
                    filter_values
                        .into_iter()
                        .map(|value| (param.clone(), value)),
                );
            }
        }
        url.set_query(None);
        let mut params = url.query_pairs_mut();
        params.extend_pairs(&values);
    }
    let mut headers = reqwest::header::HeaderMap::new();
    for (key, value) in &settings.headers {
        insert_header(&mut headers, key, value, false)?;
    }
    for (key, variable) in &settings.header_env {
        let value = std::env::var(variable)
            .map_err(|_| EngineError::rejected("engine credential unavailable"))?;
        insert_header(&mut headers, key, &value, true)?;
    }
    let mut response = client
        .get(url.clone())
        .headers(headers.clone())
        .send()
        .await
        .map_err(|_| EngineError::network("engine HTTP request failed"))?;
    if matches!(response.status().as_u16(), 429 | 503) {
        if let Some(delay) = retry_delay(response.headers(), settings.retry_after_max) {
            drop(response);
            tokio::time::sleep(delay).await;
            response = client
                .get(url)
                .headers(headers)
                .send()
                .await
                .map_err(|_| EngineError::network("engine HTTP request failed"))?;
        }
    }
    if !response.status().is_success() {
        return Err(EngineError::rejected(format!(
            "engine HTTP status {} rejected",
            response.status().as_u16()
        )));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| EngineError::network("engine HTTP read failed"))?
    {
        if (bytes.len() as u64).saturating_add(chunk.len() as u64) > settings.max_response_bytes {
            return Err(EngineError::malformed("engine output exceeded cap"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| malformed())?;
    rows(
        name,
        &value,
        Mapping {
            results: &settings.results_pointer,
            title: &settings.title_pointer,
            url: &settings.url_pointer,
            snippet: &settings.snippet_pointer,
            published: &settings.published_pointer,
            snippets: &settings.snippets_pointer,
            updated: &settings.updated_pointer,
            text_part: settings.text_part_pointer.as_deref(),
        },
        limit,
    )
}

/// Retry only when the entire server-requested delay fits the permitted wait.
/// Oversized delays preserve the original rejection; existing deadlines cancel allowed waits.
fn retry_delay(headers: &reqwest::header::HeaderMap, maximum: u64) -> Option<std::time::Duration> {
    if maximum == 0 {
        return None;
    }
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let delay = match value.parse::<u64>() {
        Ok(seconds) => std::time::Duration::from_secs(seconds),
        Err(_) => http_date(value)?
            .duration_since(std::time::SystemTime::now())
            .unwrap_or_default(),
    };
    (delay <= std::time::Duration::from_millis(maximum)).then_some(delay)
}

/// Parse the standard IMF-fixdate Retry-After form without a new dependency.
fn http_date(value: &str) -> Option<std::time::SystemTime> {
    let parts: Vec<_> = value.split_ascii_whitespace().collect();
    let [weekday, day, month, year, time, zone] = parts.as_slice() else {
        return None;
    };
    if !["Mon,", "Tue,", "Wed,", "Thu,", "Fri,", "Sat,", "Sun,"].contains(weekday) || *zone != "GMT"
    {
        return None;
    }
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| m == month)? as i64
        + 1;
    let year: i64 = year.parse().ok()?;
    let day: i64 = day.parse().ok()?;
    let clock: Vec<_> = time.split(':').collect();
    let [hour, minute, second] = clock.as_slice() else {
        return None;
    };
    let (hour, minute, second) = (
        hour.parse::<i64>().ok()?,
        minute.parse::<i64>().ok()?,
        second.parse::<i64>().ok()?,
    );
    let max_day = match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1970..=9999).contains(&year)
        || !(1..=max_day).contains(&day)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..60).contains(&second)
    {
        return None;
    }
    // Gregorian civil date to days since the Unix epoch.
    let adjusted = year - i64::from(month <= 2);
    let era = adjusted / 400;
    let yoe = adjusted - era * 400;
    let shifted = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * shifted + 2) / 5 + day - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    let seconds = days * 86400 + hour * 3600 + minute * 60 + second;
    std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(u64::try_from(seconds).ok()?))
}

/// Validate headers without including either their names or values in errors.
fn insert_header(
    headers: &mut reqwest::header::HeaderMap,
    key: &str,
    value: &str,
    sensitive: bool,
) -> Result<(), EngineError> {
    let key = reqwest::header::HeaderName::from_bytes(key.as_bytes())
        .map_err(|_| EngineError::rejected("engine header invalid"))?;
    let mut value = reqwest::header::HeaderValue::from_str(value)
        .map_err(|_| EngineError::rejected("engine header invalid"))?;
    value.set_sensitive(sensitive);
    headers.insert(key, value);
    Ok(())
}

/// Exchange one strict JSON document; dropping this future kills the direct child.
/// `command` is absolute or a bare name found on `PATH`; settings resolution
/// supplies an absolute working directory.
#[cfg(test)]
async fn command(
    name: &str,
    settings: &Command,
    query: &str,
    limit: usize,
) -> Result<Found, EngineError> {
    command_query(
        name,
        settings,
        &Query {
            text: query.into(),
            ..Default::default()
        },
        limit,
    )
    .await
}

/// Exchange a version-selected request, killing the direct child on cancellation.
async fn command_query(
    name: &str,
    settings: &Command,
    query: &Query,
    limit: usize,
) -> Result<Found, EngineError> {
    let cwd = settings
        .cwd
        .as_deref()
        .filter(|cwd| cwd.is_absolute())
        .ok_or_else(|| EngineError::rejected("engine working directory unavailable"))?;
    let mut process = tokio::process::Command::new(&settings.command);
    process.args(&settings.args).current_dir(cwd).env_clear();
    for name in [
        "PATH",
        "HOME",
        "USERPROFILE",
        "PATHEXT",
        "LANG",
        "SystemRoot",
        "SYSTEMROOT",
    ] {
        if let Some(value) = std::env::var_os(name) {
            process.env(name, value);
        }
    }
    for name in &settings.env {
        if matches!(name.as_str(), "TMPDIR" | "TMP" | "TEMP") {
            continue;
        }
        let value = std::env::var_os(name)
            .ok_or_else(|| EngineError::rejected("engine credential unavailable"))?;
        process.env(name, value);
    }
    super::scratch::prepare(&settings.temp_dir)
        .map_err(|_| EngineError::rejected("engine temporary directory unavailable or unsafe"))?;
    for name in ["TMPDIR", "TMP", "TEMP"] {
        process.env(name, &settings.temp_dir);
    }
    let mut child = process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| EngineError::network("engine command failed to start"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| EngineError::network("engine command input unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| EngineError::network("engine command output unavailable"))?;
    let mut request =
        serde_json::json!({"version": settings.version, "query": query.text, "limit": limit});
    if settings.version == 2 {
        if let Some(object) = request.as_object_mut() {
            object.insert(
                "filters".into(),
                serde_json::to_value(&query.filters).map_err(|_| malformed())?,
            );
        }
    }
    if !settings.config.is_empty() {
        if let Some(object) = request.as_object_mut() {
            object.insert(
                "config".into(),
                serde_json::to_value(&settings.config).map_err(|_| malformed())?,
            );
        }
    }
    let request = serde_json::to_vec(&request).map_err(|_| malformed())?;
    let write = async move {
        stdin
            .write_all(&request)
            .await
            .map_err(|_| EngineError::network("engine command write failed"))?;
        stdin
            .shutdown()
            .await
            .map_err(|_| EngineError::network("engine command write failed"))?;
        drop(stdin);
        Ok::<(), EngineError>(())
    };
    let read = async {
        let mut bytes = Vec::new();
        stdout
            .take(settings.max_response_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| EngineError::network("engine command read failed"))?;
        if bytes.len() as u64 > settings.max_response_bytes {
            return Err(EngineError::malformed("engine output exceeded cap"));
        }
        Ok(bytes)
    };
    let (_, bytes) = tokio::try_join!(write, read)?;
    let status = child
        .wait()
        .await
        .map_err(|_| EngineError::network("engine command wait failed"))?;
    if !status.success() {
        return Err(EngineError::rejected(
            "engine command exited unsuccessfully",
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| malformed())?;
    let mut found = rows(name, &value, Mapping::COMMAND, limit)?;
    if settings.version == 2 {
        if let Some(value) = value.get("unsupported_filters") {
            found.unsupported_filters = value
                .as_array()
                .ok_or_else(malformed)?
                .iter()
                .map(|v| v.as_str().map(str::to_owned).ok_or_else(malformed))
                .collect::<Result<Vec<_>, _>>()?;
        }
    }
    Ok(found)
}

/// JSON field locations; executable answers use the fixed protocol mapping.
struct Mapping<'a> {
    results: &'a str,
    title: &'a str,
    url: &'a str,
    snippet: &'a str,
    published: &'a str,
    snippets: &'a str,
    updated: &'a str,
    text_part: Option<&'a str>,
}

impl Mapping<'_> {
    const COMMAND: Self = Self {
        results: "/results",
        title: "/title",
        url: "/url",
        snippet: "/snippet",
        published: "/published",
        snippets: "/snippets",
        updated: "/updated",
        text_part: None,
    };
}

/// Decode text or explicitly mapped ordered text fragments; `None` when any part is unusable.
fn mapped_text(value: &serde_json::Value, part: Option<&str>) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.into());
    }
    let pointer = part?;
    let mut text = String::new();
    for part in value.as_array()? {
        text.push_str(part.pointer(pointer)?.as_str()?);
    }
    Some(text)
}

/// Map one row, or `None` when its title is empty, its URL is not an absolute
/// credential-free HTTP(S) URL, or its snippet is present but not text.
fn row(name: &str, row: &serde_json::Value, mapping: &Mapping<'_>) -> Option<Link> {
    let title = mapped_text(row.pointer(mapping.title)?, mapping.text_part)?;
    if title.trim().is_empty() {
        return None;
    }
    let url = row.pointer(mapping.url)?.as_str()?;
    // Result fragments are useful page anchors; endpoint fragments are forbidden.
    let mut parsed = url::Url::parse(url).ok()?;
    parsed.set_fragment(None);
    http_url(parsed.as_str())?;
    let snippet = match row.pointer(mapping.snippet) {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(mapped_text(value, mapping.text_part)?),
    };
    // A date is grounding, not identity: a malformed one is simply omitted, and
    // never fails the row the way a malformed snippet does.
    let published = match row.pointer(mapping.published) {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => mapped_text(value, mapping.text_part),
    };
    let mut snippets = match row.pointer(mapping.snippets) {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(value) => value
            .as_array()?
            .iter()
            .map(|v| mapped_text(v, mapping.text_part))
            .collect::<Option<Vec<_>>>()?,
    };
    let mut seen = std::collections::HashSet::new();
    snippets.retain(|text| seen.insert(text.clone()));
    if let Some(text) = &snippet {
        if !snippets.contains(text) {
            snippets.insert(0, text.clone());
        }
    }
    let updated = row
        .pointer(mapping.updated)
        .and_then(|v| mapped_text(v, mapping.text_part));
    Some(Link {
        title,
        url: url.into(),
        snippet,
        snippets,
        updated,
        published,
        engines: vec![name.into()],
        score: 0.0,
    })
}

/// Collect valid rows up to `limit`, counting skipped invalid rows on the way,
/// and stamp only locally configured provenance. A missing results array, or a
/// non-empty one with no valid row, is malformed.
fn rows(
    name: &str,
    value: &serde_json::Value,
    mapping: Mapping<'_>,
    limit: usize,
) -> Result<Found, EngineError> {
    let rows = value
        .pointer(mapping.results)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(malformed)?;
    let mut found = Found::default();
    for value in rows {
        if found.links.len() >= limit {
            break;
        }
        match row(name, value, &mapping) {
            Some(link) => found.links.push(link),
            None => found.skipped += 1,
        }
    }
    if found.links.is_empty() && found.skipped > 0 {
        return Err(malformed());
    }
    Ok(found)
}

/// Parsing errors never carry any adapter output.
fn malformed() -> EngineError {
    EngineError::malformed("engine response malformed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use crate::core::engines::{EngineStatus, Pool};
    use crate::core::Query;
    use std::time::Duration;

    /// Python is only an offline executable fixture, never a production dependency.
    fn executable(script: &str, cap: u64) -> Command {
        Command {
            version: 1,
            command: if cfg!(windows) {
                "python.exe"
            } else {
                "python3"
            }
            .into(),
            config: Default::default(),
            env: Vec::new(),
            cwd: Some(std::env::temp_dir().canonicalize().unwrap()),
            temp_dir: std::env::temp_dir().canonicalize().unwrap().join("search"),
            args: vec!["-c".into(), script.trim_start().into()],
            max_response_bytes: cap,
        }
    }

    /// Exercise the production HTTP transport with fixture settings.
    async fn query_http(
        name: &str,
        settings: &Http,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Link>, EngineError> {
        Adapter::new(name.into(), Settings::Http(settings.clone()), "search-test")
            .unwrap()
            .search(query.into(), limit)
            .await
            .map(|found| found.links)
    }

    #[tokio::test]
    async fn command_protocol_is_literal_eof_delimited_and_owns_provenance() {
        let settings = executable(
            r#"
import sys,json
request=json.load(sys.stdin)
assert request == {'version':1,'query':'$(echo secret) $HOME ~','limit':1}
assert sys.argv[1] == '$HOME'
print(json.dumps({'results':[{'title':'First','url':'https://example.com/a','engines':['forged'],'score':99},{'title':'Second','url':'https://example.com/b'}]}))
"#,
            4096,
        );
        let mut settings = settings;
        settings.args.push("$HOME".into());
        let results = command("owned", &settings, "$(echo secret) $HOME ~", 1)
            .await
            .unwrap()
            .links;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].engines, ["owned"]);
        assert_eq!(results[0].score, 0.0);
    }

    #[tokio::test]
    async fn commands_receive_only_declared_credentials_cwd_and_json_config() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("search-command-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("asset.txt"), "engine data").unwrap();
        let declared = format!("SEARCH_DECLARED_{}", uuid::Uuid::new_v4().simple());
        let hidden = format!("SEARCH_HIDDEN_{}", uuid::Uuid::new_v4().simple());
        std::env::set_var(&declared, "fixture-secret");
        std::env::set_var(&hidden, "must-not-leak");
        let mut settings = executable(
            r#"
import sys,json,os
request=json.load(sys.stdin)
assert os.environ[sys.argv[1]] == 'fixture-secret'
assert sys.argv[2] not in os.environ
assert open('asset.txt').read() == 'engine data'
assert request['config'] == {'literal':'$HOME; $(echo secret)'}
print(json.dumps({'results':[]}))
"#,
            4096,
        );
        settings.cwd = Some(root.clone());
        settings.env.push(declared.clone());
        settings.args.extend([declared.clone(), hidden.clone()]);
        settings
            .config
            .insert("literal".into(), serde_json::json!("$HOME; $(echo secret)"));
        let result = command("fixture", &settings, "q", 1).await;
        std::env::remove_var(&declared);
        std::env::remove_var(&hidden);
        std::fs::remove_dir_all(root).unwrap();
        assert!(result.unwrap().links.is_empty());
        let error = command("fixture", &settings, "q", 1).await.unwrap_err();
        assert_eq!(error.message, "engine credential unavailable");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn commands_use_private_uniform_temp_variables_and_reject_unsafe_paths() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("search-scratch-test-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new()
            .recursive(false)
            .create(&root)
            .unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let scratch = root.join("scratch");
        let mut settings = executable(
            r#"
import json,os,sys,tempfile
expected=sys.argv[1]
assert all(os.environ[key] == expected for key in ('TMPDIR','TMP','TEMP'))
with tempfile.NamedTemporaryFile() as f:
    assert os.path.dirname(f.name) == expected
assert os.stat(expected).st_mode & 0o777 == 0o700
print(json.dumps({'results':[]}))
"#,
            4096,
        );
        settings.temp_dir = scratch.clone();
        settings.env = vec!["TMPDIR".into(), "TMP".into(), "TEMP".into()];
        settings.args.push(scratch.to_string_lossy().into_owned());
        assert!(command("fixture", &settings, "q", 1)
            .await
            .unwrap()
            .links
            .is_empty());
        let link = root.join("link");
        symlink(&scratch, &link).unwrap();
        settings.temp_dir = link;
        assert!(command("fixture", &settings, "q", 1).await.is_err());
        std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o777)).unwrap();
        settings.temp_dir = scratch;
        assert!(command("fixture", &settings, "q", 1).await.is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn command_rejects_malformed_oversize_and_nonzero_without_leaks() {
        for (script, cap) in [
            ("print('not-json-secret')", 4096),
            ("print('{\"results\":[]} {\"results\":[]}')", 4096),
            ("print('x'*1000)", 10),
            ("import sys; print('{\"results\":[]}'); sys.stderr.write('stderr-secret'); sys.exit(2)", 4096),
        ] {
            let error = command("fixture", &executable(script, cap), "test", 2).await.unwrap_err();
            assert!(!error.message.contains("secret"));
        }
    }

    #[tokio::test]
    async fn command_reads_while_writing_large_requests() {
        let settings = executable("import sys; sys.stdout.write(' '*131072); sys.stdout.flush(); sys.stdin.read(); print('{\"results\":[]}')", 200000);
        let results = tokio::time::timeout(
            Duration::from_secs(5),
            command("fixture", &settings, &"q".repeat(200000), 2),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(results.links.is_empty());
    }

    /// A one-shot local socket captures the request and emits a chosen wire answer.
    async fn socket_response(
        status: &str,
        body: &str,
        extra: &str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/search", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
            body.len()
        );
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 1024];
            while !bytes.windows(4).any(|s| s == b"\r\n\r\n") {
                let size = socket.read(&mut buffer).await.unwrap();
                if size == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..size]);
            }
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(bytes).unwrap()
        });
        (url, task)
    }

    fn http_settings(url: &str) -> Http {
        serde_json::from_value(serde_json::json!({"url":url,"allow_private_networks":true}))
            .unwrap()
    }

    #[tokio::test]
    async fn http_maps_json_and_sends_whole_env_header_and_encoded_parameters() {
        let (url, task) = socket_response("200 OK", r#"{"data":{"items":[{"meta":{"heading":"Mapped"},"link":"https://example.com/","summary":"text"}]}}"#, "").await;
        let variable = format!("SEARCH_TEST_AUTH_{}", uuid::Uuid::new_v4().simple());
        std::env::set_var(&variable, "Bearer fixture-secret");
        let mut settings = http_settings(&url);
        settings
            .header_env
            .insert("Authorization".into(), variable.clone());
        settings
            .headers
            .insert("X-Fixture".into(), "constant".into());
        settings.params.insert("format".into(), "json".into());
        settings.limit_param = Some("count".into());
        settings.results_pointer = "/data/items".into();
        settings.title_pointer = "/meta/heading".into();
        settings.url_pointer = "/link".into();
        settings.snippet_pointer = "/summary".into();
        let results = query_http("mapped", &settings, "a & b", 3).await.unwrap();
        std::env::remove_var(variable);
        let request = task.await.unwrap().to_lowercase();
        assert!(request.contains("q=a+%26+b"));
        assert!(request.contains("count=3"));
        assert!(request.contains("format=json"));
        assert!(request.contains("authorization: bearer fixture-secret"));
        assert!(request.contains("x-fixture: constant"));
        assert_eq!(results[0].title, "Mapped");
        assert_eq!(results[0].snippet.as_deref(), Some("text"));
        assert_eq!(results[0].engines, ["mapped"]);
    }

    /// Repeated queries reuse a connection and read credentials after construction.
    #[tokio::test]
    async fn http_reuses_client_and_refreshes_credentials() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut settings = http_settings(&format!("http://{}/", listener.local_addr().unwrap()));
        let variable = format!("SEARCH_REFRESH_{}", uuid::Uuid::new_v4().simple());
        settings
            .header_env
            .insert("Authorization".into(), variable.clone());
        let adapter =
            Adapter::new("fixture".into(), Settings::Http(settings), "search-test").unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            for value in ["first", "second"] {
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                }
                assert!(String::from_utf8(request)
                    .unwrap()
                    .to_lowercase()
                    .contains(&format!("authorization: {value}\r\n")));
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\n\r\n{\"results\":[]}")
                    .await
                    .unwrap();
            }
        });
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            for value in ["first", "second"] {
                std::env::set_var(&variable, value);
                assert!(adapter
                    .search("q".into(), 1)
                    .await
                    .unwrap()
                    .links
                    .is_empty());
            }
            server.await.unwrap();
        })
        .await;
        std::env::remove_var(variable);
        result.unwrap();
    }

    #[tokio::test]
    async fn http_refuses_redirects_and_enforces_body_cap() {
        for (status, body, extra, cap) in [
            (
                "302 Found",
                "",
                "Location: http://127.0.0.1:1/secret\r\n",
                4096,
            ),
            ("200 OK", "body-secret-too-long", "", 4),
        ] {
            let (url, task) = socket_response(status, body, extra).await;
            let mut settings = http_settings(&url);
            settings.max_response_bytes = cap;
            let error = query_http("fixture", &settings, "q", 1).await.unwrap_err();
            task.await.unwrap();
            assert!(!error.message.contains("secret"));
            assert!(!error.message.contains("127.0.0.1"));
        }
    }

    #[tokio::test]
    async fn mapped_text_fragments_keep_the_complete_title_and_snippet() {
        let (url, task) = socket_response("200 OK", r#"{"results":[{"title":[{"value":"Rust"},{"value":" async guide"}],"url":"https://example.com/","snippet":[{"value":"First "},{"value":"second"}]}]}"#, "").await;
        let mut settings = http_settings(&url);
        settings.text_part_pointer = Some("/value".into());
        let results = query_http("fixture", &settings, "query", 1).await.unwrap();
        task.await.unwrap();
        assert_eq!(results[0].title, "Rust async guide");
        assert_eq!(results[0].snippet.as_deref(), Some("First second"));
        assert!(mapped_text(&serde_json::json!([{"value":4}]), Some("/value")).is_none());
    }

    #[tokio::test]
    async fn current_query_and_limit_replace_configured_parameter_values() {
        let (url, task) = socket_response("200 OK", r#"{"results":[]}"#, "").await;
        let mut settings = http_settings(&format!(
            "{url}?q=old&format=html&count=99&filter=a&filter=b"
        ));
        settings.params.insert("q".into(), "also-old".into());
        settings.params.insert("format".into(), "json".into());
        settings.params.insert("count".into(), "100".into());
        settings.limit_param = Some("count".into());
        query_http("fixture", &settings, "new query", 3)
            .await
            .unwrap();
        let wire = task.await.unwrap();
        let target = wire.split_whitespace().nth(1).unwrap();
        let request = url::Url::parse(&format!("http://localhost{target}")).unwrap();
        let params: Vec<_> = request.query_pairs().into_owned().collect();
        assert_eq!(
            params,
            vec![
                ("filter".into(), "a".into()),
                ("filter".into(), "b".into()),
                ("format".into(), "json".into()),
                ("q".into(), "new query".into()),
                ("count".into(), "3".into())
            ]
        );
    }

    #[tokio::test]
    async fn private_permission_is_independent_and_missing_env_remains_a_failed_engine() {
        let mut config = Config::default();
        config.fetch.allow_private_networks = true;
        let mut settings = http_settings("http://127.0.0.1:1/");
        settings.allow_private_networks = false;
        let response = Pool::from_adapters(
            vec![("private", Settings::Http(settings))],
            config.search.clone(),
        )
        .search(Query::default())
        .await;
        assert_eq!(response.engines[0].status, EngineStatus::Error);
        assert_eq!(
            response.engines[0].error.as_deref(),
            Some("engine endpoint refused")
        );
        let mut settings = http_settings("https://example.com/");
        settings.header_env.insert(
            "Authorization".into(),
            format!("SEARCH_MISSING_{}", uuid::Uuid::new_v4().simple()),
        );
        let response =
            Pool::from_adapters(vec![("missing", Settings::Http(settings))], config.search)
                .search(Query::default())
                .await;
        assert_eq!(response.engines[0].name, "missing");
        assert_eq!(response.engines[0].status, EngineStatus::Error);
        assert_eq!(
            response.engines[0].error.as_deref(),
            Some("engine credential unavailable")
        );
    }

    #[tokio::test]
    async fn pool_selects_ranks_and_times_out_configured_commands() {
        let mut config = Config::default();
        config.search.max_results = 2;
        config.search.engine_timeout = 2000;
        let pool = Pool::from_adapters(
            vec![
                (
                    "selected",
                    Settings::Command(executable(
                        r#"
import json,sys
request=json.load(sys.stdin)
assert request['limit'] == 2
print(json.dumps({'results':[{'title':str(i),'url':'https://example.com/'+str(i)} for i in range(3)]}))
"#,
                        4096,
                    )),
                ),
                (
                    "omitted",
                    Settings::Command(executable("raise Exception('must not run')", 4096)),
                ),
            ],
            config.search.clone(),
        );
        let response = pool
            .search(Query {
                limit: 10,
                engines: vec!["selected".into()],
                ..Query::default()
            })
            .await;
        assert_eq!(response.results.len(), 2);
        assert_eq!(response.engines[0].count, 2);
        assert_eq!(response.engines[0].status, EngineStatus::Ok);
        assert!(response.results[0].score > response.results[1].score);
        config.search.engine_timeout = 100;
        let response = Pool::from_adapters(
            vec![(
                "selected",
                Settings::Command(executable("import time; time.sleep(30)", 4096)),
            )],
            config.search,
        )
        .search(Query::default())
        .await;
        assert_eq!(response.engines[0].name, "selected");
        assert_eq!(response.engines[0].status, EngineStatus::Timeout);
    }

    /// One bad row costs that row, not the engine; an answer with no valid row
    /// at all is malformed rather than a silent empty success.
    #[test]
    fn invalid_rows_are_skipped_and_counted() {
        let good = serde_json::json!({"title":"Title","url":"https://example.com/"});
        for bad in [
            serde_json::json!({"title":" ","url":"https://example.com/"}),
            serde_json::json!({"url":"https://example.com/"}),
            serde_json::json!({"title":"Title","url":"/relative"}),
            serde_json::json!({"title":"Title","url":"file:///tmp/page"}),
            serde_json::json!({"title":"Title","url":"https://user:password@example.com/"}),
            serde_json::json!({"title":"Title","url":"https://example.com/","snippet":8}),
        ] {
            let value = serde_json::json!({"results":[bad.clone(), good.clone()]});
            let found = rows("fixture", &value, Mapping::COMMAND, 10).unwrap();
            assert_eq!((found.links.len(), found.skipped), (1, 1), "{bad}");
            let value = serde_json::json!({"results":[bad]});
            assert!(rows("fixture", &value, Mapping::COMMAND, 10).is_err());
        }
        let empty = serde_json::json!({"results":[]});
        assert!(rows("fixture", &empty, Mapping::COMMAND, 10)
            .unwrap()
            .links
            .is_empty());
    }

    /// Regression: a live Mwmbl answer with one empty-titled row among many
    /// returned zero results because that row failed the whole engine.
    #[test]
    fn mwmbl_row_with_empty_title_does_not_fail_the_engine() {
        let Settings::Http(mwmbl) = crate::core::config::EngineSettings::default()
            .adapter("mwmbl")
            .unwrap()
        else {
            panic!("mwmbl is an HTTP preset")
        };
        let value = serde_json::json!([
            {"title":[{"value":"How to Tie a Tie"}],"url":"https://example.com/tie","extract":[{"value":"Steps"}]},
            {"title":[],"url":"https://example.com/untitled","extract":[{"value":"No title"}]},
            {"title":[{"value":"Knots"}],"url":"https://example.com/knots","extract":[]}
        ]);
        let mapping = Mapping {
            results: &mwmbl.results_pointer,
            title: &mwmbl.title_pointer,
            url: &mwmbl.url_pointer,
            snippet: &mwmbl.snippet_pointer,
            published: &mwmbl.published_pointer,
            snippets: &mwmbl.snippets_pointer,
            updated: &mwmbl.updated_pointer,
            text_part: mwmbl.text_part_pointer.as_deref(),
        };
        let found = rows("mwmbl", &value, mapping, 10).unwrap();
        assert_eq!(found.links.len(), 2);
        assert_eq!(found.skipped, 1);
        assert_eq!(found.links[0].title, "How to Tie a Tie");
    }

    /// The presets' mappings parse saved upstream answers.
    #[test]
    fn presets_map_saved_answers() {
        let mut settings = crate::core::config::EngineSettings::default();
        settings.config.insert(
            "searxng".into(),
            serde_json::json!({"url":"https://searx.example/search"}),
        );
        for (id, saved) in [
            (
                "mwmbl",
                include_str!("../../../tests/engines/fixtures/mwmbl.json"),
            ),
            (
                "searxng",
                include_str!("../../../tests/engines/fixtures/searxng.json"),
            ),
        ] {
            let Settings::Http(http) = settings.adapter(id).unwrap() else {
                panic!("{id} is an HTTP preset")
            };
            let mapping = Mapping {
                results: &http.results_pointer,
                title: &http.title_pointer,
                url: &http.url_pointer,
                snippet: &http.snippet_pointer,
                published: &http.published_pointer,
                snippets: &http.snippets_pointer,
                updated: &http.updated_pointer,
                text_part: http.text_part_pointer.as_deref(),
            };
            let value = serde_json::from_str(saved).unwrap();
            let found = rows(id, &value, mapping, 10).unwrap();
            assert_eq!(found.skipped, 0, "{id}");
            assert!(found.links[0].snippet.is_some(), "{id}");
        }
    }

    /// Linux exposes child liveness without adding a process-control dependency.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn timeout_and_cancellation_kill_the_direct_child() {
        for timeout in [true, false] {
            let path = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("search-child-{}", uuid::Uuid::new_v4()));
            let mut settings = executable(
                "import os,sys,time; open(sys.argv[1],'w').write(str(os.getpid())); time.sleep(30)",
                4096,
            );
            settings.args.push(path.to_string_lossy().into());
            let future = Adapter::new("slow".into(), Settings::Command(settings), "search-test")
                .unwrap()
                .search("q".into(), 1);
            let task = tokio::spawn(async move {
                if timeout {
                    assert!(tokio::time::timeout(Duration::from_millis(500), future)
                        .await
                        .is_err());
                } else {
                    let _ = future.await;
                }
            });
            let pid = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Ok(pid) = std::fs::read_to_string(&path) {
                        if !pid.is_empty() {
                            break pid;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            if !timeout {
                task.abort();
            }
            let _ = task.await;
            tokio::time::timeout(Duration::from_secs(3), async {
                while std::path::Path::new(&format!("/proc/{pid}")).exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            std::fs::remove_file(path).unwrap();
        }
    }

    /// The complete v2 request reaches stdin literally; passages and dates survive mapping.
    #[tokio::test]
    async fn command_v2_carries_filters_and_multiple_passages() {
        let mut settings = executable(
            r#"
import json,sys
r=json.load(sys.stdin)
assert r['version']==2
assert r['query']=='literal query'
assert r['filters']=={'freshness':'week','start_date':'2024-02-29','end_date':'2024-03-01','include_domains':['example.com'],'exclude_domains':['other.com'],'language':'en','region':'GB','category':'science','safe':False,'exact_phrase':'a & b'}
print(json.dumps({'results':[{'title':'Article','url':'https://example.com/','snippet':'first','snippets':['first','second'],'published':'2024-02-29','updated':'2024-03-01'}]}))
"#,
            4096,
        );
        settings.version = 2;
        let filters: crate::core::service::QueryFilters = serde_json::from_value(serde_json::json!({
            "freshness":"week", "start_date":"2024-02-29", "end_date":"2024-03-01",
            "include_domains":["example.com"], "exclude_domains":["other.com"],
            "language":"en", "region":"GB", "category":"science", "safe":false, "exact_phrase":"a & b"
        })).unwrap();
        let adapter = Adapter::new("fixture".into(), Settings::Command(settings), "test").unwrap();
        assert!(adapter.unsupported_filters(&filters).is_empty());
        let found = adapter
            .search_query(
                Query {
                    text: "literal query".into(),
                    filters,
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        assert_eq!(found.links[0].snippets, ["first", "second"]);
        assert_eq!(found.links[0].updated.as_deref(), Some("2024-03-01"));
        assert_eq!(found.links[0].published.as_deref(), Some("2024-02-29"));
    }

    /// Only configured HTTP filter mappings are transmitted; unsupported coverage is honest.
    #[tokio::test]
    async fn http_filter_parameters_override_static_values_and_report_unsupported() {
        let (url, server) = socket_response("200 OK", r#"{"results":[]}"#, "").await;
        let mut settings = http_settings(&format!("{url}?domains=stale&phrase=stale"));
        settings.params.insert("domains".into(), "static".into());
        settings
            .filter_params
            .insert("include_domains".into(), "domains".into());
        settings
            .filter_params
            .insert("exact_phrase".into(), "phrase".into());
        settings.filter_params.insert("safe".into(), "safe".into());
        let pool = Pool::from_adapters(
            vec![("fixture", Settings::Http(settings))],
            Default::default(),
        );
        let answer = pool
            .search(Query {
                text: "query".into(),
                filters: crate::core::service::QueryFilters {
                    include_domains: vec!["a.example".into(), "b.example".into()],
                    exact_phrase: Some("a & b".into()),
                    safe: Some(false),
                    start_date: Some("2024-02-29".into()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await;
        assert_eq!(answer.engines[0].unsupported_filters, ["start_date"]);
        assert_eq!(answer.engines[0].status, EngineStatus::Ok);
        let request = server.await.unwrap();
        assert!(
            request.contains("domains=a.example&domains=b.example"),
            "{request}"
        );
        assert!(request.contains("phrase=a+%26+b"));
        assert!(request.contains("safe=false"));
        assert!(
            !request.contains("stale")
                && !request.contains("static")
                && !request.contains("start_date")
        );
    }

    /// Merely declaring a mapping must not erase operator defaults when a
    /// request omits the filter; explicit values still replace those defaults.
    #[tokio::test]
    async fn absent_filters_preserve_static_parameter_defaults() {
        let (url, server) = socket_response("200 OK", r#"{"results":[]}"#, "").await;
        let mut settings = http_settings(&format!("{url}?language=de&safe=true"));
        settings.params.insert("language".into(), "en".into());
        settings
            .filter_params
            .insert("language".into(), "language".into());
        settings.filter_params.insert("safe".into(), "safe".into());
        let pool = Pool::from_adapters(
            vec![("fixture", Settings::Http(settings))],
            Default::default(),
        );
        let answer = pool.search(Query::default()).await;
        assert_eq!(answer.engines[0].status, EngineStatus::Ok);
        let request = server.await.unwrap();
        assert!(request.contains("safe=true"), "{request}");
        assert!(request.contains("language=en"), "{request}");
        assert!(!request.contains("language=de"), "{request}");
    }

    /// Explicit v1 stays byte-compatible and exposes every requested unsupported constraint.
    #[tokio::test]
    async fn legacy_command_reports_unsupported_filters() {
        let settings = executable(
            r#"
import sys,json
r=json.load(sys.stdin)
assert r=={'version':1,'query':'q','limit':10}
print('{"results":[]}')
"#,
            4096,
        );
        let pool = Pool::from_adapters(
            vec![("legacy", Settings::Command(settings))],
            Default::default(),
        );
        let answer = pool
            .search(Query {
                text: "q".into(),
                filters: crate::core::service::QueryFilters {
                    safe: Some(true),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await;
        assert_eq!(answer.engines[0].status, EngineStatus::Ok);
        assert_eq!(answer.engines[0].unsupported_filters, ["safe"]);
    }

    /// Serve a retryable rejection followed by success, recording every request.
    async fn retry_server(
        status: u16,
        delay: &str,
    ) -> (
        String,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/search", listener.local_addr().unwrap());
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let recorded = count.clone();
        let delay = delay.to_owned();
        let task = tokio::spawn(async move {
            for response in [format!("HTTP/1.1 {status} Rejected\r\nRetry-After: {delay}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"), "HTTP/1.1 200 OK\r\nContent-Length: 14\r\nConnection: close\r\n\r\n{\"results\":[]}".into()] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") { socket.read_exact(&mut byte).await.unwrap(); request.push(byte[0]); }
                recorded.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (url, count, task)
    }

    /// Zero and short permitted pauses retry once without shortening the server delay.
    #[tokio::test]
    async fn http_retries_429_and_503_with_permitted_retry_after() {
        for status in [429, 503] {
            for (delay, minimum) in [
                ("0", Duration::ZERO),
                ("1", Duration::from_secs(1)),
                ("Thu, 01 Jan 1970 00:00:00 GMT", Duration::ZERO),
            ] {
                let (url, count, server) = retry_server(status, delay).await;
                let mut settings = http_settings(&url);
                settings.retry_after_max = 1000;
                let start = std::time::Instant::now();
                let answer = tokio::time::timeout(
                    Duration::from_secs(3),
                    query_http("retry", &settings, "q", 1),
                )
                .await
                .unwrap()
                .unwrap();
                assert!(answer.is_empty());
                assert!(start.elapsed() >= minimum);
                assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
                server.await.unwrap();
            }
        }
    }

    /// A valid delay beyond the wait budget returns the original status and sends no retry.
    #[tokio::test]
    async fn http_preserves_rejection_for_oversized_retry_after() {
        for status in [429, 503] {
            for delay in ["2", "Thu, 01 Jan 2099 00:00:00 GMT", "18446744073709551615"] {
                let (url, count, server) = retry_server(status, delay).await;
                let mut settings = http_settings(&url);
                settings.retry_after_max = 20;
                let error = tokio::time::timeout(
                    Duration::from_secs(1),
                    query_http("retry", &settings, "q", 1),
                )
                .await
                .unwrap()
                .unwrap_err();
                assert_eq!(error.cause, super::super::FailureCause::Rejected);
                assert_eq!(
                    error.message,
                    format!("engine HTTP status {status} rejected")
                );
                // Observe beyond the old clamped wait to catch any early second request.
                tokio::time::sleep(Duration::from_millis(40)).await;
                assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
                server.abort();
                let _ = server.await;
            }
        }
    }

    /// Backoff sleep is cancelled at the engine deadline and when callers drop work.
    #[tokio::test]
    async fn backoff_deadline_and_cancellation_prevent_retry() {
        for cancellation in [false, true] {
            let (url, count, server) = retry_server(429, "1").await;
            let mut settings = http_settings(&url);
            settings.retry_after_max = 1000;
            let adapter = Adapter::new("retry".into(), Settings::Http(settings), "test").unwrap();
            let future = adapter.search("q".into(), 1);
            if cancellation {
                let task = tokio::spawn(future);
                tokio::time::timeout(Duration::from_secs(2), async {
                    while count.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                assert!(tokio::time::timeout(Duration::from_millis(100), future)
                    .await
                    .is_err());
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
            assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
            server.abort();
        }
    }

    /// Retry-After supports both standard encodings and ignores malformed dates.
    #[test]
    fn retry_after_preserves_delays_and_rejects_excessive_waits() {
        let mut headers = reqwest::header::HeaderMap::new();
        for (value, expected) in [
            ("18446744073709551615", None),
            ("0", Some(Duration::ZERO)),
            ("1", None),
            ("Thu, 01 Jan 2099 00:00:00 GMT", None),
            ("Thu, 01 Jan 1970 00:00:00 GMT", Some(Duration::ZERO)),
            ("bad", None),
            ("Thu, 31 Feb 2099 00:00:00 GMT", None),
        ] {
            headers.insert(reqwest::header::RETRY_AFTER, value.parse().unwrap());
            assert_eq!(retry_delay(&headers, 50), expected, "{value}");
        }
        headers.insert(reqwest::header::RETRY_AFTER, "1".parse().unwrap());
        assert_eq!(retry_delay(&headers, 999), None);
        assert_eq!(retry_delay(&headers, 1000), Some(Duration::from_secs(1)));
        assert_eq!(retry_delay(&headers, 1001), Some(Duration::from_secs(1)));
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "18446744073709551615".parse().unwrap(),
        );
        assert_eq!(retry_delay(&headers, u64::MAX), None);
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Thu, 01 Jan 2099 00:00:00 GMT".parse().unwrap(),
        );
        let date = http_date("Thu, 01 Jan 2099 00:00:00 GMT").unwrap();
        let before = std::time::SystemTime::now();
        let delay = retry_delay(&headers, u64::MAX).unwrap();
        let after = std::time::SystemTime::now();
        assert!(delay <= date.duration_since(before).unwrap());
        assert!(delay >= date.duration_since(after).unwrap());
        assert_eq!(retry_delay(&headers, 0), None);
        assert_eq!(
            http_date("Thu, 01 Jan 1970 00:00:00 GMT"),
            Some(std::time::UNIX_EPOCH)
        );
    }
    /// Command v2 may explicitly decline constraints; arbitrary response strings never escape.
    #[tokio::test]
    async fn command_v2_reports_declined_filters_without_leaking_unknown_values() {
        let mut settings = executable(
            r#"
import json,sys
json.load(sys.stdin)
print(json.dumps({'results':[], 'unsupported_filters':['safe','secret-value','safe','language']}))
"#,
            4096,
        );
        settings.version = 2;
        let pool = Pool::from_adapters(
            vec![("fixture", Settings::Command(settings))],
            Default::default(),
        );
        let answer = pool
            .search(Query {
                filters: crate::core::service::QueryFilters {
                    safe: Some(true),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await;
        assert_eq!(answer.engines[0].unsupported_filters, ["safe"]);
        assert_eq!(answer.engines[0].status, EngineStatus::Ok);
        assert!(!serde_json::to_string(&answer)
            .unwrap()
            .contains("secret-value"));
    }
}
