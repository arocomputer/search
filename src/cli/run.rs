//! Dispatch and the serve loop.

use std::sync::Arc;

use crate::client::Client;
use crate::core::config;
use crate::core::Search;

use crate::cli::render::error as report_error;
use crate::cli::{args::Command, http, stdio};

/// Run one parsed command. Returns the process exit code.
pub async fn execute(command: Command) -> i32 {
    match command {
        Command::Engines {
            action,
            args,
            json,
            config,
        } => match crate::cli::engines::command(&action, args, json, config).await {
            Ok(()) => 0,
            Err(error) => report_error(error),
        },
        Command::Remote {
            action,
            args,
            config,
        } => match crate::cli::remote::command(&action, args, config).await {
            Ok(()) => 0,
            Err(error) => report_error(error),
        },
        Command::Version => {
            println!("{}", crate::VERSION);
            0
        }
        Command::Help => {
            println!("{}", crate::cli::args::usage());
            0
        }
        Command::Stdio { config } => stdio::serve(config).await,
        Command::Serve {
            config,
            address,
            hostname,
        } => http::serve(config, address, hostname).await,
        Command::Search {
            query,
            limit,
            json,
            engines,
            filters,
            config,
        } => {
            let filters = match crate::cli::args::query_filters(filters) {
                Ok(filters) => filters,
                Err(e) => return report_error(e),
            };
            let client = match build_client(config) {
                Ok(client) => client,
                Err(e) => return report_error(e),
            };
            match client
                .search(crate::core::Query {
                    text: query,
                    limit,
                    engines,
                    filters,
                })
                .await
            {
                Ok(answer) if json => crate::cli::render::json(&answer),
                Ok(answer) => crate::cli::render::search(&answer),
                Err(e) => report_error(e),
            }
        }
        Command::Fetch {
            urls,
            query,
            max_characters,
            offset,
            full,
            links,
            json,
            config,
        } => {
            let client = match build_client(config) {
                Ok(client) => client,
                Err(e) => return report_error(e),
            };
            match client
                .read(crate::client::ReadRequest {
                    urls,
                    query,
                    max_characters,
                    offset,
                    full,
                    links,
                })
                .await
            {
                Ok(results) => {
                    let failed = results.iter().any(|read| read.page.error.is_some());
                    let rendered = if json {
                        crate::cli::render::json(&results)
                    } else {
                        crate::cli::render::fetch(&results);
                        0
                    };
                    if failed {
                        1
                    } else {
                        rendered
                    }
                }
                Err(e) => report_error(e),
            }
        }
        Command::Research {
            queries,
            sources,
            source_characters,
            total_characters,
            max_per_host,
            max_attempts,
            timeout,
            session,
            filters,
            engines,
            synthesize,
            rerank,
            json,
            config,
        } => {
            let filters = match crate::cli::args::query_filters(filters) {
                Ok(filters) => filters,
                Err(e) => return report_error(e),
            };
            let client = match build_client(config) {
                Ok(client) => client,
                Err(e) => return report_error(e),
            };
            let request = crate::client::ResearchRequest {
                queries,
                options: crate::core::ResearchOptions {
                    sources,
                    source_characters,
                    total_characters,
                    max_per_host,
                    max_attempts,
                    timeout,
                    session,
                    filters,
                    engines,
                },
                synthesize,
                rerank,
            };
            match client.research(request).await {
                Ok(packet) if json => crate::cli::render::json(&packet),
                Ok(packet) => crate::cli::render::research(&packet),
                Err(e) => report_error(e),
            }
        }
        Command::Crawl {
            url,
            max_pages,
            max_depth,
            max_bytes,
            timeout,
            path_prefix,
            max_characters,
            total_characters,
            full,
            map,
            json,
            config,
        } => {
            let client = match build_client(config) {
                Ok(client) => client,
                Err(e) => return report_error(e),
            };
            let request = crate::core::CrawlRequest {
                url,
                options: crate::core::CrawlOptions {
                    max_pages,
                    max_depth,
                    max_bytes,
                    timeout,
                    path_prefix,
                    max_characters,
                    total_characters,
                    full,
                },
            };
            let result = if map {
                client.map(request).await
            } else {
                client.crawl(request).await
            };
            match result {
                Ok(report) => {
                    let failed = !report.failures.is_empty() || report.timed_out;
                    let rendered = if json {
                        crate::cli::render::json(&report)
                    } else {
                        crate::cli::render::crawl(&report)
                    };
                    if failed {
                        1
                    } else {
                        rendered
                    }
                }
                Err(e) => report_error(e),
            }
        }
    }
}

/// Select a remote before opening the local engine; selected failures are errors.
pub(crate) fn build_client(config_path: Option<String>) -> Result<Client, String> {
    if let Some(remote) = crate::cli::remote::selected(config_path.clone())? {
        return Ok(remote);
    }
    build_service(config_path, None).map(Client::Local)
}

/// Load configuration and build the service, applying serve overrides before
/// validating the effective settings.
pub(super) fn build_service(
    config_path: Option<String>,
    address: Option<String>,
) -> Result<Arc<Search>, String> {
    let path = config_path.map(std::path::PathBuf::from);
    let mut settings = config::load(path).map_err(|e| e.message().to_string())?;
    if let Some(address) = address {
        settings.address = address;
    }
    // Overrides bypassed validation at load time, so re-check the effective
    // settings before binding.
    settings.validate().map_err(|e| e.message().to_string())?;
    let service = Search::open(settings).map_err(|e| e.to_string())?;
    Ok(Arc::new(service))
}
