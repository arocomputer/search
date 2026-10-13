//! The HTTP surface: one small JSON API and the streamable MCP endpoint on one
//! listener. Paired `Client`s use the same API as scripts.
//!
//! Paired HTTPS protects every API and MCP request; pairing alone is public.
//! Inputs are checked with the shared `client` validators before execution.

use std::sync::Arc;

use crate::core::{Query, Search};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Extension;
use axum::{Json, Router};

use crate::cli::{auth::Host, run::build_service};
use crate::client::{Client, DeviceIdentity, ReadRequest, ResearchRequest};

/// Host the local engine behind paired HTTPS until interrupted.
pub async fn serve(
    config_path: Option<String>,
    address: Option<String>,
    hostname: Option<String>,
) -> i32 {
    let service = match build_service(config_path, address) {
        Ok(service) => service,
        Err(message) => {
            eprintln!("search: {message}");
            return 1;
        }
    };
    let bind_hostname = service
        .config()
        .address
        .rsplit_once(':')
        .map(|(host, _)| host.trim_matches(['[', ']']))
        .unwrap_or("localhost");
    let hostname = hostname.as_deref().unwrap_or(bind_hostname);
    if matches!(hostname, "" | "0.0.0.0" | "::") {
        eprintln!("search: wildcard binds require -hostname NAME for the TLS identity");
        return 1;
    }
    let listener = match std::net::TcpListener::bind(&service.config().address) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("search: bind: {error}");
            return 1;
        }
    };
    let host = match Host::open(
        &service.config().home,
        hostname,
        std::time::Duration::from_secs(900),
    ) {
        Ok(host) => Arc::new(host),
        Err(error) => {
            eprintln!("search: {error}");
            return 1;
        }
    };
    let tls = match axum_server::tls_rustls::RustlsConfig::from_pem(
        host.cert.as_bytes().to_vec(),
        host.key.as_bytes().to_vec(),
    )
    .await
    {
        Ok(tls) => tls,
        Err(error) => {
            eprintln!("search: TLS identity: {error}");
            return 1;
        }
    };
    eprintln!(
        "search: certificate {}",
        host.path.join("host.pem").display()
    );
    eprintln!("search: SHA-256 {}", host.fingerprint);
    eprintln!(
        "search: one-use pairing code {} (expires in 15 minutes; 20 attempts)",
        host.code
    );
    eprintln!(
        "search: listening on {} (version {})",
        service.config().address,
        crate::VERSION
    );
    eprintln!("search: run search pair-code with the same local settings to pair another device");
    let handle = axum_server::Handle::new();
    let shutdown_handle = handle.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
    });
    // MCP validates the advertised authority rather than a wildcard bind address.
    let authority = match service.config().address.rsplit_once(':') {
        Some((_, port)) if hostname.contains(':') => format!("[{hostname}]:{port}"),
        Some((_, port)) => format!("{hostname}:{port}"),
        None => hostname.to_string(),
    };
    let server = match axum_server::from_tcp_rustls(listener, tls) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("search: listener: {error}");
            return 1;
        }
    };
    if let Err(error) = server
        .handle(handle)
        .serve(router(service, host, &authority).into_make_service())
        .await
    {
        eprintln!("search: server stopped: {error}");
        return 1;
    }
    0
}

/// Protect every execution route; `allowed_host` is the advertised MCP authority.
pub fn router(service: Arc<Search>, host: Arc<Host>, allowed_host: &str) -> Router {
    let manager = Arc::new(
        rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default(),
    );
    let mcp = crate::mcp::mount_with_manager(Arc::clone(&service), allowed_host, manager.clone());
    let authorization = Arc::new(Authorization {
        host: Arc::clone(&host),
        sessions: std::sync::Mutex::new(std::collections::HashMap::new()),
        ttl: std::time::Duration::from_millis(service.config().research.session_ttl),
        slots: Arc::new(tokio::sync::Semaphore::new(
            service.config().research.session_entries,
        )),
        capacity: service.config().research.session_entries,
        manager,
    });
    let protected = Router::new()
        .route("/healthz", get(health))
        .route("/v1/status", get(status))
        .route("/v1/search", get(search).post(search_post))
        .route("/v1/fetch", post(fetch))
        .route("/v1/research", post(research))
        .route("/v1/crawl", post(crawl))
        .route("/v1/map", post(map))
        .nest_service("/mcp", mcp)
        .layer(axum::middleware::from_fn_with_state(authorization, auth))
        .with_state(service);
    protected
        .merge(Router::new().route("/pair", post(pair)).with_state(host))
        .layer(axum::extract::DefaultBodyLimit::max(1 << 20))
}

