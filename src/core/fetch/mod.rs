//! Fetch: retrieve URLs on behalf of callers and turn HTML into readable text.
//!
//! Because URLs are model-chosen, every request is treated as hostile:
//! destinations are resolved and refused if they point inside the network, each
//! redirect hop is re-checked, bodies are size-capped, and deadlines are
//! enforced. Successful pages are cached briefly in memory.

mod cache;
mod extract;
pub(crate) mod guard;

/// Apply the reader's image/reference neutralization to optional generated prose.
/// Edge processors are trusted programs, but their returned text is still data.
pub(crate) fn sanitize_text(text: &str) -> String {
    extract::sanitize(text)
}

use std::time::Duration;

use crate::core::config::FetchSettings;

pub use guard::{is_public_ip, GuardError};

/// Decide whether to follow a client-side redirect target. The target came from
/// an untrusted page, so it is accepted only when it is a well-formed http(s)
/// URL that passes the SSRF guard, does not loop, and is within the hop budget.
/// Returns the URL to fetch next, or `None` to stop.
fn follow_target(
    next: &str,
    current: &str,
    hops: usize,
    max_hops: usize,
    allow_private_networks: bool,
) -> Option<String> {
    if hops > max_hops || next == current {
        return None;
    }
    let parsed = url::Url::parse(next).ok()?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return None;
    }
    guard::check_host(parsed.host_str().unwrap_or(""), allow_private_networks).ok()?;
    Some(next.to_string())
}

/// The outcome of one fetch.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Page {
    pub url: String,
    /// Unix timestamp when Search fetched this page; retained for cache hits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,
    /// The HTTP status, or 0 when no HTTP response was received.
    pub status: u16,
    pub content_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    pub text: String,
    /// Opt-in navigation targets, kept separate from model prose.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    /// A client-side redirect target, followed by the caller. Not serialized:
    /// a model is shown the final page, never told to fetch another URL.
    #[serde(skip)]
    pub redirect: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A refused or failed fetch. Each variant is a distinct caller-facing reason.
#[derive(Debug, Clone)]
pub enum FetchError {
    /// The URL was not http or https.
    Scheme,
    /// The destination is private, loopback, or otherwise unreachable by policy.
    Refused(String),
    /// The request never completed.
    Network(String),
    /// The server answered with an error status.
    Status(u16),
    /// The response carried no readable text.
    Empty,
    /// The body is a type Search cannot read as text, such as an image or archive.
    Unsupported(String),
}

impl FetchError {
    /// The HTTP status the failure carries, or 0 when no response was received.
    pub fn status(&self) -> u16 {
        match self {
            FetchError::Status(code) => *code,
            _ => 0,
        }
    }

    /// The caller-facing reason, without the URL, for text and JSON output.
    pub fn message(&self) -> String {
        match self {
            FetchError::Scheme => "only http and https URLs are supported".into(),
            FetchError::Refused(reason) => reason.clone(),
            FetchError::Network(reason) => reason.clone(),
            FetchError::Status(code) => format!("HTTP {code}"),
            FetchError::Empty => "no readable text in response".into(),
            FetchError::Unsupported(reason) => reason.clone(),
        }
    }
}

/// Holds the guarded client, fetch bounds, and a bounded in-memory TTL cache.
pub struct Fetcher {
    settings: FetchSettings,
    client: reqwest::Client,
    cache: cache::Cache,
    permits: std::sync::Arc<tokio::sync::Semaphore>,
    crawl_hosts: tokio::sync::Mutex<std::collections::HashMap<String, CrawlHost>>,
}

/// Shared crawl scheduling serializes each host without holding network permits
/// during politeness waits. Active records cannot be evicted out from under work.
struct CrawlHost {
    touched: std::time::Instant,
    schedule: std::sync::Arc<tokio::sync::Mutex<CrawlSchedule>>,
}

#[derive(Default)]
struct CrawlSchedule {
    last: Option<std::time::Instant>,
    delay: Duration,
}

/// Abort queued blocking work when its caller stops waiting. Running work keeps
/// its owned permit until completion because Tokio cannot stop a running thread.
struct BlockingWork<T>(tokio::task::JoinHandle<T>);
impl<T> Drop for BlockingWork<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Record completion even when a request errors or its future is cancelled.
/// Waiting for politeness or a global permit does not count as a request.
struct CrawlSlot<'a> {
    schedule: tokio::sync::MutexGuard<'a, CrawlSchedule>,
    started: bool,
}
impl Drop for CrawlSlot<'_> {
    fn drop(&mut self) {
        if self.started {
            self.schedule.last = Some(std::time::Instant::now());
        }
    }
}

