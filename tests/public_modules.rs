//! Pin the native public module paths for minimal, MCP and CLI builds.

use search::client::{Client, ReadRequest, Remote, ResearchRequest};
use search::core::config::{Adapter, EngineSettings, Preset};
use search::core::engines::{Engine, EngineError, EngineState, EngineStatus, Found, Pool};
use search::core::research::{Coverage, Evidence, Packet, ResearchOptions};
use search::core::{
    Answer, Config, CrawlOptions, CrawlReport, CrawlRequest, Link, Page, Query, QueryFilters,
    Search,
};

/// Consumers can name the service, engines, settings and execution contract in one package.
#[test]
fn public_module_contract_compiles() {
    let _ = std::mem::size_of::<(Search, Config, Query, Answer, Page, Link)>();
    let _ = std::mem::size_of::<(Client, Remote, Pool, EngineState, EngineStatus)>();
    let _ = std::mem::size_of::<(EngineError, Found, EngineSettings, Adapter)>();
    let _ = std::mem::size_of::<(Packet, Evidence, Coverage, ResearchOptions, ResearchRequest)>();
    let _ = std::mem::size_of::<(
        CrawlOptions,
        CrawlRequest,
        CrawlReport,
        QueryFilters,
        ReadRequest,
    )>();
    let _: Option<&dyn Engine> = None;
    let _ = Preset::find;
    #[cfg(feature = "mcp")]
    let _ = std::mem::size_of::<search::mcp::Server>();
    #[cfg(feature = "cli")]
    let _ = search::cli::run::execute;
}
