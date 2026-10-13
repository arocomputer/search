//! MCP tools and transports for the Search engine.
//!
//! Search, bounded reading, research and site inventory tools. This module owns
//! tool handlers and both stdio and streamable HTTP transports; the CLI decides where to mount them.

use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::stdio;
use rmcp::{
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};

use crate::core::text::FocusedPage;
use crate::core::{
    Answer, CrawlOptions, CrawlRequest, Query, QueryFilters, ResearchOptions, Search,
};

mod http;
use crate::client::{
    validate_crawl, validate_filters, validate_query, validate_read, validate_research, Client,
    ReadRequest, ResearchRequest, MAX_QUERIES,
};

pub use http::mount;
#[cfg(feature = "cli")]
pub(crate) use http::mount_with_manager;

/// The MCP server over the in-process search engine.
#[derive(Clone)]
pub struct Server {
    search: Client,
    /// Read by the `#[tool_handler]` macro's generated dispatch, which clippy's
    /// field-usage analysis cannot see. Scoped allow, proof: macro-generated use.
    #[allow(dead_code)]
    tool_router: ToolRouter<Server>,
}

/// Protocol schema for directional filters; core stays free of MCP dependencies.
#[derive(Debug, Clone, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Filters {
    pub freshness: Option<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub include_domains: Vec<String>,
    pub exclude_domains: Vec<String>,
    pub language: Option<String>,
    pub region: Option<String>,
    pub category: Option<String>,
    pub safe: Option<bool>,
    pub exact_phrase: Option<String>,
}

impl From<Filters> for QueryFilters {
    /// Convert schema data without adding protocol traits to core.
    fn from(value: Filters) -> Self {
        Self {
            freshness: value.freshness,
            start_date: value.start_date,
            end_date: value.end_date,
            include_domains: value.include_domains,
            exclude_domains: value.exclude_domains,
            language: value.language,
            region: value.region,
            category: value.category,
            safe: value.safe,
            exact_phrase: value.exact_phrase,
        }
    }
}

/// Schema for bounded same-origin crawling and mapping; zero uses host settings.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CrawlArgs {
    pub url: String,
    pub max_pages: usize,
    pub max_depth: usize,
    pub max_bytes: u64,
    pub timeout: u64,
    pub path_prefix: Option<String>,
    /// Per-page text characters; zero uses the host's reading default.
    pub max_characters: usize,
    /// Total page text characters; zero uses the host's research budget.
    pub total_characters: usize,
    /// Explicitly return full page text instead of character windows.
    pub full: bool,
}

impl From<CrawlArgs> for CrawlRequest {
    /// Preserve numeric sentinels for host-side default resolution.
    fn from(value: CrawlArgs) -> Self {
        Self {
            url: value.url,
            options: CrawlOptions {
                max_pages: value.max_pages,
                max_depth: value.max_depth,
                max_bytes: value.max_bytes,
                timeout: value.timeout,
                path_prefix: value.path_prefix,
                max_characters: value.max_characters,
                total_characters: value.total_characters,
                full: value.full,
            },
        }
    }
}

/// Arguments for `web_search`.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchArgs {
    /// Directional constraints; unsupported filters are reported per engine.
    #[serde(default)]
    pub filters: Filters,
    /// One to five queries (512 bytes each), reported in input order.
    pub queries: Vec<String>,
    /// Maximum results per query, at most 50. When omitted, the host's
    /// configured maximum (`search.max_results`, normally 10), which also caps
    /// any larger request.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Restrict to selected engine IDs, such as "mwmbl" or "searxng".
    #[serde(default)]
    pub engines: Option<Vec<String>>,
}