impl Fetcher {
    /// Build the fetcher, failing if its guarded HTTP client cannot be built.
    /// The client reuses no default transport: a [`guard::GuardedResolver`] vets
    /// every address at connect time, and redirects are re-validated per hop.
    pub fn new(settings: FetchSettings, user_agent: &str) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .user_agent(user_agent.to_string())
            .timeout(settings.timeout())
            .connect_timeout(Duration::from_secs(8))
            // The resolver is the trust boundary: it refuses a private address
            // even if a name's answer changed since `check_host` ran.
            .dns_resolver(std::sync::Arc::new(guard::GuardedResolver::new(
                settings.allow_private_networks,
            )))
            .redirect(reqwest::redirect::Policy::none())
            .pool_idle_timeout(Duration::from_secs(30))
            .build()?;
        let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(
            settings
                .max_concurrency
                .clamp(1, tokio::sync::Semaphore::MAX_PERMITS),
        ));
        let cache = cache::Cache::new(settings.cache_ttl(), settings.cache_bytes);
        Ok(Fetcher {
            settings,
            client,
            cache,
            permits,
            crawl_hosts: Default::default(),
        })
    }

    /// Retrieve one URL, reusing recent answers from memory. A failure is returned, not
    /// panicked, so a batch caller can report partial success.
    ///
    /// A page that exists only to redirect the reader elsewhere (a trailing-slash
    /// or canonical-URL move, delivered by a prompt meta refresh) is
    /// followed to its target. Each target came from an untrusted page, so it is
    /// run back through the SSRF guard, and the chain is bounded so a redirect
    /// loop cannot spin. Textless stubs that cannot be followed fail as empty.
    /// `Page.url` retains the requested URL; `final_url` identifies the destination.
    pub async fn fetch(&self, raw: &str) -> Result<Page, FetchError> {
        self.fetch_mode(raw, false).await
    }

    /// Retrieve navigation targets separately from readable text.
    pub async fn fetch_with_links(&self, raw: &str) -> Result<Page, FetchError> {
        self.fetch_mode(raw, true).await
    }

    /// One deadline includes admission, every redirect, and extraction. Cancelled
    /// queued workers are aborted; running workers retain their concurrency slot.
    async fn fetch_mode(&self, raw: &str, links: bool) -> Result<Page, FetchError> {
        tokio::time::timeout(self.settings.timeout(), self.fetch_chain(raw, links))
            .await
            .map_err(|_| FetchError::Network("request timed out".into()))?
    }

    /// Follow HTTP and refresh hops with one guarded allowance and extraction mode.
    async fn fetch_chain(&self, raw: &str, links: bool) -> Result<Page, FetchError> {
        let mut target = raw.to_string();
        let mut hops = 0usize;
        loop {
            let mut fetched = self.fetch_one(&target, links, &mut hops).await?;
            fetched.url = raw.to_string();
            let Some(next) = fetched.redirect.clone() else {
                return Ok(fetched);
            };
            hops += 1;
            match follow_target(
                &next,
                &target,
                hops,
                self.settings.max_redirects,
                self.settings.allow_private_networks,
            ) {
                Some(next) => {
                    target = next;
                }
                // A redirect that is unsafe, malformed, or looping stops here and
                // a readable stub is returned; a textless stub is an error.
                None if fetched.text.trim().is_empty() => return Err(FetchError::Empty),
                None => return Ok(fetched),
            }
        }
    }

    /// One guarded HTTP chain and extraction, without client-side refresh following.
    async fn fetch_one(
        &self,
        raw: &str,
        links: bool,
        hops: &mut usize,
    ) -> Result<Page, FetchError> {
        let parsed = url::Url::parse(raw).map_err(|_| FetchError::Scheme)?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(FetchError::Scheme);
        }
        guard::check_host(
            parsed.host_str().unwrap_or(""),
            self.settings.allow_private_networks,
        )
        .map_err(|e| FetchError::Refused(e.message()))?;

        // Only pages without HTTP hops can be reused without hiding spent hops.
        if !links && *hops == 0 {
            if let Some(cached) = self.cache.get(raw) {
                return Ok(cached);
            }
        }

        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| FetchError::Network("fetcher is shutting down".into()))?;

        let response = self.request(&parsed, hops).await?;

        let status = response.status();
        let final_url = response.url().to_string();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if status.as_u16() >= 400 {
            return Err(FetchError::Status(status.as_u16()));
        }

        let (body, truncated) = read_capped(response, self.settings.max_response_bytes)
            .await
            .map_err(FetchError::Network)?;

        // Extraction is CPU-bound and its types are not `Send`, so it runs on a
        // blocking thread with the body moved in and a plain `Page` returned. The
        // caller waits at most `fetch.timeout` for it, so a pathological document
        // cannot hold a fetch open. The concurrency permit moves into the thread,
        // so an abandoned extraction keeps its slot until it actually finishes.
        let kind = body_kind(&content_type, &body)?;
        if kind == Kind::Pdf && truncated {
            return Err(FetchError::Unsupported(
                "PDF larger than fetch.max_response_bytes".into(),
            ));
        }
        let base = final_url.clone();
        let declared = content_type.clone();
        let mut page = BlockingWork(tokio::task::spawn_blocking(move || {
            let _permit = permit;
            match kind {
                Kind::Html => {
                    let html = decode(&body, &declared, true);
                    let graph = if links {
                        extract::links(&html, &base)
                    } else {
                        Vec::new()
                    };
                    Ok((extract::read(&html, &base), graph))
                }
                Kind::Text => Ok((
                    extract::Page::text(extract::sanitize(&decode(&body, &declared, false))),
                    Vec::new(),
                )),
                Kind::Pdf => {
                    extract::pdf(&body).map(|text| (extract::Page::text(text), Vec::new()))
                }
            }
        }));
        let (page, graph) = tokio::time::timeout(self.settings.timeout(), &mut page.0)
            .await
            .map_err(|_| FetchError::Network("extraction timed out".into()))?
            .map_err(|_| FetchError::Network("extraction failed".into()))?
            .map_err(FetchError::Unsupported)?;
        // A textless page that names a redirect target is not empty: the caller
        // re-guards and follows it. Only a textless page with nowhere to go is.
        if page.text.trim().is_empty() && page.redirect.is_none() {
            return Err(FetchError::Empty);
        }
        let text = page.text;

        let fetched_at = now_unix();
        let fetched = Page {
            url: raw.to_string(),
            fetched_at: Some(fetched_at),
            final_url: Some(final_url),
            status: status.as_u16(),
            content_type,
            title: Some(page.title).filter(|t| !t.is_empty()),
            byline: page.byline,
            published: page.published,
            site: page.site,
            text,
            links: graph,
            truncated: Some(truncated).filter(|t| *t),
            redirect: page.redirect,
            error: None,
        };
        if !links && *hops == 0 {
            self.cache.put(raw, &fetched);
        }
        Ok(fetched)
    }

    /// Send through the guarded transport, spending the caller's shared HTTP/refresh
    /// allowance before dialing each redirect destination.
    async fn request(
        &self,
        url: &url::Url,
        hops: &mut usize,
    ) -> Result<reqwest::Response, FetchError> {
        let started = std::time::Instant::now();
        let mut target = url.clone();
        loop {
            check_redirect(
                &target,
                *hops,
                self.settings.max_redirects,
                self.settings.allow_private_networks,
            )
            .map_err(FetchError::Refused)?;
            let remaining = self.settings.timeout().saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(FetchError::Network("request timed out".into()));
            }
            let response = self
                .client
                .get(target.clone())
                .timeout(remaining)
                .send()
                .await
                .map_err(|error| FetchError::Network(classify(&error)))?;
            if !response.status().is_redirection() {
                return Ok(response);
            }
            let Some(location) = response.headers().get(reqwest::header::LOCATION) else {
                return Ok(response);
            };
            if *hops >= self.settings.max_redirects {
                return Err(FetchError::Refused("too many redirects".into()));
            }
            *hops += 1;
            target = target
                .join(
                    location
                        .to_str()
                        .map_err(|_| FetchError::Refused("invalid redirect".into()))?,
                )
                .map_err(|_| FetchError::Refused("invalid redirect".into()))?;
        }
    }

    /// One bounded crawl hop. Redirects are returned to the crawl scheduler so scope,
    /// robots, delay, byte accounting and the global deadline apply to every request.
    pub(crate) async fn crawl_response(
        &self,
        url: &url::Url,
        max: u64,
        charged: &mut u64,
        settings: &crate::core::config::CrawlSettings,
        delay: Duration,
        deadline: tokio::time::Instant,
    ) -> Result<RawPage, FetchError> {
        check_redirect(
            url,
            0,
            self.settings.max_redirects,
            self.settings.allow_private_networks,
        )
        .map_err(FetchError::Refused)?;
        let schedule = {
            let mut hosts = self.crawl_hosts.lock().await;
            hosts.retain(|_, host| {
                std::sync::Arc::strong_count(&host.schedule) > 1
                    || host.touched.elapsed() < Duration::from_millis(settings.host_ttl)
                    || host.schedule.try_lock().is_ok_and(|schedule| {
                        schedule
                            .last
                            .is_some_and(|last| last.elapsed() < schedule.delay)
                    })
            });
            let key = url.host_str().unwrap_or_default().to_ascii_lowercase();
            if !hosts.contains_key(&key) && hosts.len() >= settings.max_hosts {
                let oldest = hosts
                    .iter()
                    .filter(|(_, host)| {
                        std::sync::Arc::strong_count(&host.schedule) == 1
                            && host.schedule.try_lock().is_ok_and(|schedule| {
                                schedule
                                    .last
                                    .is_none_or(|last| last.elapsed() >= schedule.delay)
                            })
                    })
                    .min_by_key(|(_, host)| host.touched)
                    .map(|(key, _)| key.clone());
                if let Some(oldest) = oldest {
                    hosts.remove(&oldest);
                }
            }
            if !hosts.contains_key(&key) && hosts.len() >= settings.max_hosts {
                return Err(FetchError::Network(
                    "crawl host scheduling capacity reached".into(),
                ));
            }
            let host = hosts.entry(key).or_insert_with(|| CrawlHost {
                touched: std::time::Instant::now(),
                schedule: Default::default(),
            });
            host.touched = std::time::Instant::now();
            host.schedule.clone()
        };
        let mut slot = CrawlSlot {
            schedule: schedule.lock().await,
            started: false,
        };
        let wait = slot
            .schedule
            .last
            .map(|last| {
                slot.schedule
                    .delay
                    .max(delay)
                    .saturating_sub(last.elapsed())
            })
            .unwrap_or_default();
        tokio::time::sleep(wait).await;
        if tokio::time::Instant::now() >= deadline {
            return Err(FetchError::Network("crawl deadline reached".into()));
        }
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| FetchError::Network("fetcher is shutting down".into()))?;
        if tokio::time::Instant::now() >= deadline {
            return Err(FetchError::Network("crawl deadline reached".into()));
        }
        slot.schedule.delay = delay;
        slot.started = true;
        let response = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|error| FetchError::Network(classify(&error)))?;
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let (body, truncated) = read_counted(
            response,
            max.min(self.settings.max_response_bytes),
            charged,
            true,
        )
        .await
        .map_err(FetchError::Network)?;
        Ok(RawPage {
            status,
            location,
            content_type,
            body,
            truncated,
        })
    }

    /// Extract already-budgeted crawl bytes without additional network work.
    pub(crate) async fn crawl_page(
        &self,
        url: &url::Url,
        raw: RawPage,
        remaining: Duration,
    ) -> Result<Page, FetchError> {
        let url = url.clone();
        self.crawl_work(remaining, move || Self::extract_crawl(&url, raw))
            .await?
    }

    /// Bound parsing, extraction and evidence selection by the remaining deadline and concurrency
    /// slot. A cancelled blocking worker retains its slot until it actually ends.
    pub(crate) async fn crawl_work<T, F>(
        &self,
        remaining: Duration,
        work: F,
    ) -> Result<T, FetchError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let permits = self.permits.clone();
        tokio::time::timeout(remaining, async move {
            let permit = permits
                .acquire_owned()
                .await
                .map_err(|_| FetchError::Network("fetcher is shutting down".into()))?;
            let mut task = BlockingWork(tokio::task::spawn_blocking(move || {
                let _permit = permit;
                work()
            }));
            (&mut task.0)
                .await
                .map_err(|_| FetchError::Network("crawl parsing failed".into()))
        })
        .await
        .map_err(|_| FetchError::Network("crawl parsing timed out".into()))?
    }

    /// Convert a bounded wire response on a blocking worker; no transport access.
    fn extract_crawl(url: &url::Url, raw: RawPage) -> Result<Page, FetchError> {
        let kind = body_kind(&raw.content_type, &raw.body)?;
        let (page, links) = match kind {
            Kind::Html => {
                let html = decode(&raw.body, &raw.content_type, true);
                (
                    extract::read(&html, url.as_str()),
                    extract::links(&html, url.as_str()),
                )
            }
            Kind::Text => (
                extract::Page::text(extract::sanitize(&decode(
                    &raw.body,
                    &raw.content_type,
                    false,
                ))),
                Vec::new(),
            ),
            Kind::Pdf if raw.truncated => {
                return Err(FetchError::Unsupported("truncated PDF".into()))
            }
            Kind::Pdf => (
                extract::Page::text(extract::pdf(&raw.body).map_err(FetchError::Unsupported)?),
                Vec::new(),
            ),
        };
        Ok(Page {
            url: url.to_string(),
            fetched_at: Some(now_unix()),
            final_url: Some(url.to_string()),
            status: raw.status,
            content_type: raw.content_type,
            title: Some(page.title).filter(|t| !t.is_empty()),
            byline: page.byline,
            published: page.published,
            site: page.site,
            text: page.text,
            links,
            truncated: raw.truncated.then_some(true),
            redirect: page.redirect,
            error: None,
        })
    }

    /// The configured total redirect allowance, including client-side refreshes.
    pub(crate) fn redirect_limit(&self) -> usize {
        self.settings.max_redirects
    }

    /// Fetch URLs concurrently, preserving input order.
    pub async fn fetch_many(&self, urls: &[String]) -> Vec<Page> {
        let mut futures = Vec::with_capacity(urls.len());
        for url in urls {
            futures.push(self.fetch(url));
        }
        let results = futures::future::join_all(futures).await;
        results
            .into_iter()
            .zip(urls)
            .map(|(outcome, url)| match outcome {
                Ok(fetched) => fetched,
                Err(error) => Page {
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
                },
            })
            .collect()
    }
}

