//! Deterministic, bounded same-origin discovery through the guarded fetcher.
//! Robots and sitemap reads consume the same byte, deadline and delay budget as pages.

use crate::core::{
    fetch::{Fetcher, RawPage},
    Page, Search,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

/// Request ceilings; zero selects the configured operator ceiling.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CrawlOptions {
    pub max_pages: usize,
    pub max_depth: usize,
    pub max_bytes: u64,
    pub timeout: u64,
    pub path_prefix: Option<String>,
    /// Machine-surface reading controls; core traversal retains full clean text.
    pub max_characters: usize,
    pub total_characters: usize,
    pub full: bool,
}

/// Machine-window metadata for one crawled page. The corresponding page text
/// is bounded without confusing a reader window with an incomplete download.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlWindow {
    pub url: String,
    pub returned_characters: usize,
    pub available_characters: usize,
    pub next_offset: Option<usize>,
}

/// The root URL and bounds of one crawl or map.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrawlRequest {
    pub url: String,
    #[serde(default)]
    pub options: CrawlOptions,
}

/// A refused or unsuccessful URL, including policy discovery failures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlFailure {
    pub url: String,
    pub error: String,
}

/// Bounded discovery results; map omits page content, retaining the URL inventory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlReport {
    pub root: String,
    pub urls: Vec<String>,
    pub pages: Vec<Page>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<CrawlWindow>,
    pub failures: Vec<CrawlFailure>,
    pub visited: usize,
    pub bytes: u64,
    pub timed_out: bool,
    pub truncated: bool,
    pub duration_ms: u64,
}

impl Search {
    /// Read a deterministic BFS of scoped pages with fail-closed robots policy.
    pub async fn crawl(&self, request: CrawlRequest) -> CrawlReport {
        run(&self.fetcher, self.config(), request, false).await
    }

    /// Discover the same bounded graph as crawl, returning only its URL inventory.
    pub async fn map(&self, request: CrawlRequest) -> CrawlReport {
        run(&self.fetcher, self.config(), request, true).await
    }

    /// Fetch with opt-in navigation targets; ordinary fetch keeps links empty.
    pub async fn fetch_with_links(&self, urls: &[String], links: bool) -> Vec<Page> {
        if !links {
            return self.fetch(urls).await;
        }
        let mut pages = Vec::with_capacity(urls.len());
        for url in urls {
            match self.fetcher.fetch_with_links(url).await {
                Ok(page) => pages.push(page),
                Err(error) => {
                    pages.push(Page {
                        url: url.clone(),
                        fetched_at: None,
                        final_url: None,
                        status: error.status(),
                        content_type: String::new(),
                        title: None,
                        byline: None,
                        published: None,
                        site: None,
                        text: String::new(),
                        links: Vec::new(),
                        truncated: None,
                        redirect: None,
                        error: Some(error.message()),
                    });
                }
            }
        }
        pages
    }
}

/// Page paths are scoped; robots has its well-known exception and sitemap
/// documents may live anywhere on the same origin.
#[derive(Clone, Copy)]
enum DocumentScope {
    Page,
    Robots,
    Sitemap,
}

/// Same-origin paths are compared by segment, preventing `/docs` matching
/// `/docstore`; scoped page paths reject server-decoded separator ambiguity.
#[derive(Clone)]
struct Scope {
    root: Url,
    prefix: Option<String>,
}
impl Scope {
    /// Policy documents stay same-origin; page URLs also obey the path prefix.
    fn contains(&self, url: &Url, kind: DocumentScope) -> bool {
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.origin() == self.root.origin()
            && crate::core::fetch::navigation_target(url)
            && match kind {
                DocumentScope::Sitemap => true,
                DocumentScope::Robots => url.path() == "/robots.txt" && url.query().is_none(),
                DocumentScope::Page => self.prefix.as_ref().is_none_or(|prefix| {
                    // Encoded separators can become traversal after server decoding.
                    if url.path().as_bytes().windows(3).any(|escape| {
                        escape.eq_ignore_ascii_case(b"%2f") || escape.eq_ignore_ascii_case(b"%5c")
                    }) {
                        return false;
                    }
                    let prefix = prefix.trim_end_matches('/');
                    url.path() == prefix
                        || url
                            .path()
                            .strip_prefix(prefix)
                            .is_some_and(|tail| tail.starts_with('/'))
                }),
            }
    }
}