/// Arguments for `web_fetch`.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FetchArgs {
    /// Explicitly return full text; excludes query, limit and offset.
    #[serde(default)]
    pub full: bool,
    /// Include discovered public links without adding them to prose.
    #[serde(default)]
    pub links: bool,
    /// One to ten http or https URLs to read.
    pub urls: Vec<String>,
    /// What you are looking for in these pages. When it matches, only the
    /// relevant passages are returned instead of the whole page.
    #[serde(default)]
    pub query: Option<String>,
    /// Character window per page; omitted uses the executing host settings.
    #[serde(default)]
    pub max_characters: Option<usize>,
    /// Character offset to continue reading from: pass a page's `next_offset`
    /// from an earlier call to read what followed.
    #[serde(default)]
    pub offset: Option<usize>,
}

/// Nested research options mirror inert core data without adding protocol traits there.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ResearchOptionsArgs {
    pub sources: usize,
    pub source_characters: usize,
    pub total_characters: usize,
    pub max_per_host: usize,
    pub max_attempts: usize,
    pub timeout: u64,
    pub engines: Vec<String>,
    pub filters: Filters,
    pub session: Option<String>,
}

impl From<ResearchOptionsArgs> for ResearchOptions {
    /// Keep zero bounds unresolved until execution on the host.
    fn from(value: ResearchOptionsArgs) -> Self {
        Self {
            sources: value.sources,
            source_characters: value.source_characters,
            total_characters: value.total_characters,
            max_per_host: value.max_per_host,
            max_attempts: value.max_attempts,
            timeout: value.timeout,
            engines: value.engines,
            filters: value.filters.into(),
            session: value.session,
        }
    }
}

/// Arguments for `web_research`.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResearchArgs {
    /// Optional grouped bounds, filters and session. Excludes the equivalent flat fields.
    #[serde(default)]
    pub options: Option<ResearchOptionsArgs>,
    /// Directional constraints applied to each research intent.
    #[serde(default)]
    pub filters: Filters,
    #[serde(default)]
    pub max_per_host: usize,
    #[serde(default)]
    pub max_attempts: usize,
    /// Overall research deadline in milliseconds; zero uses host settings.
    #[serde(default)]
    pub timeout: u64,
    /// Skip previously returned useful evidence within this caller's session.
    #[serde(default)]
    pub session: Option<String>,
    /// Opt in to the host's configured synthesizer.
    #[serde(default)]
    pub synthesize: bool,
    /// Opt in to the host's configured reranker.
    #[serde(default)]
    pub rerank: bool,
    /// One to five intents, assembled into one cited packet.
    pub queries: Vec<String>,
    /// Sources to return; omitted uses host settings, safety ceiling 10.
    #[serde(default)]
    pub sources: Option<usize>,
    /// Characters per source; omitted uses host settings, safety ceiling 50000.
    #[serde(default)]
    pub source_characters: Option<usize>,
    /// Packet characters; omitted uses host settings, safety ceiling 200000.
    #[serde(default)]
    pub total_characters: Option<usize>,
    /// Restrict to selected engine IDs, such as "mwmbl" or "searxng".
    #[serde(default)]
    pub engines: Option<Vec<String>>,
}

/// The JSON shape a search tool returns: one answer per query, in order.
#[derive(serde::Serialize)]
struct SearchOutput {
    queries: Vec<Answer>,
}

/// The combined answer the fetch tool returns.
#[derive(serde::Serialize)]
struct FetchOutput {
    pages: Vec<FocusedPage>,
}

#[tool_router]
impl Server {
    /// Build the MCP server for an in-process search engine.
    pub fn new(search: Arc<Search>) -> Self {
        Server {
            search: Client::scoped_local(search, uuid::Uuid::new_v4().to_string()),
            tool_router: Self::tool_router(),
        }
    }