/// Navigation excludes common image destinations even when represented by text anchors.
pub(crate) fn navigation_target(url: &url::Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    ![
        ".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".ico", ".avif", ".bmp", ".tif", ".tiff",
    ]
    .iter()
    .any(|extension| path.ends_with(extension))
}

/// A bounded wire response used only by the crawl scheduler.
pub(crate) struct RawPage {
    pub status: u16,
    pub location: Option<String>,
    pub content_type: String,
    pub body: Vec<u8>,
    pub truncated: bool,
}

/// Validate HTTP redirects before reqwest can dial a literal address or hostname.
/// `hops` counts redirects from the initial URL, so the budget allows exactly
/// `maximum` redirects: hop `maximum + 1` is refused.
fn check_redirect(
    url: &url::Url,
    hops: usize,
    maximum: usize,
    allow_private: bool,
) -> Result<(), String> {
    if hops > maximum {
        return Err("too many redirects".into());
    }
    if !matches!(url.scheme(), "http" | "https") {
        return Err("unsupported redirect scheme".into());
    }
    guard::check_host(url.host_str().unwrap_or_default(), allow_private)
        .map_err(|error| error.message())
}

/// Read a response body up to `max` bytes, reporting whether more remained.
/// The bytes are returned undecoded: the charset and content kind decide how
/// they become text.
async fn read_capped(response: reqwest::Response, max: u64) -> Result<(Vec<u8>, bool), String> {
    read_counted(response, max, &mut 0, false).await
}

