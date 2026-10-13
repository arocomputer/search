//! The command line: what `search` accepts and how it dispatches.
//!
//! `search QUERY` is the
//! human and script surface; `serve` is the HTTP and MCP surface; a bare
//! `search` with no query is the stdio MCP surface a harness spawns. Keeping
//! "no arguments" as MCP means an existing harness keeps working while a person
//! gets a real search command. Engine verbs edit and test the engines defined in
//! settings. A leading `-config PATH` applies to whatever command follows.

/// One parsed command.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// List, select, configure or explicitly test settings-defined engines.
    Engines {
        action: String,
        args: Vec<String>,
        json: bool,
        config: Option<String>,
    },
    /// Serve MCP over stdio (the default with no arguments, and what a harness
    /// invokes).
    Stdio { config: Option<String> },
    /// Serve the JSON API and MCP over HTTP.
    Serve {
        config: Option<String>,
        address: Option<String>,
        hostname: Option<String>,
    },
    /// Search the web, printing results for a person or JSON for a script.
    Search {
        query: String,
        filters: Vec<(String, String)>,
        /// Zero lets the selected service use its configured result maximum.
        limit: usize,
        json: bool,
        engines: Vec<String>,
        config: Option<String>,
    },
    /// Read one or more URLs.
    Fetch {
        urls: Vec<String>,
        query: Option<String>,
        max_characters: Option<usize>,
        /// Character to start reading at, from an earlier continuation.
        offset: usize,
        full: bool,
        links: bool,
        json: bool,
        config: Option<String>,
    },
    /// Research one or more intents into a bounded, cited evidence packet.
    Research {
        queries: Vec<String>,
        filters: Vec<(String, String)>,
        max_per_host: usize,
        max_attempts: usize,
        timeout: u64,
        session: Option<String>,
        synthesize: bool,
        rerank: bool,
        /// Zero uses the configured default, as `-limit` does.
        sources: usize,
        source_characters: usize,
        total_characters: usize,
        engines: Vec<String>,
        json: bool,
        config: Option<String>,
    },
    /// Crawl or map a bounded same-origin site.
    Crawl {
        url: String,
        max_pages: usize,
        max_depth: usize,
        max_bytes: u64,
        timeout: u64,
        path_prefix: Option<String>,
        max_characters: usize,
        total_characters: usize,
        full: bool,
        map: bool,
        json: bool,
        config: Option<String>,
    },
    /// Manage paired hosts or devices on this host.
    Remote {
        action: String,
        args: Vec<String>,
        config: Option<String>,
    },
    /// Print the version.
    Version,
    /// Print usage.
    Help,
}

/// Parse arguments. An unknown first token that is not a flag is treated as a
/// search query, so `search "rust async"` works; only a recognized verb takes a
/// subcommand's flags.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Command, String> {
    let mut args = args.into_iter().peekable();
    if matches!(
        args.peek().map(String::as_str),
        Some("-config" | "--config")
    ) {
        let _ = args.next();
        let path = args.next().ok_or("missing value for -config")?;
        let rest: Vec<String> = args.collect();
        // Bare `search -config PATH` keeps harness configs on stdio MCP.
        if rest.is_empty() {
            return Ok(Command::Stdio { config: Some(path) });
        }
        let mut command = parse(rest)?;
        if let Some(config) = command.config_mut() {
            if config.is_some() {
                return Err("-config given twice".into());
            }
            *config = Some(path);
        }
        return Ok(command);
    }
    match args.peek().map(String::as_str) {
        None => Ok(Command::Stdio { config: None }),
        Some("stdio") => {
            let _ = args.next();
            Ok(Command::Stdio {
                config: parse_config_flag(args)?,
            })
        }
        Some("serve") => {
            let _ = args.next();
            let flags = parse_serve_flags(args)?;
            Ok(Command::Serve {
                config: flags.config,
                address: flags.address,
                hostname: flags.hostname,
            })
        }
        Some("fetch" | "read") => {
            let _ = args.next();
            parse_fetch(args)
        }
        Some("crawl" | "map") => {
            let map = args.next().as_deref() == Some("map");
            parse_crawl(args, map)
        }
        Some("research") => {
            let _ = args.next();
            parse_research(args)
        }
        Some("engines") => {
            let _ = args.next();
            parse_engines(args)
        }
        Some("enable" | "disable" | "configure" | "test") => parse_engines(args),
        // Package commands were removed; say so instead of searching for the words.
        Some(verb @ ("install" | "update" | "remove")) => Err(format!(
            "search {verb} was removed: engines are defined in settings; see search engines \
             and docs/engines.md (use search -- {verb} ... to search for these words)"
        )),
        Some("remote") | Some("devices") | Some("revoke") | Some("pair-code") => {
            let verb = args.next().ok_or("missing command")?;
            let action = if verb == "remote" {
                args.next().ok_or("missing remote command")?
            } else {
                verb
            };
            let mut config = None;
            let mut values = Vec::new();
            while let Some(value) = args.next() {
                if value == "--config" || value == "-config" {
                    config = Some(args.next().ok_or("missing config path")?);
                } else {
                    values.push(value);
                }
            }
            Ok(Command::Remote {
                action,
                args: values,
                config,
            })
        }
        Some("version") | Some("--version") | Some("-version") => Ok(Command::Version),
        Some("help") | Some("--help") | Some("-h") => Ok(Command::Help),
        Some(flag) if flag.starts_with('-') && flag != "--" => parse_search(args),
        // Anything else is a search query.
        Some(_) => parse_search(args),
    }
}