    /// Build the same tool surface over an exclusive selected client.
    pub fn with_client(search: Client) -> Self {
        let search = match search {
            Client::Local(search) => Client::scoped_local(search, uuid::Uuid::new_v4().to_string()),
            client => client,
        };
        Self {
            search,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "web_search",
        description = "Search the live web using the selected engines. Returns ranked titles, URLs, snippets, and engine status. Read relevant pages with web_fetch; use web_fetch directly when you already have a URL."
    )]
    async fn web_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let queries = validate_queries(args.queries)
            .map_err(|message| McpError::invalid_params(message, None))?;
        // Zero asks for the host maximum; the client caps explicit limits.
        let limit = args.limit.unwrap_or(0);
        let engines = args.engines.unwrap_or_default();
        let filters = validate_filters(args.filters.into()).map_err(invalid_params)?;
        let responses = futures::future::join_all(queries.into_iter().map(|text| {
            self.search.search(Query {
                text,
                limit,
                engines: engines.clone(),
                filters: filters.clone(),
            })
        }))
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(client_error)?;
        json_result(SearchOutput { queries: responses })
    }

    #[tool(
        name = "web_fetch",
        description = "Read one or more public URLs (HTML, text or PDF) as clean text. By default returns a bounded character window using host settings. Pass full to read all text; links opts in to link discovery. Pass a query for relevant excerpts, or max_characters to read a window; a page that continues reports next_offset, which you pass back as offset to read on. Private and link-local addresses are refused."
    )]
    async fn web_fetch(
        &self,
        Parameters(args): Parameters<FetchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let request = validate_read(ReadRequest {
            urls: args.urls,
            query: args.query,
            max_characters: args.max_characters,
            offset: args.offset.unwrap_or(0),
            full: args.full,
            links: args.links,
        })
        .map_err(invalid_params)?;
        let focused = self.search.read(request).await.map_err(client_error)?;
        json_result(FetchOutput { pages: focused })
    }

    #[tool(
        name = "web_research",
        description = "Research a question end to end: run one to five intents, read the best distinct sources, and return a bounded, cited packet of dated passages with a coverage report. Use this instead of chaining web_search and web_fetch when you need evidence to ground an answer. Private and link-local addresses are refused."
    )]
    async fn web_research(
        &self,
        Parameters(args): Parameters<ResearchArgs>,
        extensions: rmcp::model::Extensions,
    ) -> Result<CallToolResult, McpError> {
        if args.options.is_some()
            && (args.sources.is_some()
                || args.source_characters.is_some()
                || args.total_characters.is_some()
                || args.max_per_host != 0
                || args.max_attempts != 0
                || args.timeout != 0
                || args.engines.is_some()
                || args.session.is_some()
                || !QueryFilters::from(args.filters.clone()).values().is_empty())
        {
            return Err(McpError::invalid_params(
                "options cannot be combined with flat research options",
                None,
            ));
        }
        let request = ResearchRequest {
            queries: args.queries,
            options: args
                .options
                .map(ResearchOptions::from)
                .unwrap_or(ResearchOptions {
                    sources: args.sources.unwrap_or(0),
                    source_characters: args.source_characters.unwrap_or(0),
                    total_characters: args.total_characters.unwrap_or(0),
                    max_per_host: args.max_per_host,
                    max_attempts: args.max_attempts,
                    timeout: args.timeout,
                    session: args.session,
                    filters: args.filters.into(),
                    engines: args.engines.unwrap_or_default(),
                }),
            synthesize: args.synthesize,
            rerank: args.rerank,
        };
        let request = validate_research(request).map_err(invalid_params)?;
        let packet = self
            .research_client(&extensions)
            .research(request)
            .await
            .map_err(client_error)?;
        json_result(packet)
    }
    #[tool(
        name = "web_crawl",
        description = "Crawl a bounded same-origin public site, respecting robots and host limits. Returns clean pages, URL inventory and explicit failures. Private addresses are refused."
    )]
    async fn web_crawl(
        &self,
        Parameters(args): Parameters<CrawlArgs>,
    ) -> Result<CallToolResult, McpError> {
        let request = validate_crawl(args.into()).map_err(invalid_params)?;
        json_result(self.search.crawl(request).await.map_err(client_error)?)
    }

    #[tool(
        name = "web_map",
        description = "Map a bounded same-origin public site, respecting robots and host limits. Returns URL inventory and failures without page prose. Private addresses are refused."
    )]
    async fn web_map(
        &self,
        Parameters(args): Parameters<CrawlArgs>,
    ) -> Result<CallToolResult, McpError> {
        let request = validate_crawl(args.into()).map_err(invalid_params)?;
        json_result(self.search.map(request).await.map_err(client_error)?)
    }
}