/// A robots group selected by the longest matching user-agent token.
#[derive(Default)]
struct Robots {
    rules: Vec<(String, bool)>,
    sitemaps: Vec<String>,
    delay: Duration,
}
/// One parsed agent group, retaining rules until the best matching token is known.
struct RobotsGroup {
    agents: Vec<String>,
    rules: Vec<(String, bool)>,
    delay: Duration,
}
impl Robots {
    /// Parse groups, combining equally specific matches; missing/empty rules allow.
    fn parse(text: &str, agent: &str, deadline: PolicyDeadline) -> Result<Self, String> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut groups = Vec::new();
        let mut agents = Vec::new();
        let mut rules = Vec::new();
        let mut delay = Duration::ZERO;
        let mut directives = false;
        let mut sitemaps = Vec::new();
        for line in text.lines() {
            deadline.check()?;
            let line = line.split('#').next().unwrap_or_default().trim();
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim().to_ascii_lowercase().as_str() {
                "user-agent" => {
                    if directives {
                        groups.push(RobotsGroup {
                            agents: std::mem::take(&mut agents),
                            rules: std::mem::take(&mut rules),
                            delay,
                        });
                        delay = Duration::ZERO;
                        directives = false;
                    }
                    agents.push(value.to_ascii_lowercase());
                }
                "allow" | "disallow" if !agents.is_empty() => {
                    directives = true;
                    if !value.is_empty() {
                        rules.push((
                            robots_normalize(value, deadline)?,
                            key.trim().eq_ignore_ascii_case("allow"),
                        ));
                    }
                }
                "crawl-delay" if !agents.is_empty() => {
                    directives = true;
                    if let Ok(seconds) = value.parse::<f64>() {
                        if seconds.is_finite() && seconds >= 0.0 {
                            delay = Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX);
                        }
                    }
                }
                "sitemap" => sitemaps.push(value.to_string()),
                _ => {}
            }
        }
        groups.push(RobotsGroup {
            agents,
            rules,
            delay,
        });
        let agent = agent
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .split('/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let specificity = |agents: &[String]| {
            agents
                .iter()
                .filter_map(|token| {
                    if token == "*" {
                        Some(0)
                    } else if !token.is_empty() && agent.contains(token) {
                        Some(token.len())
                    } else {
                        None
                    }
                })
                .max()
        };
        let best = groups
            .iter()
            .filter_map(|group| specificity(&group.agents))
            .max();
        let mut result = Self {
            sitemaps,
            ..Self::default()
        };
        for group in groups {
            deadline.check()?;
            if best.is_some() && specificity(&group.agents) == best {
                result.rules.extend(group.rules);
                result.delay = result.delay.max(group.delay);
            }
        }
        deadline.check()?;
        Ok(result)
    }

    /// Longest matching rule wins, with Allow winning equal specificity.
    fn allows(&self, url: &Url, deadline: PolicyDeadline) -> Result<bool, String> {
        deadline.check()?;
        let path = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_string(),
        };
        let path = robots_normalize(&path, deadline)?;
        let mut best = None;
        for (pattern, allow) in &self.rules {
            deadline.check()?;
            if robots_match(pattern, &path, deadline)? {
                let rank = (
                    pattern
                        .bytes()
                        .filter(|c| !matches!(c, b'*' | b'$'))
                        .count(),
                    *allow,
                );
                best = Some(best.map_or(rank, |previous| std::cmp::max(previous, rank)));
            }
        }
        deadline.check()?;
        Ok(best.is_none_or(|(_, allow)| allow))
    }

    /// Evaluate policy off the runtime, sharing the crawl deadline and fetcher
    /// concurrency slot. Matching workers also stop cooperatively between rules.
    async fn permits(
        self: &Arc<Self>,
        fetcher: &Fetcher,
        budget: &Budget,
        url: &Url,
    ) -> Result<bool, String> {
        let policy = self.clone();
        let url = url.clone();
        let deadline = PolicyDeadline {
            started: budget.started,
            timeout: budget.timeout,
        };
        fetcher
            .crawl_work(
                budget.timeout.saturating_sub(budget.started.elapsed()),
                move || policy.allows(&url, deadline),
            )
            .await
            .map_err(|error| error.message())?
    }
}

/// Cooperative stop shared by policy parsing, normalization and matching workers.
#[derive(Clone, Copy)]
struct PolicyDeadline {
    started: Instant,
    timeout: Duration,
}
impl PolicyDeadline {
    /// Refuse further policy work once the whole-call budget expires.
    fn check(self) -> Result<(), String> {
        if self.started.elapsed() >= self.timeout {
            Err("crawl policy deadline reached".into())
        } else {
            Ok(())
        }
    }
}

/// Normalize RFC 9309 comparison octets: decode only ASCII unreserved escapes,
/// uppercase retained escapes, and percent-encode raw UTF-8 bytes. Encoded reserved
/// characters remain distinct from their literal syntax (including `*` and `$`).
fn robots_normalize(value: &str, deadline: PolicyDeadline) -> Result<String, String> {
    let mut output = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut cursor = 0usize;
    let mut next_check = 0usize;
    while let Some(&byte) = bytes.get(cursor) {
        if cursor >= next_check {
            deadline.check()?;
            next_check = cursor.saturating_add(256);
        }
        if byte == b'%' {
            if let Some(decoded) = bytes
                .get(cursor + 1)
                .and_then(|hi| char::from(*hi).to_digit(16))
                .zip(
                    bytes
                        .get(cursor + 2)
                        .and_then(|lo| char::from(*lo).to_digit(16)),
                )
                .map(|(hi, lo)| (hi * 16 + lo) as u8)
            {
                if decoded.is_ascii_alphanumeric() || matches!(decoded, b'-' | b'.' | b'_' | b'~') {
                    output.push(char::from(decoded));
                } else {
                    percent_octet(&mut output, decoded);
                }
                cursor += 3;
                continue;
            }
        }
        if byte.is_ascii() {
            output.push(char::from(byte));
        } else {
            percent_octet(&mut output, byte);
        }
        cursor += 1;
    }
    deadline.check()?;
    Ok(output)
}

/// Append one octet as an uppercase percent escape, without interpreting syntax.
fn percent_octet(output: &mut String, byte: u8) {
    output.push('%');
    output.push(
        char::from_digit(u32::from(byte >> 4), 16)
            .unwrap_or('0')
            .to_ascii_uppercase(),
    );
    output.push(
        char::from_digit(u32::from(byte & 15), 16)
            .unwrap_or('0')
            .to_ascii_uppercase(),
    );
}

/// Linear wildcard matching by searching literal segments in successive disjoint
/// suffixes. `str::find` uses substring search, never wildcard suffix backtracking;
/// a terminal anchored segment is compared directly with the remaining suffix.
fn robots_match(pattern: &str, path: &str, deadline: PolicyDeadline) -> Result<bool, String> {
    deadline.check()?;
    let anchored = pattern.ends_with('$');
    let pattern = pattern.strip_suffix('$').unwrap_or(pattern);
    let mut segments = pattern.split('*').peekable();
    let prefix = segments.next().unwrap_or_default();
    if !path.starts_with(prefix) {
        return Ok(false);
    }
    let mut remaining = path.get(prefix.len()..).unwrap_or_default();
    if segments.peek().is_none() {
        return Ok(!anchored || remaining.is_empty());
    }
    while let Some(segment) = segments.next() {
        deadline.check()?;
        if anchored && segments.peek().is_none() {
            let matches = remaining.ends_with(segment);
            deadline.check()?;
            return Ok(matches);
        }
        let position = remaining.find(segment);
        deadline.check()?;
        let Some(position) = position else {
            return Ok(false);
        };
        remaining = remaining
            .get(position + segment.len()..)
            .unwrap_or_default();
    }
    deadline.check()?;
    Ok(true)
}