/// Concise command reference; examples use primary verbs rather than flag aliases.
pub fn usage() -> &'static str {
    "search — web search and clean page reading

Start:
  search \"rust async\"            search with the keyless Mwmbl default
  search fetch https://example.com/  read a public page as clean text
  search help                    show this reference
  search version                 print the release version
  search -- test driven development  search a literal query starting with a command

Engines:
  search engines [list] [-json]   built-in and configured engines, selection and setup status
  search configure ID             show an engine's effective adapter and status
  search configure ID FIELD VALUE set one adapter field in engines.config.ID
  search enable ID                select a ready engine
  search disable ID               deselect an engine
  search test ID QUERY [-json]    run one engine explicitly and print its results
  Built-in: mwmbl (keyless default), searxng (needs url). Any other ID is a custom
  http or command adapter in engines.config. Engine commands always run locally.
  Settings are trusted: a command engine runs that program as you. Reference
  credentials by environment variable name (env/header_env), never by value.

Hosting:
  search serve [flags]            host the paired HTTPS JSON API and MCP endpoint
  search stdio [-config PATH]     serve MCP over stdio (also the no-argument default)
  search remote pair NAME HTTPS_URL CERT_FILE SHA256  pair; code read from stdin
  search remote use NAME          route CLI and stdio MCP to a paired host
  search remote list              inspect saved hosts
  search remote off               return to local execution
  search remote remove NAME       forget a saved host
  search pair-code               renew this host's one-use pairing code
  search devices                 list this host's paired devices
  search revoke DEVICE_ID         revoke a device on this host

Serve flags:
  -address HOST:PORT              listener (default 127.0.0.1:8642)
  -hostname NAME                  certificate name; required for wildcard binds

Search flags:
  -limit N                       result count (default from settings, normally 10)
  -engines A,B                    restrict to selected engine IDs
  -json                          print JSON instead of text

Fetch flags:
  -query TEXT                    relevant passages; fall back to text if none match
  -max-chars N                    per-page character window (default from settings)
  -offset N                       continue reading at character N
  -json                          print JSON instead of text
  -full                          explicitly read the full clean page
  -links                         include discovered public links
  read is an alias for fetch; default windows use research.read_characters.

Directional filters (search and research):
  -freshness TEXT  -start-date YYYY-MM-DD  -end-date YYYY-MM-DD
  -include-domains A,B  -exclude-domains A,B  -language TEXT  -region TEXT
  -category TEXT  -safe true|false  -exact-phrase TEXT