/// Bind MCP transport sessions to authenticated devices; state expires and is bounded.
struct Authorization {
    host: Arc<Host>,
    sessions: std::sync::Mutex<std::collections::HashMap<String, TransportOwner>>,
    ttl: std::time::Duration,
    slots: Arc<tokio::sync::Semaphore>,
    capacity: usize,
    manager: Arc<rmcp::transport::streamable_http_server::session::local::LocalSessionManager>,
}

/// A live transport owns one admission slot until deletion or expiry.
struct TransportOwner {
    identity: String,
    touched: std::time::Instant,
    _slot: tokio::sync::OwnedSemaphorePermit,
}

impl Authorization {
    /// Remove expired ownership and close its real MCP transport before allowing
    /// replacement initialization. No registry lock is held across async work.
    async fn expire(self: &Arc<Self>) -> Result<(), ()> {
        let live = self.manager.sessions.read().await;
        use rmcp::transport::streamable_http_server::SessionManager;
        let expired = {
            let mut sessions = self.sessions.lock().map_err(|_| ())?;
            let keys: Vec<String> = sessions
                .iter()
                .filter(|(key, owner)| {
                    !live.contains_key(key.as_str())
                        || (!self.ttl.is_zero() && owner.touched.elapsed() >= self.ttl)
                })
                .map(|(key, _)| key.clone())
                .collect();
            keys.into_iter()
                .filter_map(|key| sessions.remove(&key).map(|owner| (key, owner)))
                .collect::<Vec<_>>()
        };
        drop(live);
        if expired.is_empty() {
            return Ok(());
        }
        let manager = self.manager.clone();
        // Cleanup must finish even if the request that triggered expiry disconnects.
        tokio::spawn(async move {
            for (key, owner) in expired {
                let _ = manager.close_session(&Arc::from(key)).await;
                drop(owner);
            }
        })
        .await
        .map_err(|_| ())
    }
}