/// Charge retained bytes as they arrive, including reads cancelled by the crawl
/// deadline. At the cap, an uncompressed Content-Length proves completion;
/// unknown framing remains conservative without polling an extra hidden chunk.
async fn read_counted(
    response: reqwest::Response,
    max: u64,
    charged: &mut u64,
    stop_at_cap: bool,
) -> Result<(Vec<u8>, bool), String> {
    use futures::StreamExt;
    let known_length = if !response
        .headers()
        .contains_key(reqwest::header::CONTENT_ENCODING)
        && !response
            .headers()
            .contains_key(reqwest::header::TRANSFER_ENCODING)
    {
        response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|length| length.to_str().ok())
            .and_then(|length| length.parse::<u64>().ok())
    } else {
        None
    };
    let mut stream = response.bytes_stream();
    let mut collected: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("body failed: {}", classify(&error)))?;
        let remaining = max.saturating_sub(collected.len() as u64);
        if chunk.len() as u64 > remaining {
            let take = remaining as usize;
            collected.extend_from_slice(chunk.get(..take).unwrap_or(&chunk));
            *charged = charged.saturating_add(take as u64);
            return Ok((collected, true));
        }
        collected.extend_from_slice(&chunk);
        *charged = charged.saturating_add(chunk.len() as u64);
        if stop_at_cap && collected.len() as u64 >= max {
            let complete = known_length == Some(collected.len() as u64);
            return Ok((collected, !complete));
        }
    }
    Ok((collected, false))
}

/// Turn a transport error into a concise message that does not leak the URL.
/// A resolver refusal — the SSRF guard catching a name that resolves inside the
/// network — is preserved, because that is a deliberate refusal, not a failure.
fn classify(error: &reqwest::Error) -> String {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    for _ in 0..6 {
        let Some(current) = source else { break };
        let text = current.to_string();
        if text.contains("refusing") || text.contains("resolves to private") {
            return text;
        }
        source = current.source();
    }
    if error.is_timeout() {
        "request timed out".into()
    } else if error.is_connect() {
        "host could not be reached".into()
    } else {
        "request failed".into()
    }
}

/// How a response body is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Html,
    Text,
    Pdf,
}

/// Decide how to read a body from its declared media type, sniffing the bytes
/// when the server declares nothing useful or mislabels a PDF. Images, archives,
/// media and other binaries are refused by type rather than returned as noise.
fn body_kind(content_type: &str, body: &[u8]) -> Result<Kind, FetchError> {
    if body.starts_with(b"%PDF-") {
        return Ok(Kind::Pdf);
    }
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match essence.as_str() {
        "text/html" | "application/xhtml+xml" => Ok(Kind::Html),
        "application/pdf" => Ok(Kind::Pdf),
        "" | "application/octet-stream" | "binary/octet-stream" | "application/unknown" => {
            sniff(body)
        }
        textual
            if textual.starts_with("text/")
                || textual.ends_with("+json")
                || textual.ends_with("+xml")
                || matches!(
                    textual,
                    "application/json"
                        | "application/xml"
                        | "application/javascript"
                        | "application/x-javascript"
                        | "application/ecmascript"
                        | "application/yaml"
                        | "application/x-yaml"
                        | "application/toml"
                        | "application/x-sh"
                        | "application/sql"
                        | "application/x-ndjson"
                ) =>
        {
            Ok(Kind::Text)
        }
        other => Err(FetchError::Unsupported(format!(
            "unsupported content type {other}; Search reads HTML, text and PDF"
        ))),
    }
}

/// Classify an undeclared body: markup that opens like HTML is HTML, bytes
/// without NULs are text, and anything else is an unreadable binary.
fn sniff(body: &[u8]) -> Result<Kind, FetchError> {
    let head = body.get(..body.len().min(4096)).unwrap_or(body);
    let lowered = String::from_utf8_lossy(head).to_ascii_lowercase();
    let opening = lowered.trim_start_matches('\u{feff}').trim_start();
    if opening.starts_with("<!doctype html")
        || opening.starts_with("<html")
        || lowered.contains("<body")
    {
        return Ok(Kind::Html);
    }
    if head.contains(&0) {
        return Err(FetchError::Unsupported(
            "unsupported binary content; Search reads HTML, text and PDF".into(),
        ));
    }
    Ok(Kind::Text)
}