Research flags:
  search research QUERY [-query MORE]...
                                  search, read, and return a bounded, cited
                                  packet of dated passages in one call
  -sources N                      distinct sources to read (default 5, maximum 10)
  -chars N                        passage characters per source (default 4000)
  -total-chars N                  passage characters in the packet (default 12000)
  -engines A,B                    restrict to selected engine IDs
  -json                           print JSON instead of text

  -max-per-host N  -max-attempts N  -timeout MS
  -session ID                     skip useful evidence already seen in this session
  -synthesize  -rerank             opt in to configured processing commands

Crawl/map flags:
  search crawl URL | search map URL
  -max-pages N  -max-depth N  -max-bytes N  -timeout MS  -path-prefix PATH
  -max-chars N  -total-chars N      bounded page/collection text windows
  -full                           explicitly return full crawled page text
  -json                           print the bounded report as JSON

Settings:
  -config PATH                    settings file; before or after the command/query
  Default: $CONFIG or $SEARCH_HOME/settings.json (SEARCH_HOME defaults to ~/.search).
  Double-dash flag forms are accepted. Use -- before literal query/value arguments.
  Guide: https://github.com/fschrhunt/search/tree/main/docs"
}

/// Parse engine commands, extracting flags before literal `--` arguments.
/// A bare `search engines` lists engines.
fn parse_engines<I: IntoIterator<Item = String>>(args: I) -> Result<Command, String> {
    let mut args = args.into_iter().peekable();
    let action = match args.peek().map(String::as_str) {
        None => "list".to_owned(),
        Some(flag) if flag.starts_with('-') => "list".to_owned(),
        Some(_) => args.next().ok_or("expected an engines command")?,
    };
    let mut config = None;
    let mut json = false;
    let mut values = Vec::new();
    let mut literal = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--" if !literal => literal = true,
            "-config" | "--config" if !literal => {
                config = Some(args.next().ok_or("missing value for -config")?);
            }
            "-json" | "--json" if !literal && matches!(action.as_str(), "list" | "test") => {
                json = true
            }
            flag if !literal && flag.starts_with('-') => {
                return Err(format!("unknown engine flag {flag:?}"));
            }
            _ => values.push(arg),
        }
    }
    let valid = match action.as_str() {
        "list" => values.is_empty(),
        "enable" | "disable" => values.len() == 1,
        "configure" => values.len() == 1 || values.len() == 3,
        "test" => values.len() >= 2 && !values.iter().skip(1).all(|s| s.trim().is_empty()),
        _ => false,
    };
    if !valid {
        return Err("invalid engines command or arguments; see search help".into());
    }
    Ok(Command::Engines {
        action,
        args: values,
        json,
        config,
    })
}

impl Command {
    /// The settings-path slot of commands that read settings.
    fn config_mut(&mut self) -> Option<&mut Option<String>> {
        match self {
            Command::Engines { config, .. }
            | Command::Stdio { config }
            | Command::Serve { config, .. }
            | Command::Search { config, .. }
            | Command::Fetch { config, .. }
            | Command::Research { config, .. }
            | Command::Crawl { config, .. }
            | Command::Remote { config, .. } => Some(config),
            Command::Version | Command::Help => None,
        }
    }
}

fn parse_config_flag<I: IntoIterator<Item = String>>(args: I) -> Result<Option<String>, String> {
    let mut config = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-config" | "--config" => {
                config = Some(args.next().ok_or("missing value for -config")?);
            }
            other => return Err(format!("unknown flag {other:?}")),
        }
    }
    Ok(config)
}

/// Parse a query, engine selection and directional filters without resolving host defaults.
fn parse_search<I: IntoIterator<Item = String>>(args: I) -> Result<Command, String> {
    let mut query_words = Vec::new();
    let mut limit = 0usize;
    let mut json = false;
    let mut engines = Vec::new();
    let mut filters = Vec::new();
    let mut config = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--" => {
                query_words.extend(args);
                break;
            }
            "-limit" | "--limit" => {
                limit = args
                    .next()
                    .ok_or("missing value for -limit")?
                    .parse()
                    .map_err(|_| "-limit needs a number".to_string())?;
            }
            "-json" | "--json" => json = true,
            "-engines" | "--engines" => {
                let value = args.next().ok_or("missing value for -engines")?;
                engines = value
                    .split(',')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect();
            }
            "-config" | "--config" => {
                config = Some(args.next().ok_or("missing value for -config")?)
            }
            flag if is_filter_flag(flag) => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("missing value for {flag}"))?;
                filters.push((flag.trim_start_matches('-').to_string(), value));
            }
            other if other.starts_with('-') => return Err(format!("unknown flag {other:?}")),
            word => query_words.push(word.to_string()),
        }
    }
    if query_words.is_empty() {
        return Err("a search needs a query".into());
    }
    Ok(Command::Search {
        query: query_words.join(" "),
        limit,
        json,
        engines,
        filters,
        config,
    })
}

