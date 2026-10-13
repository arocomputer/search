# Working on search

Instructions for an agent editing this repo.

## Build and check

```sh
./x build          # fast dev build
./x test           # the whole behavioral contract
./x check          # format, lint, tests, shell syntax, and the security-surface guard
./x shell          # shell-script syntax alone
./x guard          # the security-surface audit alone
./x serve          # the JSON API and MCP over HTTP (dev build)
./x stdio          # MCP over stdio (what an agent spawns)
```

`./x check` is not optional. It runs formatting and linting, Cargo checks for
all features, minimal features, and MCP-only features, package tests, a syntax
check of every shell script, and `scripts/guard.sh`.

## Where things live

Search is one root Cargo package, with library `search` and CLI binary `search`.
`search::core` provides protocol-free live discovery and clean fetch;
`search::core::engines` runs the engines defined in settings, without per-engine
Rust code. CLI and MCP share local/remote execution through
`search::client::Client`; MCP owns only tools and transports. Module conventions,
boundary tests and `scripts/guard.sh` preserve dependency direction; separate
crate compilation no longer enforces it.

```
Cargo.toml     one package; default feature cli includes mcp
build.rs       stamps release identity
src/lib.rs     public core, client, mcp and cli modules; production panic deny
src/main.rs    CLI binary entry point (requires cli)
src/private_fs.rs  Windows owner-only ACLs and no-reparse handles for scratch, settings and trust
src/core/      protocol-free engine (`search::core::Search`)
  config/      inert settings, defaults, loading and validation; Adapter/Http/Command
    engines.rs presets (mwmbl, searxng) and resolving an engine ID to an adapter
  engines/     Engine, Pool, EngineState; HTTP/command adapters, scratch dirs
    README.md  engine module contracts
  fetch/       guarded requests, redirects, byte bound, content types and charsets; Page
    guard.rs   the SSRF guard and resolver — the security-critical file
    extract/   main content (Markdown), visibility filtering, PDF text, image guard
    cache.rs   transient in-memory pages
  text.rs      focus: query passages and continuable character windows
  crawl.rs     bounded robots-aware collection traversal and mapping
  research.rs  deterministic evidence packets: Packet, Evidence, Coverage
  service.rs   the in-process Search service; Query, Answer, Link
src/client/mod.rs  shared local/remote execution, input bounds and pinned HTTPS
src/client/processing.rs explicit bounded packet reranking and synthesis
src/mcp/       mod.rs tools and stdio transport; http.rs streamable HTTP transport
src/cli/       CLI and paired HTTPS host
  args.rs      command parsing
  run.rs       dispatch and serve overrides
  http.rs      JSON API and device authentication
  auth.rs      persistent TLS identity, pairing and private credential storage
  remote.rs    profile selection and device management
  engines.rs   engine list/enable/disable/configure/test; locked settings writes
tests/        integration tests and fixtures under cli/ and engines/
docs/examples/json-post/  custom command engine starter and its settings entry
scripts/guard.sh  security-surface audit: hosts, panic sites, SSRF and auth
scripts/release.sh · scripts/formula.sh · install.sh · install.ps1  release tooling
scripts/test-install.ps1  offline Windows installer contract
docs/          user pages and docs/contributing/ for contributors
x              repository entry point
```

`--no-default-features` builds core (with engines) and client without protocol
or CLI dependencies. `--no-default-features --features mcp` adds MCP only. The
default `cli` feature includes `mcp` and optional hosting/authentication
dependencies; `mcp` enables optional `rmcp`.

## Conventions

- **Shipped code denies explicit panic sites.** `src/lib.rs` denies
  `clippy::unwrap_used`, `expect_used`, `panic`, `unreachable`, and
  `indexing_slicing`. Every allowed site carries a `proof:` comment or a scoped
  `#[allow]` explaining why runtime input cannot reach it. `scripts/guard.sh`
  enforces the same rule.
- **The core is protocol-free.** `search::core::Search` is the public in-process
  engine. MCP, CLI and client depend on core; core (including `core::engines`)
  must not depend on any of them. `scripts/guard.sh` enforces the direction.
- **The security-critical file is `src/core/fetch/guard.rs`.** A URL is model-chosen, so
  every class of address that can reach infrastructure must be classified
  private, and `check_host` strips IPv6 brackets before parsing an address. The
  guard's tests carry the counterexamples; do not loosen them.