/// Shared work accounting, including unsuccessful bodies and policy requests.
struct Budget {
    started: Instant,
    timeout: Duration,
    max_bytes: u64,
    bytes: u64,
    delay: Duration,
    settings: crate::core::config::CrawlSettings,
    timed_out: bool,
    truncated: bool,
}
impl Budget {
    /// One request cannot outlive the run; no hidden redirect or policy fetch occurs.
    async fn read(&mut self, fetcher: &Fetcher, url: &Url) -> Result<RawPage, String> {
        let remaining = self.timeout.saturating_sub(self.started.elapsed());
        if remaining.is_zero() {
            self.timed_out = true;
            return Err("crawl deadline reached".into());
        }
        if self.bytes >= self.max_bytes {
            self.truncated = true;
            return Err("crawl byte budget reached".into());
        }
        let result = tokio::time::timeout(remaining, async {
            fetcher
                .crawl_response(
                    url,
                    self.max_bytes.saturating_sub(self.bytes),
                    &mut self.bytes,
                    &self.settings,
                    self.delay,
                    tokio::time::Instant::now()
                        + self.timeout.saturating_sub(self.started.elapsed()),
                )
                .await
        })
        .await;
        match result {
            Ok(Ok(raw)) => {
                self.truncated |= raw.truncated;
                Ok(raw)
            }
            Ok(Err(error)) => {
                self.timed_out |= self.started.elapsed() >= self.timeout;
                Err(error.message())
            }
            Err(_) => {
                self.timed_out = true;
                Err("crawl deadline reached".into())
            }
        }
    }
}

/// Resolve zero defaults and clamp requested work to operator ceilings.
fn ceiling(value: u64, maximum: u64) -> u64 {
    if value == 0 {
        maximum
    } else {
        value.min(maximum)
    }
}

/// Normalize fragments before graph deduplication.
fn normalized(mut url: Url) -> Url {
    url.set_fragment(None);
    url
}

/// Follow only authorized hops, spending the common budget before every request.
async fn retrieve(
    fetcher: &Fetcher,
    budget: &mut Budget,
    scope: &Scope,
    robots: Option<&Arc<Robots>>,
    url: Url,
    kind: DocumentScope,
    redirects: &mut usize,
) -> Result<(Url, RawPage), String> {
    let mut target = url;
    loop {
        if !scope.contains(&target, kind) {
            return Err("URL outside crawl scope".into());
        }
        if let Some(robots) = robots {
            if !robots.permits(fetcher, budget, &target).await? {
                return Err("robots disallow".into());
            }
        }
        let raw = budget.read(fetcher, &target).await?;
        if (300..400).contains(&raw.status) {
            if *redirects >= fetcher.redirect_limit() {
                return Err("too many redirects".into());
            }
            *redirects += 1;
            let location = raw.location.as_deref().ok_or("redirect without Location")?;
            target = normalized(target.join(location).map_err(|_| "invalid redirect")?);
        } else {
            return Ok((target, raw));
        }
    }
}

/// Extract a bounded flat sitemap; indexes enqueue sitemap documents, never recurse.
fn sitemap(text: &str, base: &Url, scope: &Scope, maximum: usize) -> (bool, Vec<Url>, bool) {
    let document = dom_query::Document::from(text);
    let index = !document.select("sitemapindex").is_empty();
    let mut urls = BTreeSet::new();
    for node in document
        .select("loc")
        .nodes()
        .iter()
        .take(maximum.saturating_add(1))
    {
        if let Ok(url) = base.join(node.text().trim()) {
            let url = normalized(url);
            if scope.contains(
                &url,
                if index {
                    DocumentScope::Sitemap
                } else {
                    DocumentScope::Page
                },
            ) {
                urls.insert(url.to_string());
            }
        }
    }
    let truncated = document.select("loc").nodes().len() > maximum;
    (
        index,
        urls.into_iter()
            .take(maximum)
            .filter_map(|url| Url::parse(&url).ok())
            .collect(),
        truncated,
    )
}