/// Parse bounded reading and explicit full/link opt-ins.
fn parse_fetch<I: IntoIterator<Item = String>>(args: I) -> Result<Command, String> {
    let mut urls = Vec::new();
    let mut query = None;
    let mut max_characters = None;
    let mut offset = 0;
    let mut full = false;
    let mut links = false;
    let mut json = false;
    let mut config = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-query" | "--query" => query = Some(args.next().ok_or("missing value for -query")?),
            "-max-chars" | "--max-chars" => {
                max_characters = Some(
                    args.next()
                        .ok_or("missing value for -max-chars")?
                        .parse()
                        .map_err(|_| "-max-chars needs a number".to_string())?,
                );
            }
            "-offset" | "--offset" => {
                offset = args
                    .next()
                    .ok_or("missing value for -offset")?
                    .parse()
                    .map_err(|_| "-offset needs a number".to_string())?;
            }
            "-full" | "--full" => full = true,
            "-links" | "--links" => links = true,
            "--" => {
                urls.extend(args);
                break;
            }
            "-json" | "--json" => json = true,
            "-config" | "--config" => {
                config = Some(args.next().ok_or("missing value for -config")?)
            }
            other if other.starts_with('-') => return Err(format!("unknown flag {other:?}")),
            url => urls.push(url.to_string()),
        }
    }
    if urls.is_empty() {
        return Err("fetch needs at least one URL".into());
    }
    Ok(Command::Fetch {
        urls,
        query,
        max_characters,
        offset,
        full,
        links,
        json,
        config,
    })
}

/// Parse research intents, bounds, session and optional processing controls.
fn parse_research<I: IntoIterator<Item = String>>(args: I) -> Result<Command, String> {
    let mut words = Vec::new();
    let mut queries = Vec::new();
    let mut sources = 0usize;
    let mut source_characters = 0usize;
    let mut total_characters = 0usize;
    let mut engines = Vec::new();
    let mut filters = Vec::new();
    let mut json = false;
    let mut config = None;
    let mut max_per_host = 0;
    let mut max_attempts = 0;
    let mut timeout = 0;
    let mut session = None;
    let mut synthesize = false;
    let mut rerank = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--" => {
                words.extend(args);
                break;
            }
            "-query" | "--query" => queries.push(args.next().ok_or("missing value for -query")?),
            "-sources" | "--sources" => {
                sources = args
                    .next()
                    .ok_or("missing value for -sources")?
                    .parse()
                    .map_err(|_| "-sources needs a number".to_string())?;
            }
            "-chars" | "--chars" => {
                source_characters = args
                    .next()
                    .ok_or("missing value for -chars")?
                    .parse()
                    .map_err(|_| "-chars needs a number".to_string())?;
            }
            "-total-chars" | "--total-chars" => {
                total_characters = args
                    .next()
                    .ok_or("missing value for -total-chars")?
                    .parse()
                    .map_err(|_| "-total-chars needs a number".to_string())?;
            }
            "-engines" | "--engines" => {
                let value = args.next().ok_or("missing value for -engines")?;
                engines = value
                    .split(',')
                    .map(|part| part.trim().to_string())
                    .filter(|part| !part.is_empty())
                    .collect();
            }
            "-max-per-host" | "--max-per-host" => max_per_host = number(&mut args, "max-per-host")?,
            "-max-attempts" | "--max-attempts" => max_attempts = number(&mut args, "max-attempts")?,
            "-timeout" | "--timeout" => timeout = number(&mut args, "timeout")?,
            "-session" | "--session" => {
                session = Some(args.next().ok_or("missing value for -session")?)
            }
            "-synthesize" | "--synthesize" => synthesize = true,
            "-rerank" | "--rerank" => rerank = true,
            "-json" | "--json" => json = true,
            "-config" | "--config" => {
                config = Some(args.next().ok_or("missing value for -config")?)
            }
            flag if is_filter_flag(flag) => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("missing value for {flag}"))?;
                filters.push((flag.trim_start_matches('-').to_string(), value));
            }
            other if other.starts_with('-') => return Err(format!("unknown flag {other:?}")),
            word => words.push(word.to_string()),
        }
    }
    // Positional words are the first intent; `-query` adds more.
    if !words.is_empty() {
        queries.insert(0, words.join(" "));
    }
    if queries.is_empty() {
        return Err("research needs a query".into());
    }
    Ok(Command::Research {
        queries,
        sources,
        source_characters,
        total_characters,
        engines,
        filters,
        max_per_host,
        max_attempts,
        timeout,
        session,
        synthesize,
        rerank,
        json,
        config,
    })
}