- **Engines live in settings.** `engines.use` selects IDs (default `mwmbl`, the
  single keyless default); `engines.config.ID` defines each one. The `mwmbl` and
  `searxng` presets are adapter objects in `src/core/config/engines.rs`, merged
  shallowly with their entry and never changing `type`; any other ID is a
  complete `http` or `command` adapter. No per-engine Rust code. HTTP adapters
  read credential headers from configured environment variables. Command
  adapters are trusted host code, not a sandbox; only declared environment
  credentials are passed through. An invalid result row is skipped and counted,
  not fatal. An engine's `EngineState` reports its `EngineStatus`, so an empty
  answer is never mistaken for a broken one. Binary upgrades must not rewrite
  settings.
- **Settings are trusted operator configuration.** They can define executables,
  so they are not shareable data: never load settings or engines implicitly from
  the working directory or from request input. `SEARCH_HOME` selects the root,
  defaulting to `~/.search`. Engine management is local-only; CLI settings writes
  are locked, atomic and owner-only. Trust stays under home/trust.
- **Don't hardcode what a user might change.** Timeouts, result counts, the user
  agent, and the enabled engines are read from configuration with built-in
  defaults. A new user-facing behaviour becomes a config field, not a constant.
- **Keep the vocabulary consistent.** `search::core` exposes `Search`, `Config`,
  `Query`, `Answer`, `Page`, and `Link`; `search::core::research` exposes
  `Packet`, `Evidence`, `Coverage`, and `ResearchOptions`. `search::core::engines` exposes `Engine`,
  `Pool`, `EngineState`, `EngineStatus`, `EngineError`, and `Found`. Fetch returns
  `Page`; user fields and CLI selection use `engines` and `-engines`; an engine's
  `engines.config` entry is its adapter. Use native module imports, without old
  facade aliases or separate client/engine crate names.
- **Separate settings from execution.** `Config` is inert data, without resolved
  transports. Transport settings are `core::config::Adapter`,
  `core::config::Http`, and `core::config::Command`; `EngineSettings::adapter`
  resolves an ID. `Pool` owns running adapters; HTTP engines reuse one client. CLI authentication lives in
  `src/cli/auth.rs`; on-disk trust filenames stay unchanged.
- **No persistent corpus.** Search and fetch use live engines and a transient
  fetch cache. Old corpus settings are rejected; existing files/data are untouched.
- **Never truncate content silently.** `fetch.max_response_bytes` bounds work and
  memory, not what a reader gets: extraction never cuts text, a body cut at the
  byte bound reports `truncated`, and a character window reports `next_offset`.
  Readers choose how much to read through `core::text::focus`; CLI and MCP must
  not reimplement it.
- **Research assembles; it never synthesizes.** `core::research` runs a
  deterministic, model-free pipeline — fan out, fuse with RRF, read the best
  distinct sources, score passages with `core::text` — and returns a bounded
  packet with a coverage report. No model call belongs in core; a synthesizer is
  an optional trusted command at the client edge, never a default. Every bound is a config field, and a
  packet never exceeds its `ResearchOptions` ceilings.
- **Crawling keeps the fetch trust boundary.** Every page, robots, sitemap and
  redirect goes through the guarded fetcher. Stay same-origin/path-scoped,
  respect robots, and bound pages, depth, bytes, delay and the whole-call time.
- **Quality claims need retrieval evidence.** Offline labeled evaluations pin
  useful-source recall and context contracts; they are not proof of live-index
  quality or competitive parity. `./x eval` scores labeled saved/live packets.
- **Extraction must be precise before it is aggressive.** A false removal loses
  the article; match class tokens and segments and parse styles, never substrings.
  Image markup must not survive in any output (prose, code, nested links, reference
  definitions). Pin each behavior with a counterexample from a real page.
- **Validate inputs once.** Query, limit and URL bounds live in `search::client`
  (`validate_query`, `validate_search`, `validate_urls`). HTTP, MCP and `Client`
  call them; do not repeat the numbers elsewhere. The paired host exposes one REST
  API, which remote `Client`s also use.
- **Comment modules and functions with their purpose and contract.** Avoid
  line-by-line comments. Update a comment when the behavior it describes changes.
- **One concern per PR.** The branch is `<type>/<slug>`; the title is
  conventional (`fix(search): ...`), because it becomes the squash commit on main.
- Anything user-visible gets a `CHANGELOG.md` entry under `Unreleased`.