impl Server {
    /// HTTP research uses verified device identity; stdio retains its local caller scope.
    fn research_client(&self, _extensions: &rmcp::model::Extensions) -> Client {
        #[cfg(feature = "cli")]
        if let Some(identity) = _extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<crate::client::DeviceIdentity>())
        {
            match &self.search {
                Client::Local(search) | Client::ScopedLocal { search, .. } => {
                    return Client::scoped_local(Arc::clone(search), identity.0.clone())
                }
                Client::Remote { .. } => {}
            }
        }
        self.search.clone()
    }
}

#[tool_handler]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("search", crate::VERSION))
            .with_instructions(
                "Search the live web and read pages. Use web_search to find sources, \
                 web_fetch to read the ones that matter, and web_research to gather a \
                 bounded, cited packet of evidence for a question in one call. Results \
                 report which engines answered, so an empty answer is never mistaken for \
                 a broken one.",
            )
    }
}

/// Identify shared input-validation failures as caller errors in the protocol.
fn invalid_params(message: String) -> McpError {
    McpError::invalid_params(message, None)
}

/// Surface routing failures as MCP errors without substituting local answers.
fn client_error(message: String) -> McpError {
    McpError::internal_error(message, None)
}

/// Return both structured JSON and readable JSON text for MCP clients.
fn json_result<T: serde::Serialize>(value: T) -> Result<CallToolResult, McpError> {
    let value = serde_json::to_value(value)
        .map_err(|e| McpError::internal_error(format!("serialize result: {e}"), None))?;
    Ok(CallToolResult::structured(value))
}

/// Require one to five independent queries, each trimmed by the shared query bound.
fn validate_queries(queries: Vec<String>) -> Result<Vec<String>, String> {
    if !(1..=MAX_QUERIES).contains(&queries.len()) {
        return Err("provide between one and five queries".into());
    }
    queries.iter().map(|query| validate_query(query)).collect()
}

/// Serve MCP over stdin/stdout until the client disconnects.
pub async fn serve_stdio(
    search: Arc<Search>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let running = Server::new(search).serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}

