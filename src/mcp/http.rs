//! The streamable HTTP transport for MCP, mounted by the CLI at `/mcp`.

use std::sync::Arc;

use crate::core::Search;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};

use super::Server;

/// Build the tower service that serves MCP over streamable HTTP. A new
/// `Server` is created per session, sharing the configured `Search` engine.
///
/// `allowed_host` is the authority the server is reached by (its bind address,
/// or the hostname a proxy forwards). rmcp validates the inbound `Host` against
/// this list to prevent DNS rebinding against a locally running server, so the
/// deployment must name itself here — a request whose `Host` is not listed is
/// refused. Loopback forms are always allowed.
pub fn mount(
    search: Arc<Search>,
    allowed_host: &str,
) -> StreamableHttpService<Server, LocalSessionManager> {
    mount_with_manager(
        search,
        allowed_host,
        Arc::new(LocalSessionManager::default()),
    )
}

/// Share the actual transport registry with host admission so expiring an
/// authenticated session also closes its transport, rather than orphaning it.
pub(crate) fn mount_with_manager(
    search: Arc<Search>,
    allowed_host: &str,
    manager: Arc<LocalSessionManager>,
) -> StreamableHttpService<Server, LocalSessionManager> {
    let mut hosts = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    if !allowed_host.is_empty() {
        hosts.push(allowed_host.to_string());
        // A `host:port` bind also answers by bare host for a proxy that strips
        // the port; keep both forms.
        if let Some((host, _)) = allowed_host.rsplit_once(':') {
            hosts.push(host.to_string());
        }
    }
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(hosts)
        // One request body is at most a small JSON-RPC envelope; bound it so a
        // hostile client cannot stream an unbounded frame.
        .with_max_request_body_bytes(1 << 20)
        .with_sse_keep_alive(Some(std::time::Duration::from_secs(30)));
    StreamableHttpService::new(
        move || Ok(Server::new(Arc::clone(&search))),
        manager,
        config,
    )
}