/// Refuse browser requests, verify credentials and bind MCP sessions to their owners.
async fn auth(
    State(authorization): State<Arc<Authorization>>,
    headers: HeaderMap,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if headers.contains_key(axum::http::header::ORIGIN) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !authorization.host.authorized(presented) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Identity comes only from a verified secret; never trust device IDs in input.
    let namespace = crate::client::session_namespace(presented);
    let mcp = request.uri().path() == "/mcp" || request.uri().path().starts_with("/mcp/");
    let mut admission = None;
    if mcp {
        if authorization.expire().await.is_err() {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        if let Some(session) = headers.get("mcp-session-id") {
            let Ok(session) = session.to_str() else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            let Ok(mut sessions) = authorization.sessions.lock() else {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            };
            match sessions.get_mut(session) {
                Some(owner) if owner.identity == namespace => {
                    owner.touched = std::time::Instant::now()
                }
                _ => return StatusCode::FORBIDDEN.into_response(),
            }
        } else {
            // Include transports still shutting down after a failed initialization.
            if authorization.manager.sessions.read().await.len() >= authorization.capacity {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            // Reserve BEFORE the transport factory runs, including concurrent
            // initialization. The lifecycle task retains it through cancellation.
            match authorization.slots.clone().try_acquire_owned() {
                Ok(slot) => admission = Some(slot),
                Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            }
        }
    }
    request
        .extensions_mut()
        .insert(DeviceIdentity(namespace.clone()));
    let deleted_session = (mcp && request.method() == axum::http::Method::DELETE)
        .then(|| {
            headers
                .get("mcp-session-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        })
        .flatten();
    if !mcp {
        return next.run(request).await;
    }
    mcp_response(
        authorization,
        namespace,
        admission,
        deleted_session,
        next.run(request),
    )
    .await
}

/// Finish transport ownership and deletion independently of HTTP cancellation.
/// If initialization loses its caller, close the new transport before releasing capacity.
async fn mcp_response(
    authorization: Arc<Authorization>,
    namespace: String,
    admission: Option<tokio::sync::OwnedSemaphorePermit>,
    deleted_session: Option<String>,
    response: impl std::future::Future<Output = Response> + Send + 'static,
) -> Response {
    if admission.is_none() && deleted_session.is_none() {
        return response.await;
    }
    let (sender, receiver) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        use rmcp::transport::streamable_http_server::SessionManager;
        let response = response.await;
        let initialized_session = admission.as_ref().and_then(|_| {
            response
                .headers()
                .get("mcp-session-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        });
        if response.status().is_success() {
            if let Some(session) = &initialized_session {
                if let Ok(mut sessions) = authorization.sessions.lock() {
                    if let Some(slot) = admission {
                        sessions.insert(
                            session.clone(),
                            TransportOwner {
                                identity: namespace,
                                touched: std::time::Instant::now(),
                                _slot: slot,
                            },
                        );
                    }
                }
            }
            if let Some(session) = deleted_session {
                if let Ok(mut sessions) = authorization.sessions.lock() {
                    sessions.remove(&session);
                }
            }
        }
        if sender.send(response).is_err() {
            if let Some(session) = initialized_session {
                let _ = authorization
                    .manager
                    .close_session(&Arc::from(session.clone()))
                    .await;
                if let Ok(mut sessions) = authorization.sessions.lock() {
                    sessions.remove(&session);
                }
            }
        }
    });
    receiver
        .await
        .unwrap_or_else(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())
}

/// A pairing request carries the one-time code, never an existing device credential.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PairBody {
    code: String,
    name: String,
}

/// Enroll one device; deliberately return generic failures.
async fn pair(
    State(host): State<Arc<Host>>,
    headers: HeaderMap,
    Json(body): Json<PairBody>,
) -> Response {
    if headers.contains_key(axum::http::header::ORIGIN) {
        return StatusCode::FORBIDDEN.into_response();
    }
    match host.pair(&body.code, &body.name) {
        Ok(credential) => Json(credential).into_response(),
        Err(_) => StatusCode::FORBIDDEN.into_response(),
    }
}

/// Query parameters for the search endpoint.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchParams {
    q: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    engines: Option<String>,
}

/// `GET /v1/search`: one `Answer`. An omitted limit means the host's configured
/// maximum; `engines` is a comma-separated list of package IDs.
async fn search(
    State(service): State<Arc<Search>>,
    axum::extract::Query(params): axum::extract::Query<SearchParams>,
) -> Response {
    let engines = params
        .engines
        .map(|p| {
            p.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    search_post(
        State(service),
        Json(Query {
            text: params.q,
            limit: params.limit.unwrap_or(0),
            engines,
            ..Default::default()
        }),
    )
    .await
}

/// `POST /v1/search` preserves the full directional query shape.
async fn search_post(State(service): State<Arc<Search>>, Json(query): Json<Query>) -> Response {
    match Client::Local(service).search(query).await {
        Ok(answer) => Json(answer).into_response(),
        Err(message) => bad_request(&message),
    }
}

/// `POST /v1/fetch` returns bounded reads; full content and links are opt-in.
async fn fetch(State(service): State<Arc<Search>>, Json(body): Json<ReadRequest>) -> Response {
    match Client::Local(service).read(body).await {
        Ok(pages) => Json(serde_json::json!({ "pages": pages })).into_response(),
        Err(message) => bad_request(&message),
    }
}

/// `POST /v1/research` resolves host defaults and scopes sessions per device.
async fn research(
    State(service): State<Arc<Search>>,
    Extension(identity): Extension<DeviceIdentity>,
    Json(body): Json<ResearchRequest>,
) -> Response {
    match Client::scoped_local(service, identity.0)
        .research(body)
        .await
    {
        Ok(packet) => Json(packet).into_response(),
        Err(message) => bad_request(&message),
    }
}

/// `POST /v1/crawl` applies the host's guarded inventory policy.
async fn crawl(
    State(service): State<Arc<Search>>,
    Json(body): Json<crate::core::CrawlRequest>,
) -> Response {
    match Client::Local(service).crawl(body).await {
        Ok(report) => Json(report).into_response(),
        Err(message) => bad_request(&message),
    }
}

/// `POST /v1/map` uses the same bounded crawler with inventory-only output.
async fn map(
    State(service): State<Arc<Search>>,
    Json(body): Json<crate::core::CrawlRequest>,
) -> Response {
    match Client::Local(service).map(body).await {
        Ok(report) => Json(report).into_response(),
        Err(message) => bad_request(&message),
    }
}

/// `GET /v1/status`: the host version and its selected engine names.
async fn status(State(service): State<Arc<Search>>) -> Response {
    Json(serde_json::json!({
        "version": crate::VERSION,
        "engines": service.engine_names(),
    }))
    .into_response()
}

/// `GET /healthz`.
async fn health() -> Response {
    Json(serde_json::json!({"status": "ok", "version": crate::VERSION})).into_response()
}

/// A JSON 400 naming the violated bound.
fn bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": message })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::SessionManager;

    /// Isolate the real transport manager and one-slot admission registry for lifecycle tests.
    fn authorization() -> (Arc<Authorization>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("search-admission-{}", uuid::Uuid::new_v4()));
        let authorization = Arc::new(Authorization {
            host: Arc::new(
                Host::open(&root, "localhost", std::time::Duration::from_secs(60)).unwrap(),
            ),
            sessions: std::sync::Mutex::new(Default::default()),
            ttl: std::time::Duration::from_secs(60),
            slots: Arc::new(tokio::sync::Semaphore::new(1)),
            capacity: 1,
            manager: Arc::new(LocalSessionManager::default()),
        });
        (authorization, root)
    }

    /// Cancelling an initialization retains admission until its orphan transport is closed.
    #[tokio::test]
    async fn cancelled_initialization_closes_transport_before_releasing_admission() {
        let (authorization, root) = authorization();
        let slot = authorization.slots.clone().try_acquire_owned().unwrap();
        let (session, _transport) = authorization.manager.create_session().await.unwrap();
        let ready = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let response = {
            let ready = ready.clone();
            let finish = finish.clone();
            let session = session.clone();
            async move {
                ready.notify_one();
                finish.notified().await;
                let mut response = StatusCode::OK.into_response();
                response
                    .headers_mut()
                    .insert("mcp-session-id", session.parse().unwrap());
                response
            }
        };
        let request = tokio::spawn(mcp_response(
            authorization.clone(),
            "device".into(),
            Some(slot),
            None,
            response,
        ));
        ready.notified().await;
        request.abort();
        let _ = request.await;
        assert_eq!(authorization.slots.available_permits(), 0);
        finish.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while authorization.slots.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!authorization.manager.has_session(&session).await.unwrap());
        assert!(authorization.sessions.lock().unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// rmcp can close an idle transport earlier than host TTL; its admission must be reusable.
    #[tokio::test]
    async fn expiry_releases_ownership_for_already_closed_transports() {
        let (authorization, root) = authorization();
        let (session, _transport) = authorization.manager.create_session().await.unwrap();
        let slot = authorization.slots.clone().try_acquire_owned().unwrap();
        authorization.sessions.lock().unwrap().insert(
            session.to_string(),
            TransportOwner {
                identity: "device".into(),
                touched: std::time::Instant::now(),
                _slot: slot,
            },
        );
        authorization.manager.close_session(&session).await.unwrap();
        authorization.expire().await.unwrap();
        assert_eq!(authorization.slots.available_permits(), 1);
        assert!(authorization.sessions.lock().unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A disconnect after transport deletion cannot strand the corresponding ownership slot.
    #[tokio::test]
    async fn cancelled_deletion_still_releases_admission() {
        let (authorization, root) = authorization();
        let (session, _transport) = authorization.manager.create_session().await.unwrap();
        let slot = authorization.slots.clone().try_acquire_owned().unwrap();
        authorization.sessions.lock().unwrap().insert(
            session.to_string(),
            TransportOwner {
                identity: "device".into(),
                touched: std::time::Instant::now(),
                _slot: slot,
            },
        );
        let ready = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let response = {
            let manager = authorization.manager.clone();
            let ready = ready.clone();
            let finish = finish.clone();
            let session = session.clone();
            async move {
                manager.close_session(&session).await.unwrap();
                ready.notify_one();
                finish.notified().await;
                StatusCode::OK.into_response()
            }
        };
        let request = tokio::spawn(mcp_response(
            authorization.clone(),
            "device".into(),
            None,
            Some(session.to_string()),
            response,
        ));
        ready.notified().await;
        request.abort();
        let _ = request.await;
        finish.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while authorization.slots.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(authorization.sessions.lock().unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