/// Run serial BFS so results and work accounting do not depend on scheduling.
async fn run(
    fetcher: &Fetcher,
    config: &crate::core::Config,
    request: CrawlRequest,
    map: bool,
) -> CrawlReport {
    let started = Instant::now();
    let settings = &config.crawl;
    let max_pages = ceiling(request.options.max_pages as u64, settings.max_pages as u64) as usize;
    let max_depth = ceiling(request.options.max_depth as u64, settings.max_depth as u64) as usize;
    let mut budget = Budget {
        started,
        timeout: Duration::from_millis(ceiling(request.options.timeout, settings.timeout)),
        max_bytes: ceiling(request.options.max_bytes, settings.max_bytes),
        bytes: 0,
        delay: Duration::from_millis(settings.delay),
        settings: settings.clone(),
        timed_out: false,
        truncated: false,
    };
    let mut report = CrawlReport {
        root: request.url.clone(),
        urls: Vec::new(),
        pages: Vec::new(),
        windows: Vec::new(),
        failures: Vec::new(),
        visited: 0,
        bytes: 0,
        timed_out: false,
        truncated: false,
        duration_ms: 0,
    };
    let result = async {
        let root =
            normalized(Url::parse(&request.url).map_err(|_| "invalid crawl URL".to_string())?);
        if request
            .options
            .path_prefix
            .as_ref()
            .is_some_and(|p| !p.starts_with('/') || p.contains(['?', '#']))
        {
            return Err("path_prefix must be an absolute URL path".into());
        }
        let scope = Scope {
            root: root.clone(),
            prefix: request.options.path_prefix,
        };
        if !scope.contains(&root, DocumentScope::Page) {
            return Err("root outside crawl scope".into());
        }
        let robots_url = root.join("/robots.txt").map_err(|_| "invalid robots URL")?;
        let (_, raw) = retrieve(
            fetcher,
            &mut budget,
            &scope,
            None,
            robots_url,
            DocumentScope::Robots,
            &mut 0,
        )
        .await?;
        let robots = if raw.status == 404 || raw.status == 410 {
            Robots::default()
        } else if (200..300).contains(&raw.status) && !raw.truncated {
            let agent = config.user_agent.clone();
            let deadline = PolicyDeadline {
                started,
                timeout: budget.timeout,
            };
            fetcher
                .crawl_work(
                    budget.timeout.saturating_sub(started.elapsed()),
                    move || {
                        let text = std::str::from_utf8(&raw.body)
                            .map_err(|_| "robots unreadable".to_string())?;
                        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
                        if text.trim_start().starts_with('<')
                            || text
                                .chars()
                                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
                        {
                            return Err("robots unreadable".to_string());
                        }
                        Robots::parse(text, &agent, deadline)
                    },
                )
                .await
                .map_err(|e| e.message())??
        } else {
            return Err(format!(
                "robots unavailable (HTTP {}, truncated={})",
                raw.status, raw.truncated
            ));
        };
        let robots = Arc::new(robots);
        budget.delay = budget.delay.max(robots.delay);
        if !robots.permits(fetcher, &budget, &root).await? {
            return Err("robots disallow root".into());
        }
        let mut queue = VecDeque::from([(root.clone(), 0usize)]);
        let mut seen = BTreeSet::from([root.to_string()]);
        // Sitemap requests and queued URLs are bounded by the same page ceiling.
        let mut maps = VecDeque::new();
        for target in robots.sitemaps.iter().take(max_pages) {
            if let Ok(url) = root.join(target) {
                if scope.contains(&url, DocumentScope::Sitemap) {
                    maps.push_back(normalized(url));
                }
            }
        }
        if maps.is_empty() {
            if let Ok(url) = root.join("/sitemap.xml") {
                if scope.contains(&url, DocumentScope::Sitemap) {
                    maps.push_back(url);
                }
            }
        }
        let mut maps_seen = BTreeSet::new();
        while maps_seen.len() < max_pages && max_depth > 0 && seen.len() < max_pages {
            let Some(url) = maps.pop_front() else { break };
            if !maps_seen.insert(url.to_string()) {
                continue;
            }
            match retrieve(
                fetcher,
                &mut budget,
                &scope,
                Some(&robots),
                url.clone(),
                DocumentScope::Sitemap,
                &mut 0,
            )
            .await
            {
                Ok((base, raw)) if (200..300).contains(&raw.status) && !raw.truncated => {
                    let map_scope = scope.clone();
                    let parsed = fetcher
                        .crawl_work(
                            budget.timeout.saturating_sub(started.elapsed()),
                            move || {
                                sitemap(
                                    &String::from_utf8_lossy(&raw.body),
                                    &base,
                                    &map_scope,
                                    max_pages,
                                )
                            },
                        )
                        .await;
                    let (index, found, clipped) = match parsed {
                        Ok(found) => found,
                        Err(error) => {
                            report.failures.push(CrawlFailure {
                                url: url.to_string(),
                                error: error.message(),
                            });
                            break;
                        }
                    };
                    budget.truncated |= clipped;
                    for url in found {
                        if index {
                            if maps.len() + maps_seen.len() < max_pages {
                                maps.push_back(url);
                            } else {
                                budget.truncated = true;
                            }
                        } else if !seen.contains(url.as_str())
                            && robots.permits(fetcher, &budget, &url).await?
                        {
                            if seen.len() < max_pages {
                                seen.insert(url.to_string());
                                queue.push_back((url, 1));
                            } else {
                                budget.truncated = true;
                            }
                        }
                    }
                }
                Ok((_, raw)) if raw.status == 404 || raw.status == 410 => {}
                Ok(_) => report.failures.push(CrawlFailure {
                    url: url.to_string(),
                    error: "sitemap unavailable or truncated".into(),
                }),
                Err(error) => report.failures.push(CrawlFailure {
                    url: url.to_string(),
                    error,
                }),
            }
            if budget.timed_out || budget.bytes >= budget.max_bytes {
                break;
            }
        }
        let mut finals = BTreeSet::new();
        while let Some((url, depth)) = queue.pop_front() {
            if report.visited >= max_pages || budget.bytes >= budget.max_bytes || budget.timed_out {
                budget.truncated = true;
                break;
            }
            if !robots.permits(fetcher, &budget, &url).await? {
                report.failures.push(CrawlFailure {
                    url: url.to_string(),
                    error: "robots disallow".into(),
                });
                continue;
            }
            report.visited += 1;
            let outcome = async {
                let mut target = url.clone();
                let mut redirects = 0;
                // Refreshes share the HTTP hop ceiling rather than spawning an unbounded chain.
                for _ in 0..=fetcher.redirect_limit() {
                    let (final_url, raw) = retrieve(
                        fetcher,
                        &mut budget,
                        &scope,
                        Some(&robots),
                        target,
                        DocumentScope::Page,
                        &mut redirects,
                    )
                    .await?;
                    if !(200..300).contains(&raw.status) {
                        return Err(format!("HTTP {}", raw.status));
                    }
                    let page = fetcher
                        .crawl_page(
                            &final_url,
                            raw,
                            budget.timeout.saturating_sub(started.elapsed()),
                        )
                        .await
                        .map_err(|e| e.message())?;
                    if let Some(next) = page.redirect.as_ref() {
                        if redirects >= fetcher.redirect_limit() {
                            return Err("too many redirects".into());
                        }
                        redirects += 1;
                        target = normalized(final_url.join(next).map_err(|_| "invalid refresh")?);
                        continue;
                    }
                    return Ok((final_url, page));
                }
                Err("too many refresh redirects".into())
            }
            .await;
            match outcome {
                Ok((final_url, mut page)) => {
                    if !finals.insert(final_url.to_string()) {
                        continue;
                    }
                    report.urls.push(final_url.to_string());
                    let discovery = async {
                        for target in &page.links {
                            let Ok(target) = Url::parse(target) else {
                                continue;
                            };
                            if !scope.contains(&target, DocumentScope::Page)
                                || seen.contains(target.as_str())
                                || !robots.permits(fetcher, &budget, &target).await?
                            {
                                continue;
                            }
                            if depth >= max_depth || seen.len() >= max_pages {
                                budget.truncated = true;
                                continue;
                            }
                            seen.insert(target.to_string());
                            queue.push_back((target, depth + 1));
                        }
                        Ok::<(), String>(())
                    }
                    .await;
                    page.url = url.to_string();
                    if !map {
                        report.pages.push(page);
                    }
                    discovery?;
                }
                Err(error) => report.failures.push(CrawlFailure {
                    url: url.to_string(),
                    error,
                }),
            }
            if started.elapsed() >= budget.timeout {
                budget.timed_out = true;
            }
        }
        Ok::<(), String>(())
    }
    .await;
    if let Err(error) = result {
        report.failures.push(CrawlFailure {
            url: request.url,
            error,
        });
    }
    budget.timed_out |= started.elapsed() >= budget.timeout;
    report.bytes = budget.bytes;
    report.timed_out = budget.timed_out;
    report.truncated = budget.truncated || budget.timed_out;
    report.duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A generous worker deadline for focused policy semantics tests.
    fn policy_deadline() -> PolicyDeadline {
        PolicyDeadline {
            started: Instant::now(),
            timeout: Duration::from_secs(5),
        }
    }
    fn test_robots(text: &str, agent: &str) -> Robots {
        Robots::parse(text, agent, policy_deadline()).unwrap()
    }
    fn test_allowed(robots: &Robots, url: &Url) -> bool {
        robots.allows(url, policy_deadline()).unwrap()
    }

    /// Serve a closed set of routes and record every request, with no public network.
    struct Site {
        root: String,
        hits: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Site {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    impl Site {
        async fn new(routes: &[(&str, u16, &str, &str)]) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let root = format!("http://{}", listener.local_addr().unwrap());
            let routes: HashMap<String, (u16, String, String)> = routes
                .iter()
                .map(|(path, status, headers, body)| {
                    (
                        path.to_string(),
                        (*status, headers.to_string(), body.to_string()),
                    )
                })
                .collect();
            let hits = Arc::new(Mutex::new(Vec::new()));
            let recorded = hits.clone();
            let task = tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let mut bytes = vec![0; 8192];
                    let size = socket.read(&mut bytes).await.unwrap();
                    let request = String::from_utf8_lossy(&bytes[..size]);
                    let path = request.split_whitespace().nth(1).unwrap().to_string();
                    recorded.lock().unwrap().push(path.clone());
                    let (status, headers, body) =
                        routes
                            .get(&path)
                            .cloned()
                            .unwrap_or((404, String::new(), String::new()));
                    let response = format!("HTTP/1.1 {status} fixture\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}", body.len());
                    let _ = socket.write_all(response.as_bytes()).await;
                }
            });
            Self { root, hits, task }
        }
        fn search(&self) -> Search {
            let mut config = crate::core::Config::default();
            config.fetch.allow_private_networks = true;
            config.crawl.delay = 0;
            Search::open(config).unwrap()
        }
        fn request(&self, path: &str) -> CrawlRequest {
            CrawlRequest {
                url: format!("{}{path}", self.root),
                options: CrawlOptions::default(),
            }
        }
        fn paths(&self) -> Vec<String> {
            self.hits.lock().unwrap().clone()
        }
    }

    #[test]
    fn robots_longest_allow_wildcards_and_terminal_patterns() {
        let rules = test_robots("User-agent: *\nDisallow: /private\nAllow: /private/public\nDisallow: /*.pdf$\nAllow: /allowed.pdf$", "Search/1");
        for (path, allowed) in [
            ("/private/a", false),
            ("/private/public/a", true),
            ("/any.pdf", false),
            ("/any.pdf?download", true),
            ("/allowed.pdf", true),
        ] {
            assert_eq!(
                test_allowed(
                    &rules,
                    &Url::parse(&format!("https://example.com{path}")).unwrap()
                ),
                allowed,
                "{path}"
            );
        }
    }

    #[test]
    fn robots_specific_groups_override_star_and_merge_equal_matches() {
        let rules = test_robots("User-agent: *\nDisallow: /\nUser-agent: search\nDisallow: /one\nUser-agent: other\nDisallow: /three\nUser-agent: search\nDisallow: /two\nAllow: /one/open", "Search/1");
        for (path, allowed) in [
            ("/", true),
            ("/one", false),
            ("/two", false),
            ("/three", true),
            ("/one/open", true),
        ] {
            assert_eq!(
                test_allowed(
                    &rules,
                    &Url::parse(&format!("https://example.com{path}")).unwrap()
                ),
                allowed
            );
        }
    }

    /// Policy redirects must never interpret an ordinary scoped page as robots.
    #[tokio::test]
    async fn robots_redirect_cannot_use_page_scope_exception() {
        for prefix in [None, Some("/docs".to_string())] {
            let site = Site::new(&[
                ("/robots.txt", 302, "Location: /docs/policy\r\n", ""),
                ("/docs/policy", 200, "", "User-agent: *\nAllow: /"),
            ])
            .await;
            let mut request = site.request("/docs");
            request.options.path_prefix = prefix;
            let report = site.search().crawl(request).await;
            assert_eq!(site.paths(), ["/robots.txt"]);
            assert_eq!(report.visited, 0);
            assert!(report.failures[0].error.contains("scope"));
        }
    }

    /// A UTF-8 BOM does not hide the first user-agent group and its restrictions.
    #[test]
    fn robots_bom_does_not_turn_disallow_into_allow() {
        let rules = test_robots("\u{feff}User-agent: *\nDisallow: /", "Search/1");
        assert!(!test_allowed(
            &rules,
            &Url::parse("https://example.com/").unwrap()
        ));
    }

    #[tokio::test]
    async fn robots_disallow_prevents_network_access() {
        let site = Site::new(&[("/robots.txt", 200, "", "User-agent: *\nDisallow: /secret")]).await;
        let report = site.search().crawl(site.request("/secret")).await;
        assert_eq!(site.paths(), ["/robots.txt"]);
        assert_eq!(report.visited, 0);
        assert!(report.failures[0].error.contains("robots disallow"));
    }

    #[tokio::test]
    async fn robots_failure_and_truncation_fail_closed() {
        for status in [403, 500] {
            let site = Site::new(&[("/robots.txt", status, "", "unavailable")]).await;
            let report = site.search().crawl(site.request("/")).await;
            assert_eq!(site.paths(), ["/robots.txt"]);
            assert!(report.urls.is_empty());
        }
        let site = Site::new(&[("/robots.txt", 200, "", "User-agent: *\nDisallow: /")]).await;
        let mut request = site.request("/");
        request.options.max_bytes = 5;
        let report = site.search().crawl(request).await;
        assert!(report.truncated);
        assert_eq!(report.bytes, 5);
        assert_eq!(site.paths(), ["/robots.txt"]);
    }

    #[tokio::test]
    async fn bfs_is_sorted_deduplicated_and_map_omits_content() {
        let site = Site::new(&[
            ("/", 200, "Content-Type: text/html\r\n", "<body><p>Root.</p><a href='/b'>B</a><a href='/a#fragment'>A</a><a href='/a'>A</a></body>"),
            ("/a", 200, "Content-Type: text/html\r\n", "<body>A<a href='/c'>C</a></body>"),
            ("/b", 200, "Content-Type: text/plain\r\n", "B"),
            ("/c", 200, "Content-Type: text/plain\r\n", "C"),
        ]).await;
        let search = site.search();
        let crawl = search.crawl(site.request("/")).await;
        let map = search.map(site.request("/")).await;
        assert_eq!(
            crawl.urls,
            ["/", "/a", "/b", "/c"].map(|p| format!("{}{p}", site.root))
        );
        assert_eq!(map.urls, crawl.urls);
        assert_eq!(map.visited, crawl.visited);
        assert_eq!(map.bytes, crawl.bytes);
        assert!(map.pages.is_empty());
        assert_eq!(crawl.pages.len(), 4);
    }

    #[tokio::test]
    async fn redirects_cannot_dial_outside_origin_path_or_robots() {
        for location in [
            "http://example.invalid/",
            "/docstore",
            "/docs/private",
            "/docs/%2e%2e%2fprivate",
            "/docs/%5C..%5Cprivate",
        ] {
            let headers = format!("Location: {location}\r\n");
            let site = Site::new(&[
                (
                    "/robots.txt",
                    200,
                    "",
                    "User-agent: *\nDisallow: /docs/private",
                ),
                ("/docs", 302, &headers, ""),
            ])
            .await;
            let mut request = site.request("/docs");
            request.options.path_prefix = Some("/docs".into());
            let report = site.search().crawl(request).await;
            assert_eq!(site.paths(), ["/robots.txt", "/sitemap.xml", "/docs"]);
            assert_eq!(report.visited, 1);
            assert!(report.urls.is_empty());
            assert_eq!(report.failures.len(), 1);
        }
    }

    #[tokio::test]
    async fn private_root_is_refused_before_connect() {
        let site = Site::new(&[]).await;
        let search = Search::open(crate::core::Config::default()).unwrap();
        let report = search.crawl(site.request("/")).await;
        assert!(report.failures[0].error.contains("refusing"));
        assert!(site.paths().is_empty());
        assert_eq!(report.bytes, 0);
    }

    #[tokio::test]
    async fn page_and_depth_ceilings_stop_discovery() {
        let site = Site::new(&[
            (
                "/",
                200,
                "Content-Type: text/html\r\n",
                "<a href='/a'>A</a><a href='/b'>B</a>",
            ),
            (
                "/a",
                200,
                "Content-Type: text/html\r\n",
                "<a href='/c'>C</a>",
            ),
            ("/b", 200, "", "B"),
        ])
        .await;
        let mut request = site.request("/");
        request.options.max_pages = 2;
        let report = site.search().crawl(request).await;
        assert_eq!(report.visited, 2);
        assert!(report.truncated);
        assert!(!site.paths().contains(&"/b".into()));
        let mut request = site.request("/");
        request.options.max_depth = 1;
        let report = site.search().crawl(request).await;
        assert_eq!(report.visited, 3);
        assert!(report.truncated);
        assert!(!site.paths().contains(&"/c".into()));
    }

    #[tokio::test]
    async fn byte_budget_charges_policy_and_page_bodies() {
        let site = Site::new(&[
            ("/robots.txt", 200, "", "User-agent: *\n"),
            (
                "/",
                200,
                "Content-Type: text/plain\r\n",
                "abcdefghijklmnopqrstuvwxyz",
            ),
        ])
        .await;
        let mut request = site.request("/");
        request.options.max_bytes = 20;
        let report = site.search().crawl(request).await;
        assert_eq!(report.bytes, 20);
        assert!(report.truncated);
        assert_eq!(report.pages[0].text, "abcdef");
        assert_eq!(report.pages[0].truncated, Some(true));
    }

    #[tokio::test]
    async fn deadline_includes_robots_delay_and_prevents_late_requests() {
        let site = Site::new(&[("/robots.txt", 200, "", "User-agent: *\nCrawl-delay: 1")]).await;
        let mut request = site.request("/");
        request.options.timeout = 20;
        let report = site.search().crawl(request).await;
        assert!(report.timed_out);
        // Under load the deadline can expire before the initial policy request.
        // Either way, no page/sitemap request may follow the robots delay.
        assert!(site.paths().iter().all(|path| path == "/robots.txt"));
    }

    #[tokio::test]
    async fn sitemap_discovery_is_guarded_scoped_and_cycle_bounded() {
        let site = Site::new(&[
            ("/robots.txt", 200, "", "Sitemap: /docs/index.xml\nSitemap: http://example.invalid/escape.xml"),
            ("/docs/index.xml", 200, "Content-Type: application/xml\r\n", "<sitemapindex><sitemap><loc>/docs/urls.xml</loc></sitemap><sitemap><loc>/docs/index.xml</loc></sitemap></sitemapindex>"),
            ("/docs/urls.xml", 200, "Content-Type: application/xml\r\n", "<urlset><url><loc>/docs/a</loc></url><url><loc>/outside</loc></url></urlset>"),
            ("/docs/", 200, "", "Root"), ("/docs/a", 200, "", "A"),
        ]).await;
        let mut request = site.request("/docs/");
        request.options.path_prefix = Some("/docs".into());
        let report = site.search().map(request).await;
        assert_eq!(
            report.urls,
            [
                format!("{}/docs/", site.root),
                format!("{}/docs/a", site.root)
            ]
        );
        assert_eq!(
            site.paths(),
            [
                "/robots.txt",
                "/docs/index.xml",
                "/docs/urls.xml",
                "/docs/",
                "/docs/a"
            ]
        );
        assert!(report.pages.is_empty());
    }

    #[tokio::test]
    async fn link_graph_is_opt_in_and_never_restores_image_or_inline_targets() {
        let site = Site::new(&[("/", 200, "Content-Type: text/html\r\n", "<body><p>Article.</p><a href='/read'>Read</a><a href='/photo'><img src='/photo.png' alt='Photo'>Photo</a><a href='/image.jpg'>Image</a></body>")]).await;
        let search = site.search();
        let urls = [format!("{}/", site.root)];
        let plain = search.fetch(&urls).await;
        let linked = search.fetch_with_links(&urls, true).await;
        assert!(plain[0].links.is_empty());
        assert_eq!(linked[0].links, [format!("{}/read", site.root)]);
        assert_eq!(plain[0].text, linked[0].text);
        assert!(!linked[0].text.contains("/read"));
        assert!(!linked[0].text.contains("photo.png"));
    }

    #[tokio::test]
    async fn robots_redirect_cannot_escape_path_scope() {
        let site = Site::new(&[("/robots.txt", 302, "Location: /outside\r\n", "")]).await;
        let mut request = site.request("/docs/");
        request.options.path_prefix = Some("/docs".into());
        let report = site.search().crawl(request).await;
        assert_eq!(site.paths(), ["/robots.txt"]);
        assert!(report.failures[0].error.contains("scope"));
    }

    #[tokio::test]
    async fn combined_http_and_refresh_chain_uses_one_hop_limit() {
        let site = Site::new(&[
            ("/", 302, "Location: /a\r\n", ""),
            (
                "/a",
                200,
                "Content-Type: text/html\r\n",
                "<meta http-equiv='refresh' content='0; url=/b'>",
            ),
            ("/b", 200, "", "Should not be fetched"),
        ])
        .await;
        let mut config = crate::core::Config::default();
        config.fetch.allow_private_networks = true;
        config.fetch.max_redirects = 1;
        config.crawl.delay = 0;
        let report = Search::open(config).unwrap().crawl(site.request("/")).await;
        assert_eq!(site.paths(), ["/robots.txt", "/sitemap.xml", "/", "/a"]);
        assert!(report.failures[0].error.contains("too many redirects"));
    }

    #[tokio::test]
    async fn partial_bytes_remain_charged_when_deadline_cancels_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nUser-agent: *\n")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let mut config = crate::core::Config::default();
        config.fetch.allow_private_networks = true;
        config.crawl.delay = 0;
        let report = Search::open(config)
            .unwrap()
            .crawl(CrawlRequest {
                url,
                options: CrawlOptions {
                    timeout: 100,
                    ..Default::default()
                },
            })
            .await;
        task.abort();
        assert!(report.timed_out);
        assert_eq!(report.bytes, 14);
        assert_eq!(report.visited, 0);
    }

    #[tokio::test]
    async fn requested_limits_cannot_raise_operator_depth_or_page_ceilings() {
        let site = Site::new(&[(
            "/",
            200,
            "Content-Type: text/html\r\n",
            "<a href='/a'>A</a>",
        )])
        .await;
        let mut config = crate::core::Config::default();
        config.fetch.allow_private_networks = true;
        config.crawl.delay = 0;
        config.crawl.max_pages = 1;
        config.crawl.max_depth = 0;
        let mut request = site.request("/");
        request.options.max_pages = 100;
        request.options.max_depth = 100;
        let report = Search::open(config).unwrap().crawl(request).await;
        assert_eq!(site.paths(), ["/robots.txt", "/"]);
        assert_eq!(report.visited, 1);
        assert!(report.truncated);
    }

    #[test]
    fn robots_equivalent_utf8_and_unreserved_escapes_obey_the_same_policy() {
        let rules = test_robots("User-agent: *\nDisallow: /secret\nDisallow: /café\nDisallow: /foo/%62ar\nDisallow: /private\nDisallow: /reserved%2fslash\nDisallow: /literal%2Astar", "Search/1");
        for path in [
            "/%73ecret",
            "/caf%C3%A9",
            "/caf%c3%a9",
            "/café",
            "/foo/bar",
            "/foo/%62ar",
            "/%70rivate",
            "/reserved%2Fslash",
            "/literal%2astar",
        ] {
            assert!(
                !test_allowed(
                    &rules,
                    &Url::parse(&format!("https://example.com{path}")).unwrap()
                ),
                "{path}"
            );
        }
        for path in ["/reserved/slash", "/literal-any-star", "/Secret"] {
            assert!(
                test_allowed(
                    &rules,
                    &Url::parse(&format!("https://example.com{path}")).unwrap()
                ),
                "{path}"
            );
        }
        let equal = test_robots(
            "User-agent: *\nDisallow: /foo/%62ar\nAllow: /foo/bar",
            "Search/1",
        );
        assert!(test_allowed(
            &equal,
            &Url::parse("https://example.com/foo/bar").unwrap()
        ));
    }

    #[test]
    fn robots_selects_product_name_without_version_or_contact_url() {
        let rules = test_robots("User-agent: *\nDisallow: /\nUser-agent: github\nAllow: /\nUser-agent: preview\nAllow: /", "Search/preview (+https://github.com/arocomputer/search)");
        assert!(!test_allowed(
            &rules,
            &Url::parse("https://example.com/").unwrap()
        ));
        let rules = test_robots(
            "User-agent: *\nDisallow: /\nUser-agent: SEARCH\nAllow: /",
            "Search/preview (+https://github.com/arocomputer/search)",
        );
        assert!(test_allowed(
            &rules,
            &Url::parse("https://example.com/").unwrap()
        ));
    }

    #[test]
    fn wildcard_segments_match_without_rescanning_adversarial_suffixes() {
        let path = format!("/{}", "a".repeat(8192));
        for end in ["b", "b$"] {
            let pattern = format!("/*{}{end}", "a".repeat(4096));
            assert!(!robots_match(&pattern, &path, policy_deadline()).unwrap());
        }
        for (pattern, path, expected) in [
            ("/a*b*c$", "/axbyc", true),
            ("/a*b*c$", "/axbycz", false),
            ("/a*bar$", "/abar-more-bar", true),
            ("/a*bar$", "/abar-more", false),
            ("/abc*bc$", "/abc", false),
            ("/a**b*", "/axb-tail", true),
        ] {
            assert_eq!(
                robots_match(pattern, path, policy_deadline()).unwrap(),
                expected,
                "{pattern}: {path}"
            );
        }
    }

    #[tokio::test]
    async fn adversarial_policy_matching_stops_at_worker_deadline() {
        let pattern = format!("/*{}b", "a".repeat(4096));
        let rules = Arc::new(Robots {
            rules: vec![(pattern, false); 1000],
            ..Default::default()
        });
        let url = Url::parse(&format!("https://example.com/{}", "a".repeat(8192))).unwrap();
        let fetcher =
            Fetcher::new(crate::core::config::FetchSettings::default(), "Search/1").unwrap();
        let mut budget = Budget {
            started: Instant::now(),
            timeout: Duration::from_millis(1),
            max_bytes: 100,
            bytes: 0,
            delay: Duration::ZERO,
            settings: Default::default(),
            timed_out: false,
            truncated: false,
        };
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            rules.permits(&fetcher, &budget, &url),
        )
        .await
        .unwrap();
        if let Ok(allowed) = result {
            // Fast machines may finish the linear scan before the deadline.
            assert!(allowed);
        }
        assert!(budget.started.elapsed() < Duration::from_secs(1));
        // An already expired policy also refuses parsing, normalization and matching.
        budget.timeout = Duration::ZERO;
        let deadline = PolicyDeadline {
            started: budget.started,
            timeout: budget.timeout,
        };
        assert!(Robots::parse("User-agent: *", "Search/1", deadline).is_err());
        assert!(robots_normalize("/%73ecret", deadline).is_err());
        assert!(robots_match("/*", "/", deadline).is_err());
    }

    #[tokio::test]
    async fn malformed_utf8_robots_fails_closed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 8192];
            let count = socket.read(&mut request).await.unwrap();
            assert!(String::from_utf8_lossy(&request[..count]).starts_with("GET /robots.txt "));
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n\xff\xfe",
                )
                .await
                .unwrap();
        });
        let mut config = crate::core::Config::default();
        config.fetch.allow_private_networks = true;
        config.crawl.delay = 0;
        let report = Search::open(config)
            .unwrap()
            .crawl(CrawlRequest {
                url,
                options: CrawlOptions::default(),
            })
            .await;
        task.await.unwrap();
        assert!(report.failures[0].error.contains("robots unreadable"));
        assert_eq!(report.visited, 0);
        assert!(report.urls.is_empty());
    }

    #[tokio::test]
    async fn complete_robots_at_response_cap_still_authorizes_crawl() {
        let site = Site::new(&[
            ("/robots.txt", 200, "", "User-agent: *\n"),
            ("/", 200, "", "Root"),
        ])
        .await;
        let mut config = crate::core::Config::default();
        config.fetch.allow_private_networks = true;
        config.fetch.max_response_bytes = 14;
        config.crawl.delay = 0;
        let report = Search::open(config).unwrap().crawl(site.request("/")).await;
        assert_eq!(report.urls, [format!("{}/", site.root)]);
        assert!(report.failures.is_empty());
        assert!(!report.truncated);
        assert_eq!(report.bytes, 18);
    }

    #[tokio::test]
    async fn complete_sitemap_at_response_cap_is_not_discarded() {
        let xml = "<urlset><url><loc>/a</loc></url></urlset>";
        let site = Site::new(&[
            (
                "/sitemap.xml",
                200,
                "Content-Type: application/xml\r\n",
                xml,
            ),
            ("/", 200, "", "Root"),
            ("/a", 200, "", "A"),
        ])
        .await;
        let mut config = crate::core::Config::default();
        config.fetch.allow_private_networks = true;
        config.fetch.max_response_bytes = xml.len() as u64;
        config.crawl.delay = 0;
        let report = Search::open(config).unwrap().crawl(site.request("/")).await;
        assert_eq!(
            report.urls,
            [format!("{}/", site.root), format!("{}/a", site.root)]
        );
        assert!(!report.truncated);
        assert!(report.failures.is_empty());
    }

    #[tokio::test]
    async fn sitemap_documents_outside_page_prefix_keep_child_pages_scoped() {
        for advertised in ["", "Sitemap: /sitemap.xml"] {
            let site = Site::new(&[
                ("/robots.txt", 200, "", advertised),
                ("/sitemap.xml", 200, "", "<sitemapindex><sitemap><loc>/outside/collection.xml</loc></sitemap><sitemap><loc>http://example.invalid/escape.xml</loc></sitemap></sitemapindex>"),
                ("/outside/collection.xml", 302, "Location: /collection.xml\r\n", ""),
                ("/collection.xml", 200, "", "<urlset><url><loc>/docs/a</loc></url><url><loc>/outside/page</loc></url></urlset>"),
                ("/docs/", 200, "", "Root"), ("/docs/a", 200, "", "A"),
            ]).await;
            let mut request = site.request("/docs/");
            request.options.path_prefix = Some("/docs".into());
            let report = site.search().map(request).await;
            assert_eq!(
                report.urls,
                [
                    format!("{}/docs/", site.root),
                    format!("{}/docs/a", site.root)
                ]
            );
            assert_eq!(
                site.paths(),
                [
                    "/robots.txt",
                    "/sitemap.xml",
                    "/outside/collection.xml",
                    "/collection.xml",
                    "/docs/",
                    "/docs/a"
                ]
            );
            assert!(report.failures.is_empty());
        }
    }
}