/// Decode a body using its byte-order mark, the declared `charset`, or for HTML
/// a `<meta>` charset near the start of the document, defaulting to UTF-8.
/// Undecodable bytes become replacement characters rather than errors.
fn decode(body: &[u8], content_type: &str, html: bool) -> String {
    let label =
        charset_param(content_type).or_else(|| if html { meta_charset(body) } else { None });
    let encoding = label
        .and_then(|label| encoding_rs::Encoding::for_label(label.trim().as_bytes()))
        // A document cannot declare itself UTF-16 from inside its own text;
        // only a byte-order mark selects it, which `decode` honors below.
        .filter(|encoding| *encoding != encoding_rs::UTF_16LE && *encoding != encoding_rs::UTF_16BE)
        .unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(body);
    text.into_owned()
}

/// The `charset` parameter of a Content-Type header, if any.
fn charset_param(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches(['"', '\'']).to_string())
    })
}

/// The charset an HTML document declares in a `<meta charset>` or
/// `<meta http-equiv="Content-Type" content="...; charset=...">` tag within its
/// first 4 KiB, the prescan window browsers use (with some slack).
fn meta_charset(body: &[u8]) -> Option<String> {
    let head = body.get(..body.len().min(4096)).unwrap_or(body);
    let lowered = String::from_utf8_lossy(head).to_ascii_lowercase();
    let mut rest = lowered.as_str();
    while let Some(start) = rest.find("<meta") {
        let tag = rest.get(start..)?;
        let end = tag.find('>').unwrap_or(tag.len());
        let attributes = tag.get(..end)?;
        if let Some(at) = attributes.find("charset") {
            let value = attributes
                .get(at + "charset".len()..)?
                .trim_start()
                .strip_prefix('=')?
                .trim_start()
                .trim_start_matches(['"', '\'']);
            let label: String = value
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
                .collect();
            if !label.is_empty() {
                return Some(label);
            }
        }
        rest = tag.get(end..)?;
    }
    None
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Saturated fetch/extraction admission cannot keep a read waiting beyond
    /// its configured deadline, even before any network request can start.
    #[tokio::test]
    async fn fetch_deadline_includes_admission_wait() {
        let fetcher = Fetcher::new(
            FetchSettings {
                timeout: 20,
                max_concurrency: 1,
                allow_private_networks: true,
                ..Default::default()
            },
            "search-test",
        )
        .unwrap();
        let permit = fetcher.permits.acquire().await.unwrap();
        for links in [false, true] {
            let error = tokio::time::timeout(
                Duration::from_secs(2),
                fetcher.fetch_mode("http://127.0.0.1:1/unreachable", links),
            )
            .await
            .unwrap()
            .unwrap_err();
            assert_eq!(error.message(), "request timed out");
        }
        assert_eq!(fetcher.permits.available_permits(), 0);
        drop(permit);
        assert_eq!(fetcher.permits.available_permits(), 1);
    }

    /// Cancellation of an in-flight response preserves a fresh completion time.
    #[tokio::test]
    async fn cancelled_crawl_response_records_completion() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = url::Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (sent, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nx")
                .await
                .unwrap();
            sent.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let fetcher = local_fetcher(100);
        let settings = Default::default();
        let mut bytes = 0;
        let delay = Duration::from_secs(1);
        let mut request = Box::pin(fetcher.crawl_response(
            &url,
            100,
            &mut bytes,
            &settings,
            delay,
            tokio::time::Instant::now() + Duration::from_secs(5),
        ));
        tokio::select! {
            _ = received => {},
            _ = &mut request => panic!("fixture response must remain incomplete"),
        }
        let cancelled = std::time::Instant::now();
        drop(request);
        let hosts = fetcher.crawl_hosts.lock().await;
        let schedule = hosts.get("127.0.0.1").unwrap().schedule.lock().await;
        assert!(schedule.last.unwrap() >= cancelled);
        assert_eq!(schedule.delay, delay);
        assert_eq!(
            fetcher.permits.available_permits(),
            fetcher.settings.max_concurrency
        );
        server.abort();
    }

    /// A delay belongs to the last request; a later crawl can lower it after
    /// honoring that request's outstanding wait.
    #[tokio::test]
    async fn crawl_delay_does_not_permanently_poison_host_schedule() {
        let port = serve(vec![("/", "text/plain", b"ok".to_vec())]).await;
        let url = url::Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let fetcher = local_fetcher(100);
        let settings = Default::default();
        let mut bytes = 0;
        let delay = Duration::from_millis(20);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        fetcher
            .crawl_response(&url, 100, &mut bytes, &settings, delay, deadline)
            .await
            .unwrap();
        fetcher
            .crawl_response(&url, 100, &mut bytes, &settings, Duration::ZERO, deadline)
            .await
            .unwrap();
        let hosts = fetcher.crawl_hosts.lock().await;
        assert_eq!(
            hosts.get("127.0.0.1").unwrap().schedule.lock().await.delay,
            Duration::ZERO
        );
    }

    /// Dropping a queued blocking worker prevents execution and releases its permit.
    #[test]
    fn cancelled_queued_blocking_work_never_runs() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let (release, held) = std::sync::mpsc::channel();
        let (ready, started) = std::sync::mpsc::channel();
        let blocker = runtime.spawn_blocking(move || {
            ready.send(()).unwrap();
            held.recv().unwrap();
        });
        started.recv().unwrap();
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        runtime.block_on(async {
            let fetcher = local_fetcher(100);
            let observed = ran.clone();
            let result = fetcher
                .crawl_work(Duration::from_millis(10), move || {
                    observed.store(true, std::sync::atomic::Ordering::SeqCst);
                })
                .await;
            assert!(result.is_err());
        });
        release.send(()).unwrap();
        runtime.block_on(blocker).unwrap();
        runtime.shutdown_timeout(Duration::from_secs(1));
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// Separate concurrent crawl calls share host spacing instead of each
    /// applying an independent delay and doubling requests to the same host.
    #[tokio::test]
    async fn concurrent_crawl_calls_share_host_politeness() {
        let port = serve(vec![
            ("/a", "text/plain", b"first".to_vec()),
            ("/b", "text/plain", b"second".to_vec()),
        ])
        .await;
        let fetcher = local_fetcher(DEFAULT_TEST_BYTES);
        let first = url::Url::parse(&format!("http://127.0.0.1:{port}/a")).unwrap();
        let second = url::Url::parse(&format!("http://127.0.0.1:{port}/b")).unwrap();
        let settings = crate::core::config::CrawlSettings::default();
        let delay = Duration::from_millis(30);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut first_bytes = 0;
        fetcher
            .crawl_response(&first, 100, &mut first_bytes, &settings, delay, deadline)
            .await
            .unwrap();
        let schedule = fetcher
            .crawl_hosts
            .lock()
            .await
            .get("127.0.0.1")
            .unwrap()
            .schedule
            .clone();
        let first_completed = schedule.lock().await.last.unwrap();
        let mut left_bytes = 0;
        let mut right_bytes = 0;
        let (left, right) = tokio::join!(
            fetcher.crawl_response(&first, 100, &mut left_bytes, &settings, delay, deadline),
            fetcher.crawl_response(&second, 100, &mut right_bytes, &settings, delay, deadline)
        );
        left.unwrap();
        right.unwrap();
        assert!(
            schedule
                .lock()
                .await
                .last
                .unwrap()
                .duration_since(first_completed)
                >= delay * 2
        );
        assert_eq!(fetcher.crawl_hosts.lock().await.len(), 1);
    }

    /// A bounded scheduler never forgets an outstanding host delay merely to
    /// admit another host; idle safe records remain evictable without growth.
    #[tokio::test]
    async fn host_capacity_cannot_evict_required_politeness() {
        let port = serve(vec![("/", "text/plain", b"ok".to_vec())]).await;
        let fetcher = local_fetcher(DEFAULT_TEST_BYTES);
        let first = url::Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let other = url::Url::parse(&format!("http://localhost:{port}/")).unwrap();
        let settings = crate::core::config::CrawlSettings {
            max_hosts: 1,
            ..Default::default()
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut bytes = 0;
        fetcher
            .crawl_response(
                &first,
                100,
                &mut bytes,
                &settings,
                Duration::from_secs(3600),
                deadline,
            )
            .await
            .unwrap();
        let error = fetcher
            .crawl_response(&other, 100, &mut bytes, &settings, Duration::ZERO, deadline)
            .await
            .err()
            .unwrap();
        assert_eq!(error.message(), "crawl host scheduling capacity reached");
        {
            let hosts = fetcher.crawl_hosts.lock().await;
            hosts.get("127.0.0.1").unwrap().schedule.lock().await.delay = Duration::ZERO;
        }
        fetcher
            .crawl_response(&other, 100, &mut bytes, &settings, Duration::ZERO, deadline)
            .await
            .unwrap();
        let hosts = fetcher.crawl_hosts.lock().await;
        assert_eq!(hosts.len(), 1);
        assert!(hosts.contains_key("localhost"));
    }

    #[test]
    fn http_redirects_cannot_bypass_the_guard_with_literal_addresses() {
        for target in [
            "http://127.0.0.1/",
            "http://169.254.169.254/",
            "http://[::1]/",
            "http://metadata.google.internal/",
        ] {
            let url = url::Url::parse(target).unwrap();
            assert!(check_redirect(&url, 1, 5, false).is_err());
        }
        let public = url::Url::parse("https://example.com/").unwrap();
        assert!(check_redirect(&public, 1, 5, false).is_ok());
        assert!(check_redirect(&public, 1, 0, false).is_err());
        // The budget allows exactly `maximum` redirects, not maximum + 1.
        assert!(check_redirect(&public, 5, 5, false).is_ok());
        assert!(check_redirect(&public, 6, 5, false).is_err());
    }
    use crate::core::config::FetchSettings;

    fn fetcher() -> Fetcher {
        Fetcher::new(FetchSettings::default(), "search-test").expect("valid test client")
    }

    #[test]
    fn invalid_user_agent_fails_client_construction() {
        let result = Fetcher::new(FetchSettings::default(), "bad\nuser-agent");
        assert!(
            result.is_err(),
            "invalid user agents must not get a default client"
        );
    }

    #[tokio::test]
    async fn private_and_metadata_addresses_are_refused() {
        let fetcher = fetcher();
        for raw in [
            "http://169.254.169.254/latest/meta-data/",
            "http://localhost:8642/healthz",
            "http://127.0.0.1/",
            "http://[::1]/",
            "http://2130706433/",
            "file:///etc/passwd",
        ] {
            let result = fetcher.fetch(raw).await;
            assert!(result.is_err(), "{raw} should be refused");
        }
    }

    #[tokio::test]
    async fn a_non_http_scheme_is_a_scheme_error() {
        let fetcher = fetcher();
        assert!(matches!(
            fetcher.fetch("ftp://example.com/x").await,
            Err(FetchError::Scheme)
        ));
    }

    /// A client-side redirect target is untrusted page content, so the follow
    /// decision must refuse a private destination, a non-http scheme, a loop,
    /// and anything past the hop budget.
    #[test]
    fn follow_target_guards_every_redirect() {
        let current = "https://example.com/a";
        // A public http(s) target is followed, bounded by the hop budget.
        assert_eq!(
            follow_target("https://example.com/b", current, 1, 3, false).as_deref(),
            Some("https://example.com/b")
        );
        // Private destinations are refused.
        for private in [
            "http://169.254.169.254/",
            "http://127.0.0.1/",
            "http://[::1]/",
            "http://localhost/",
            "http://[64:ff9b::a9fe:a9fe]/",
        ] {
            assert_eq!(
                follow_target(private, current, 1, 3, false),
                None,
                "{private}"
            );
        }
        // A non-http scheme is refused.
        assert_eq!(
            follow_target("file:///etc/passwd", current, 1, 3, false),
            None
        );
        // A loop back to where we are is refused.
        assert_eq!(follow_target(current, current, 1, 3, false), None);
        // Past the hop budget, nothing more is followed.
        assert_eq!(
            follow_target("https://example.com/c", current, 4, 3, false),
            None
        );
        // The guard's escape hatch, used only by tests and mirrors.
        assert!(follow_target("http://127.0.0.1/", current, 1, 3, true).is_some());
        assert_eq!(
            follow_target("https://example.com/b", current, 1, 0, false),
            None
        );
    }

    /// Serve a fixed redirect chain: /hop1 -> ... -> /hopN -> /end, then stop.
    async fn serve_redirect_chain(hops: usize) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => request.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let path = String::from_utf8_lossy(&request);
                    let path = path.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let response = if path == "/end" {
                        "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nfinal"
                            .to_string()
                    } else {
                        let hop: usize = path.trim_start_matches("/hop").parse().unwrap_or(0);
                        let location = if hop >= hops {
                            "/end".to_string()
                        } else {
                            format!("/hop{}", hop + 1)
                        };
                        format!(
                            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                    };
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        port
    }

    /// The HTTP redirect budget allows exactly `max_redirects` hops: a chain of
    /// that length reaches its final page, and one hop more is refused.
    #[tokio::test]
    async fn http_redirect_budget_allows_exactly_the_configured_hops() {
        let settings = FetchSettings {
            allow_private_networks: true,
            max_redirects: 5,
            ..FetchSettings::default()
        };
        let fetcher = Fetcher::new(settings, "search-test").expect("valid test client");

        let port = serve_redirect_chain(5).await;
        let page = fetcher
            .fetch(&format!("http://127.0.0.1:{port}/hop1"))
            .await
            .expect("a chain of exactly `max_redirects` hops is followed");
        assert_eq!(page.text, "final");

        let port = serve_redirect_chain(6).await;
        assert!(fetcher
            .fetch(&format!("http://127.0.0.1:{port}/hop1"))
            .await
            .is_err());
    }

    /// A page that is only a prompt redirect is followed even when it shows no
    /// text; a textless page with nowhere to go is still an empty error.
    #[tokio::test]
    async fn a_textless_meta_refresh_is_followed() {
        let port = serve(vec![
            (
                "/stub",
                "text/html",
                b"<meta http-equiv=\"refresh\" content=\"0; url=/end\">".to_vec(),
            ),
            ("/end", "text/plain", b"final".to_vec()),
        ])
        .await;
        let page = local_fetcher(DEFAULT_TEST_BYTES)
            .fetch(&format!("http://127.0.0.1:{port}/stub"))
            .await
            .expect("a textless stub with a target is followed");
        assert_eq!(page.text, "final");
        assert_eq!(page.url, format!("http://127.0.0.1:{port}/stub"));

        // With no hop budget the stub cannot be followed, so it is empty.
        let fetcher = Fetcher::new(
            FetchSettings {
                allow_private_networks: true,
                max_redirects: 0,
                ..FetchSettings::default()
            },
            "search-test",
        )
        .expect("valid test client");
        assert!(matches!(
            fetcher
                .fetch(&format!("http://127.0.0.1:{port}/stub"))
                .await,
            Err(FetchError::Empty)
        ));
    }

    /// A failed HTTP response keeps its status in the per-URL result, so a machine can
    /// tell a 404 from a request that never got a response.
    #[tokio::test]
    async fn failed_http_status_is_kept_per_url() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut chunk = [0u8; 1024];
                let _ = socket.read(&mut chunk).await;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
            }
        });
        let pages = local_fetcher(DEFAULT_TEST_BYTES)
            .fetch_many(&[format!("http://127.0.0.1:{port}/missing")])
            .await;
        assert_eq!(pages[0].status, 404);
        assert_eq!(pages[0].error.as_deref(), Some("HTTP 404"));
    }

    /// Serve fixed `(path, content type, body)` responses on a loopback port.
    async fn serve(routes: Vec<(&'static str, &'static str, Vec<u8>)>) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let routes = std::sync::Arc::new(routes);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let routes = std::sync::Arc::clone(&routes);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => request.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let request = String::from_utf8_lossy(&request);
                    let path = request.split_whitespace().nth(1).unwrap_or("/");
                    let (_, content_type, body) =
                        routes.iter().find(|(route, _, _)| *route == path).unwrap();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body).await;
                });
            }
        });
        port
    }

    fn local_fetcher(max_response_bytes: u64) -> Fetcher {
        let settings = FetchSettings {
            allow_private_networks: true,
            max_response_bytes,
            ..FetchSettings::default()
        };
        Fetcher::new(settings, "search-test").unwrap()
    }

    fn article(marker: &str) -> String {
        format!(
            "<article><h1>Ownership</h1>{}</article>",
            (0..6)
                .map(|i| format!(
                    "<p>{marker} paragraph {i}: the borrow checker tracks ownership, \
                     moves and borrows at compile time, so memory safety costs nothing at runtime.</p>"
                ))
                .collect::<String>()
        )
    }

    /// Modern pages carry hundreds of kilobytes of inline script and style
    /// before the article. The whole article must survive, untruncated.
    #[tokio::test]
    async fn an_article_after_a_large_head_is_read_whole() {
        let script = format!("<script>var bundle = '{}';</script>", "x".repeat(400_000));
        let html = format!(
            "<html><head><title>Big</title>{script}</head><body>{}</body></html>",
            article("LATE")
        );
        let port = serve(vec![(
            "/big",
            "text/html; charset=utf-8",
            html.into_bytes(),
        )])
        .await;
        let page = local_fetcher(DEFAULT_TEST_BYTES)
            .fetch(&format!("http://127.0.0.1:{port}/big"))
            .await
            .unwrap();
        assert!(page.text.contains("LATE paragraph 5"), "{}", page.text);
        assert!(!page.text.contains("bundle"));
        assert_eq!(page.truncated, None);
    }

    const DEFAULT_TEST_BYTES: u64 = 16 << 20;

    /// A body beyond the byte bound is reported as truncated, not passed off as whole.
    #[tokio::test]
    async fn the_byte_bound_reports_truncation() {
        let text = "line of plain text\n".repeat(1000);
        let port = serve(vec![("/long.txt", "text/plain", text.into_bytes())]).await;
        let page = local_fetcher(1000)
            .fetch(&format!("http://127.0.0.1:{port}/long.txt"))
            .await
            .unwrap();
        assert_eq!(page.truncated, Some(true));
        assert!(page.text.chars().count() <= 1000);
    }

    /// Legacy encodings are decoded from the header or the document's own meta tag.
    #[tokio::test]
    async fn declared_charsets_are_decoded() {
        let sentence = "私はその人を常に先生と呼んでいた。だからここでもただ先生と書くだけで本名は打ち明けない。";
        let html = format!(
            "<html><head><meta http-equiv=\"Content-Type\" content=\"text/html;charset=Shift_JIS\">\
             <title>こころ</title></head><body><article>{}</article></body></html>",
            format!("<p>{sentence}</p>").repeat(6)
        );
        let (shift_jis, _, _) = encoding_rs::SHIFT_JIS.encode(&html);
        let (latin, _, _) = encoding_rs::WINDOWS_1252.encode("<p>Café crème brûlée</p>");
        let port = serve(vec![
            ("/sjis", "text/html", shift_jis.into_owned()),
            (
                "/latin",
                "text/plain; charset=windows-1252",
                latin.into_owned(),
            ),
        ])
        .await;
        let fetcher = local_fetcher(DEFAULT_TEST_BYTES);
        let page = fetcher
            .fetch(&format!("http://127.0.0.1:{port}/sjis"))
            .await
            .unwrap();
        assert!(page.text.contains(sentence), "{}", page.text);
        let page = fetcher
            .fetch(&format!("http://127.0.0.1:{port}/latin"))
            .await
            .unwrap();
        assert!(page.text.contains("Café crème brûlée"), "{}", page.text);
    }

    /// A PDF's text layer is read, even when the server mislabels it.
    #[tokio::test]
    async fn pdf_text_is_extracted() {
        let pdf = minimal_pdf("Search reads portable documents");
        let port = serve(vec![
            ("/paper.pdf", "application/pdf", pdf.clone()),
            ("/download", "application/octet-stream", pdf),
        ])
        .await;
        let fetcher = local_fetcher(DEFAULT_TEST_BYTES);
        for path in ["/paper.pdf", "/download"] {
            let page = fetcher
                .fetch(&format!("http://127.0.0.1:{port}{path}"))
                .await
                .unwrap();
            assert!(
                page.text.contains("Search reads portable documents"),
                "{path}: {:?}",
                page.text
            );
        }
    }

    /// Images and other binaries are refused by type instead of returned as noise.
    #[tokio::test]
    async fn binary_bodies_are_refused() {
        let port = serve(vec![
            (
                "/logo.png",
                "image/png",
                b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec(),
            ),
            ("/blob", "", b"\0\x01\x02binary".to_vec()),
        ])
        .await;
        let fetcher = local_fetcher(DEFAULT_TEST_BYTES);
        for path in ["/logo.png", "/blob"] {
            let error = fetcher
                .fetch(&format!("http://127.0.0.1:{port}{path}"))
                .await
                .unwrap_err();
            assert!(
                matches!(error, FetchError::Unsupported(_)),
                "{path}: {}",
                error.message()
            );
        }
    }

    /// A one-page PDF whose text layer is `text`, with a correct xref table.
    fn minimal_pdf(text: &str) -> Vec<u8> {
        let stream = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_string(),
            format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                .to_string(),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend(format!("{} 0 obj\n{object}\nendobj\n", index + 1).bytes());
        }
        let xref = pdf.len();
        pdf.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
        for offset in offsets {
            pdf.extend(format!("{offset:010} 00000 n \n").bytes());
        }
        pdf.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .bytes(),
        );
        pdf
    }

    /// Mixed HTTP and meta redirects spend one allowance, including subsequent
    /// HTTP chains; repeated calls must not bypass the limit through cached stubs.
    #[tokio::test]
    async fn ordinary_fetch_shares_http_and_meta_redirect_allowance() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = hits.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut bytes = [0; 8192];
                let count = socket.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..count]);
                let path = request.split_whitespace().nth(1).unwrap().to_string();
                recorded.lock().unwrap().push(path.clone());
                let (status, headers, body) = match path.as_str() {
                    "/a" => (302, "Location: /b\r\n", ""),
                    "/b" => (
                        200,
                        "Content-Type: text/html\r\n",
                        "<meta http-equiv='refresh' content='0; url=/c'>",
                    ),
                    "/c" => (302, "Location: /d\r\n", ""),
                    "/d" => (200, "Content-Type: text/plain\r\n", "Final"),
                    _ => (404, "", ""),
                };
                let wire = format!("HTTP/1.1 {status} fixture\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}", body.len());
                let _ = socket.write_all(wire.as_bytes()).await;
            }
        });
        let settings = |limit| FetchSettings {
            allow_private_networks: true,
            max_redirects: limit,
            ..Default::default()
        };
        for (limit, paths) in [
            (1, vec!["/a", "/b"]),
            (2, vec!["/a", "/b", "/c"]),
            (3, vec!["/a", "/b", "/c", "/d"]),
        ] {
            let fetcher = Fetcher::new(settings(limit), "Search/1").unwrap();
            for _ in 0..2 {
                hits.lock().unwrap().clear();
                let fetched = fetcher.fetch(&format!("{base}/a")).await;
                assert_eq!(hits.lock().unwrap().as_slice(), paths);
                if limit == 3 {
                    assert_eq!(fetched.unwrap().text, "Final");
                } else {
                    assert!(fetched.is_err());
                }
            }
        }
        task.abort();
    }

    /// Uncompressed framing proves that an exact-cap PDF is complete and extractable.
    #[tokio::test]
    async fn crawl_pdf_exactly_at_content_length_cap_remains_readable() {
        let pdf = minimal_pdf("A complete exact-cap document");
        let size = pdf.len() as u64;
        let port = serve(vec![("/paper.pdf", "application/pdf", pdf)]).await;
        let fetcher = local_fetcher(size);
        let url = url::Url::parse(&format!("http://127.0.0.1:{port}/paper.pdf")).unwrap();
        let mut bytes = 0;
        let raw = fetcher
            .crawl_response(
                &url,
                size,
                &mut bytes,
                &Default::default(),
                Duration::ZERO,
                tokio::time::Instant::now() + Duration::from_secs(15),
            )
            .await
            .unwrap();
        assert!(!raw.truncated);
        assert_eq!(bytes, size);
        let page = fetcher
            .crawl_page(&url, raw, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(page.text.contains("A complete exact-cap document"));
        assert_eq!(page.truncated, None);
    }

    /// An unknown body at the cap stops immediately rather than waiting for its
    /// next chunk or EOF. Conservative truncation does not trigger extra reads.
    #[tokio::test]
    async fn unknown_length_at_cap_stops_without_reading_excess() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = url::Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nText\r\n")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let mut bytes = 0;
        let fetcher = local_fetcher(4);
        let raw = tokio::time::timeout(
            Duration::from_secs(1),
            fetcher.crawl_response(
                &url,
                4,
                &mut bytes,
                &Default::default(),
                Duration::ZERO,
                tokio::time::Instant::now() + Duration::from_secs(15),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        task.abort();
        assert_eq!(raw.body, b"Text");
        assert_eq!(bytes, 4);
        assert!(raw.truncated);
    }
}
