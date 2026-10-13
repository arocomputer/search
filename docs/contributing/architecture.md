# Architecture

Search is one root Cargo package, with library `search` (`src/lib.rs`) and
CLI binary `search` (`src/main.rs`). There is no workspace, facade, or separate
client/engine crate. Public APIs use the native modules below.

| Module | Contract / source |
| --- | --- |
| `search::core` | Protocol-free `Search`, `Config`, `Query`, `Answer`, `Page`, `Link`; `src/core/{config,engines,fetch,text,research,service}` |
| `search::core::research` | Deterministic evidence packets: `Packet`, `Evidence`, `Coverage`, `ResearchOptions`; `src/core/research.rs` |
| `search::core::engines` | `Engine`, `Pool`, `EngineState`, `EngineStatus`, `EngineError`, `Found`; `src/core/engines/` |
| `search::client` | `Client` for shared local/remote execution and pinned HTTPS; `src/client/mod.rs` |
| `search::mcp` | `Server`, tools and stdio transport in `src/mcp/mod.rs`; streamable HTTP in `src/mcp/http.rs` |
| `search::cli` | Commands and paired HTTPS host; `src/cli/{args,run,http,auth,remote,engines,..}` |

For navigation within the larger modules, see the
[configuration](../../src/core/config/README.md),
[fetch](../../src/core/fetch/README.md), [engines](../../src/core/engines/README.md),
and [CLI](../../src/cli/README.md) READMEs.

Engines are defined in settings: the Mwmbl and SearXNG presets are adapter
objects in `src/core/config/engines.rs`, and any other engine is an
`engines.config` entry. Root `build.rs` stamps release identity only. Integration
fixtures live under `tests/{cli,engines}`; the custom starter remains at
`docs/examples/json-post`.

## Features and dependency direction

Windows storage primitives live in the private, Windows-only `src/private_fs.rs`
module. Command scratch directories, CLI settings writes, and paired credentials
share its ACL and no-reparse checks; it is not a new public service or protocol
boundary. Unix paths retain their owner/permission and no-follow rules.

| Cargo features | Build |
| --- | --- |
| Default (`cli`) | Library and CLI binary; `cli` includes `mcp` and optional hosting/authentication dependencies |
| `--no-default-features` | Core (with engines) and client library without protocol or CLI dependencies |
| `--no-default-features --features mcp` | Library plus MCP only; `mcp` enables optional `rmcp` |

Core supplies discovery, engine execution, fetching and configuration; client
routes local/remote operations; MCP and CLI use these lower modules. Core,
including `core::engines`, must remain independent of client, MCP and CLI. The single package simplifies builds and
removes cross-crate plumbing, but compilation no longer enforces layer boundaries.
Module conventions and boundary tests enforce this direction instead. `./x check`
checks all features, minimal features, and MCP-only features as well as package
tests, formatting, linting, shell syntax and the security-surface guard.

Search is unreleased, so Rust APIs, settings shapes and the command protocol may
still change. Upgrading the binary never rewrites settings or credentials.

## Core

`search::core::Search` is the in-process service. Core exposes `Config`, `Query`,
`Link`, `Answer`, and `Page`. `search::core::engines` defines `Engine`,
`EngineState`, `EngineStatus`, and `EngineError`. `Pool` resolves the selected
engines from settings, runs them concurrently with per-engine and whole-query
deadlines, normalizes URLs and merges results using reciprocal-rank fusion. The
private `Adapter` implements the HTTP and command transports, skipping and
counting invalid result rows. `Pool` owns running adapters; HTTP engines reuse
one client. Engine-specific behavior belongs in settings and programs, never in
per-engine Rust code.

`search::core::research` assembles an evidence packet: it runs one or more
queries through `Pool`, fuses their leads with reciprocal-rank fusion, reads the
best distinct sources (per-host diversity, bounded by `ResearchOptions`), scores
passages against the intent with `core::text`, and returns dated, cited
`Evidence` with a `Coverage` report. It is deterministic and model-free;
optional synthesis belongs at the client edge, never the default. The packet is the machine's
research primitive: one call replaces the find-then-fetch-then-trim loop an agent
would otherwise hand-roll.

`search::core::crawl` owns robots-aware, same-origin collection traversal and
mapping. It reuses the guarded fetcher for pages, robots, and sitemap discovery;
there is no separate crawler network client or weakened SSRF exception.
Operator defaults/ceilings live in `core::config`; request validation lives in
`client`. Ephemeral research sessions and query/page caches are bounded and
expire in memory; they do not create a persistent corpus.

Optional processing lives in `client::processing`: explicitly configured
commands can reorder a packet or attach cited generated prose. Core never
executes a synthesizer or invokes a model. The host executes requested edge
processing for remote clients under the same output and cancellation bounds.

The fetcher returns `Page`: guarded retrieval, bounded bodies, clean extraction,
and optional passage selection. Its cache is transient and in memory. There is
no persistent corpus, indexing, or refresh. The `core::config` module owns
settings, defaults, engine presets and validation; credentials belong to adapters
and private trust storage. `Config` is inert data, without resolved transports.
Transport settings are `core::config::Adapter`, `core::config::Http`, and
`core::config::Command`.

User selection and attribution use `engines`; the CLI flag is `-engines`.
For removed settings and commands, see the
[migration guide](../configuration.md#migration).

## Routing and trust

CLI and stdio MCP use `search::client::Client` for exclusive local/remote execution.
A selected remote is checked before opening a local engine; errors never trigger
local fallback. Hosting always opens the local engine. Pairing verifies a copied
public certificate's fingerprint before sending the one-use code. HTTPS checks
the exact leaf pin plus standard WebPKI validation. Host trust stores credential
hashes; client trust stores secrets. CLI identity, pairing, and credential storage
live in `src/cli/auth.rs`; on-disk trust filenames are unchanged. See
[remote hosting](../remote.md).

Engines come only from the operator's settings file, never from request input
or implicit working-directory discovery. Settings are trusted configuration: a
command entry runs that program. `SEARCH_HOME` owns the default settings file
and private trust; engine management is local-only, and CLI settings writes are
locked, atomic, and owner-only. HTTP adapters reuse the SSRF resolver, refuse
redirects and proxies, and require independent permission for private networks.
Command adapters are trusted host code; only declared credentials are inherited,
and cancellation kills the direct child. See [engines](../engines.md).

## Invariants

- `search::core` (including `core::engines`) has no client or frontend dependency;
  MCP owns tools and transports. `scripts/guard.sh` checks the import direction.
  Minimal library builds do not compile MCP or CLI dependencies.
- `src/core/fetch/guard.rs` classifies infrastructure addresses as private and strips IPv6
  brackets before parsing. Preserve its counterexample tests and guard call sites.
- Root `src/lib.rs` denies explicit panic sites in production across its modules;
  allowed sites need a scoped explanation or `proof:` comment.
  `scripts/guard.sh` audits this surface.
- Mwmbl is the single keyless default. Each `EngineState` reports an `EngineStatus`.
- User-facing limits and enabled engines come from configuration.