/// Read a numeric flag value with a useful error.
fn number<T: std::str::FromStr>(
    args: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<T, String> {
    args.next()
        .ok_or_else(|| format!("missing value for -{flag}"))?
        .parse()
        .map_err(|_| format!("-{flag} needs a number"))
}

/// Recognize directional filters shared by search and research.
fn is_filter_flag(flag: &str) -> bool {
    flag.starts_with('-')
        && matches!(
            flag.trim_start_matches('-'),
            "freshness"
                | "start-date"
                | "end-date"
                | "include-domains"
                | "exclude-domains"
                | "language"
                | "region"
                | "category"
                | "safe"
                | "exact-phrase"
        )
}

/// Translate CLI spelling into the core filter shape; Client validates values.
pub(super) fn query_filters(
    flags: Vec<(String, String)>,
) -> Result<crate::core::QueryFilters, String> {
    let mut values = serde_json::Map::new();
    for (name, value) in flags {
        let key = name.replace('-', "_");
        let value = match name.as_str() {
            "include-domains" | "exclude-domains" => serde_json::json!(value
                .split(',')
                .map(str::trim)
                .map(str::to_string)
                .collect::<Vec<_>>()),
            "safe" => serde_json::json!(value
                .parse::<bool>()
                .map_err(|_| "-safe needs true or false")?),
            _ => serde_json::json!(value),
        };
        values.insert(key, value);
    }
    serde_json::from_value(serde_json::Value::Object(values)).map_err(|e| e.to_string())
}

/// Parse bounded site inventory flags; zero numeric bounds defer to settings.
fn parse_crawl(args: impl IntoIterator<Item = String>, map: bool) -> Result<Command, String> {
    let mut args = args.into_iter();
    let (mut url, mut path_prefix, mut config) = (None, None, None);
    let (mut max_pages, mut max_depth, mut max_bytes, mut timeout) = (0, 0, 0, 0);
    let mut json = false;
    let (mut max_characters, mut total_characters, mut full) = (0, 0, false);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-max-pages" | "--max-pages" => max_pages = number(&mut args, "max-pages")?,
            "-max-depth" | "--max-depth" => max_depth = number(&mut args, "max-depth")?,
            "-max-bytes" | "--max-bytes" => max_bytes = number(&mut args, "max-bytes")?,
            "-max-chars" | "--max-chars" => max_characters = number(&mut args, "max-chars")?,
            "-total-chars" | "--total-chars" => {
                total_characters = number(&mut args, "total-chars")?
            }
            "-full" | "--full" => full = true,
            "-timeout" | "--timeout" => timeout = number(&mut args, "timeout")?,
            "-path-prefix" | "--path-prefix" => {
                path_prefix = Some(args.next().ok_or("missing value for -path-prefix")?)
            }
            "-config" | "--config" => {
                config = Some(args.next().ok_or("missing value for -config")?)
            }
            "-json" | "--json" => json = true,
            flag if flag.starts_with('-') => return Err(format!("unknown flag {flag:?}")),
            _ if url.is_none() => url = Some(arg),
            _ => return Err("crawl/map accepts one root URL".into()),
        }
    }
    Ok(Command::Crawl {
        url: url.ok_or("crawl/map needs a root URL")?,
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
    })
}