/// Serve the regular stdio tools over the selected execution client.
pub async fn serve_client(search: Client) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let running = Server::with_client(search).serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn explicit_full_fetch_returns_the_clean_page_without_a_character_cutoff(
    ) -> Result<(), Box<dyn std::error::Error>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/", listener.local_addr()?);
        let body = "Readable text. ".repeat(700);
        let expected = body.clone();
        let host = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await?;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
            Ok::<_, std::io::Error>(())
        });
        let mut config: crate::core::Config = serde_json::from_value(serde_json::json!({
            "engines": {"use": []}, "fetch": {"allow_private_networks": true}
        }))?;
        config.home = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join("search-full-fetch");
        let server = Server::new(Arc::new(Search::open(config)?));
        let answer = server
            .web_fetch(Parameters(FetchArgs {
                urls: vec![url],
                full: true,
                query: None,
                max_characters: None,
                offset: None,
                ..Default::default()
            }))
            .await?
            .structured_content
            .ok_or("missing content")?;
        assert_eq!(
            answer
                .pointer("/pages/0/text")
                .and_then(serde_json::Value::as_str),
            Some(expected.as_str())
        );
        host.await??;
        Ok(())
    }

    #[test]
    fn search_queries_are_bounded_and_trimmed_without_dropping_inputs() {
        assert_eq!(
            validate_queries(vec![" rust ".into(), "async".into()]).unwrap_or_default(),
            vec!["rust", "async"]
        );
        assert!(validate_queries(Vec::new()).is_err());
        assert!(validate_queries(vec!["rust".into(); 6]).is_err());
        assert!(validate_queries(vec!["rust".into(), " ".into()]).is_err());
        assert!(validate_queries(vec!["x".repeat(513)]).is_err());
    }

    /// Conflicting reading controls are invalid input, not a broken execution host.
    #[tokio::test]
    async fn reading_validation_returns_invalid_params() {
        let config: crate::core::Config = serde_json::from_value(serde_json::json!({
            "engines": {"use": []}
        }))
        .unwrap();
        let server = Server::new(Arc::new(Search::open(config).unwrap()));
        let error = server
            .web_fetch(Parameters(FetchArgs {
                urls: vec!["https://example.com/".into()],
                full: true,
                query: Some("example".into()),
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn tool_results_include_structured_content() {
        let structured = json_result(serde_json::json!({"ok": true}))
            .ok()
            .and_then(|result| result.structured_content);
        assert_eq!(structured, Some(serde_json::json!({"ok": true})));
    }

    /// Tool validation must use the selected engines, not a fixed name list,
    /// and an omitted limit must reach the engine as the host's configured maximum.
    #[tokio::test]
    async fn web_search_accepts_custom_engines_and_defaults_to_the_host_limit(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let config: crate::core::Config = serde_json::from_value(serde_json::json!({
            "engines": {"use": ["custom"], "config": {"custom": {
                "type": "command", "command": if cfg!(windows) { "python.exe" } else { "python3" }, "args": ["-c",
                    "import json,sys; r=json.load(sys.stdin); json.dump({'results':[{'title':r['query'],'url':'https://example.com/','snippet':str(r['limit'])}]},sys.stdout)"]
            }}}, "search": {"max_results": 3}
        }))?;
        let server = Server::new(Arc::new(Search::open(config)?));
        let answer = server
            .web_search(Parameters(SearchArgs {
                queries: vec!["custom query".into()],
                limit: None,
                engines: Some(vec!["custom".into()]),
                ..Default::default()
            }))
            .await?
            .structured_content
            .ok_or("missing structured content")?;
        assert_eq!(
            answer.pointer("/queries/0/results/0/title"),
            Some(&serde_json::json!("custom query"))
        );
        assert_eq!(
            answer.pointer("/queries/0/results/0/engines"),
            Some(&serde_json::json!(["custom"]))
        );
        assert_eq!(
            answer.pointer("/queries/0/results/0/snippet"),
            Some(&serde_json::json!("3"))
        );
        Ok(())
    }

    /// `web_research` returns one packet: the intent, the leads, and an honest
    /// coverage report even when every fetch fails.
    #[tokio::test]
    async fn web_research_returns_a_bounded_packet() -> Result<(), Box<dyn std::error::Error>> {
        let config: crate::core::Config = serde_json::from_value(serde_json::json!({
            "engines": {"use": ["custom"], "config": {"custom": {
                "type": "command", "command": if cfg!(windows) { "python.exe" } else { "python3" },
                "args": ["-c", "import json,sys; json.load(sys.stdin); json.dump({'results':[{'title':'T','url':'http://127.0.0.1:1/x','published':'2026-01-01'}]},sys.stdout)"]
            }}},
            "fetch": {"allow_private_networks": true}
        }))?;
        let server = Server::new(Arc::new(Search::open(config)?));
        let answer = server
            .web_research(
                Parameters(ResearchArgs {
                    queries: vec!["q".into()],
                    sources: None,
                    source_characters: None,
                    total_characters: None,
                    engines: None,
                    ..Default::default()
                }),
                Default::default(),
            )
            .await?
            .structured_content
            .ok_or("missing structured content")?;
        assert_eq!(answer.pointer("/queries/0"), Some(&serde_json::json!("q")));
        assert_eq!(
            answer.pointer("/coverage/candidates"),
            Some(&serde_json::json!(1))
        );
        assert_eq!(
            answer.pointer("/coverage/failed"),
            Some(&serde_json::json!(1))
        );
        Ok(())
    }
}