fn parse_serve_flags<I: IntoIterator<Item = String>>(args: I) -> Result<ServeFlags, String> {
    let mut flags = ServeFlags::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-config" | "--config" => {
                flags.config = Some(args.next().ok_or("missing value for -config")?)
            }
            "-address" | "--address" => {
                flags.address = Some(args.next().ok_or("missing value for -address")?)
            }
            "-hostname" | "--hostname" => {
                flags.hostname = Some(args.next().ok_or("missing value for -hostname")?)
            }
            other => return Err(format!("unknown flag {other:?}")),
        }
    }
    Ok(flags)
}

/// The serve-only flags, named so the parse signature stays readable.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ServeFlags {
    pub config: Option<String>,
    pub address: Option<String>,
    pub hostname: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn engine_commands_extract_flags_and_preserve_literal_query() {
        assert_eq!(
            parse(args(&[
                "engines",
                "test",
                "fixture",
                "--config",
                "x.json",
                "-json",
                "--",
                "-site:example.com",
                "rust"
            ]))
            .unwrap(),
            Command::Engines {
                action: "test".into(),
                args: vec!["fixture".into(), "-site:example.com".into(), "rust".into()],
                json: true,
                config: Some("x.json".into())
            }
        );
        for values in [
            vec!["engines", "test", "fixture"],
            vec!["engines", "list", "fixture"],
            vec!["engines", "configure", "fixture", "url"],
            vec!["engines", "enable", "fixture", "-json"],
            vec!["engines", "install", "fixture"],
            vec!["engines", "list", "--home", "/tmp"],
        ] {
            assert!(parse(args(&values)).is_err(), "{values:?}");
        }
        for listing in [vec!["engines"], vec!["engines", "-json"]] {
            assert!(matches!(
                parse(args(&listing)).unwrap(),
                Command::Engines { action, .. } if action == "list"
            ));
        }
    }

    #[test]
    fn top_level_engine_commands_route_like_their_aliases() {
        for values in [
            vec!["enable", "fixture"],
            vec!["disable", "fixture"],
            vec!["configure", "fixture", "url", "https://example.com"],
            vec!["test", "fixture", "--", "-config", "literal"],
        ] {
            let command = parse(args(&values)).unwrap();
            assert!(matches!(&command, Command::Engines { action, .. } if action == values[0]));
            let mut alias = vec!["engines"];
            alias.extend(&values);
            assert_eq!(command, parse(args(&alias)).unwrap());
        }
    }

    /// Removed package commands explain themselves instead of becoming searches,
    /// and `--` still searches for the words.
    #[test]
    fn removed_package_commands_are_errors() {
        for verb in ["install", "update", "remove"] {
            let error = parse(args(&[verb, "searxng"])).unwrap_err();
            assert!(error.contains("removed"), "{error}");
        }
        assert!(matches!(
            parse(args(&["--", "install", "rust"])).unwrap(),
            Command::Search { query, .. } if query == "install rust"
        ));
    }

    /// A leading `-config PATH` applies to the command after it.
    #[test]
    fn leading_config_applies_to_the_following_command() {
        assert_eq!(
            parse(args(&["-config", "s.json", "fetch", "https://a.example"])).unwrap(),
            Command::Fetch {
                urls: vec!["https://a.example".into()],
                query: None,
                max_characters: None,
                offset: 0,
                full: false,
                links: false,
                json: false,
                config: Some("s.json".into()),
            }
        );
        assert!(matches!(
            parse(args(&["--config", "s.json", "rust", "async"])).unwrap(),
            Command::Search { query, config: Some(path), .. } if query == "rust async" && path == "s.json"
        ));
        assert!(parse(args(&["-config", "a.json", "engines", "-config", "b.json"])).is_err());
    }

    #[test]
    fn literal_search_queries_can_contain_command_names_and_flags() {
        assert_eq!(
            parse(args(&["--", "test", "-engines", "literal"])).unwrap(),
            Command::Search {
                query: "test -engines literal".into(),
                limit: 0,
                json: false,
                engines: vec![],
                filters: vec![],
                config: None,
            }
        );
    }

    #[test]
    fn search_selection_uses_engine_flags() {
        assert!(matches!(
            parse(args(&["rust", "-engines", "mwmbl, custom"])).unwrap(),
            Command::Search { engines, .. } if engines == vec!["mwmbl", "custom"]
        ));
        assert!(parse(args(&["rust", "-providers", "mwmbl"])).is_err());
    }

    #[test]
    fn no_arguments_means_stdio() {
        assert_eq!(parse(args(&[])).unwrap(), Command::Stdio { config: None });
    }

    #[test]
    fn a_query_is_a_search() {
        assert_eq!(
            parse(args(&["rust", "async", "-limit", "5"])).unwrap(),
            Command::Search {
                query: "rust async".into(),
                limit: 5,
                json: false,
                engines: vec![],
                filters: vec![],
                config: None,
            }
        );
    }

    #[test]
    fn search_limits_default_to_settings_and_preserve_explicit_values() {
        assert!(matches!(
            parse(args(&["rust"])).unwrap(),
            Command::Search { limit: 0, .. }
        ));
        for limit in ["0", "1", "75"] {
            assert!(matches!(
                parse(args(&["rust", "-limit", limit])).unwrap(),
                Command::Search { limit: parsed, .. } if parsed == limit.parse::<usize>().unwrap()
            ));
        }
    }

    #[test]
    fn serve_reads_its_flags() {
        assert_eq!(
            parse(args(&["serve", "-address", "0.0.0.0:1"])).unwrap(),
            Command::Serve {
                config: None,
                address: Some("0.0.0.0:1".into()),
                hostname: None,
            }
        );
    }

    #[test]
    fn serve_accepts_both_hostname_flags_and_requires_a_value() {
        for flag in ["-hostname", "--hostname"] {
            assert!(
                matches!(parse(args(&["serve", flag, "search.example"])).unwrap(),
                Command::Serve { hostname: Some(name), .. } if name == "search.example")
            );
            assert!(parse(args(&["serve", flag])).is_err());
        }
    }

    #[test]
    fn fetch_takes_urls_and_a_focus() {
        assert_eq!(
            parse(args(&["fetch", "https://a.example", "-query", "x y"])).unwrap(),
            Command::Fetch {
                urls: vec!["https://a.example".into()],
                query: Some("x y".into()),
                max_characters: None,
                offset: 0,
                full: false,
                links: false,
                json: false,
                config: None,
            }
        );
    }

    #[test]
    fn a_lone_flag_defaults_to_stdio() {
        assert_eq!(
            parse(args(&["-config", "/etc/settings.json"])).unwrap(),
            Command::Stdio {
                config: Some("/etc/settings.json".into())
            }
        );
    }

    #[test]
    fn a_search_without_words_is_an_error() {
        // A recognized verb with flags but no query is a mistake.
        assert!(parse(args(&["fetch"])).is_err());
        // A bare query word is a search.
        assert!(matches!(
            parse(args(&["hello"])).unwrap(),
            Command::Search { .. }
        ));
    }

    #[test]
    fn fetch_without_urls_is_an_error() {
        assert!(parse(args(&["fetch"])).is_err());
    }

    #[test]
    fn research_takes_a_query_additional_intents_and_bounds() {
        assert_eq!(
            parse(args(&[
                "research",
                "rust",
                "ownership",
                "-query",
                "borrow checker",
                "-sources",
                "3",
                "-chars",
                "500",
                "-json",
            ]))
            .unwrap(),
            Command::Research {
                queries: vec!["rust ownership".into(), "borrow checker".into()],
                max_per_host: 0,
                max_attempts: 0,
                timeout: 0,
                session: None,
                synthesize: false,
                rerank: false,
                sources: 3,
                source_characters: 500,
                total_characters: 0,
                engines: vec![],
                filters: vec![],
                json: true,
                config: None,
            }
        );
        assert!(parse(args(&["research"])).is_err());
        assert!(parse(args(&["research", "-sources", "not-a-number"])).is_err());
    }
}
