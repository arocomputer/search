# Changelog

## Unreleased

- Update installation, Homebrew, documentation, and repository contact URLs for
  the move to `fschrhunt/search`; existing settings and protocols stay compatible.

## v0.1.0 · 2026-10-16

- Refuse only the `3fff::/20` IPv6 documentation range (RFC 9637) instead of the
  whole `3ff0::/12`, so public addresses just outside it are reachable again.
- Review hardening: processors share bounded admission and one deadline within
  the research budget, report end-to-end latency, and use command-engine working
  directory defaults. Invalid fetch concurrency is rejected before startup.
- Preserve distinct URL paths/query bytes, absent HTTP filter defaults, and
  final-host diversity through redirects. Duplicate research intents no longer
  multiply engine work or ranking votes; blank downloads are not useful evidence.
- Correct evaluation alias recall, destination-host diversity, opaque URL
  identity, blank evidence ranks, and overlapping within-passage span coverage.
- Keep paired MCP admission consistent across disconnects and transport expiry;
  terminal output now reports continuation and independent truncation limits,
  and invalid MCP requests return invalid-parameter errors.
- Harden robots policy scope, encoded path separators, BOM handling, and host
  politeness across errors/cancellation. Abort queued parsing on cancellation;
  complete page reads now share one deadline across admission and redirects.
- Neutralize multiline image/reference markup and unsafe metadata, and redact
  URL-bearing body-read diagnostics. Keep completed pages on discovery timeout.

- **Machine-first retrieval.** Directional queries carry domain/date/language/
  region/category/safety/phrase filters through settings-defined adapters, with
  explicit unsupported-filter reports. Results retain multiple snippets and
  update dates. Query caching is transient and bounded; HTTP engines respect
  bounded retry delays within their deadlines.
- **Auditable research.** Packets preserve extracted publication dates, final
  URLs, content hashes, incomplete-download flags, character ranges and
  continuation offsets. Whole-call deadlines and bounded replacement attempts
  retain useful partial results; content deduplication and expiring sessions
  avoid repeated evidence. Coverage distinguishes attempted downloads from
  useful returned evidence and unvisited candidates.
- **Collection exploration.** `crawl`/`map`, `web_crawl`/`web_map`, and paired
  HTTP routes explore guarded same-origin collections with robots rules, path
  scope, sitemap discovery, politeness, and page/depth/byte/time ceilings.
- Machine reads default to a configurable character window, with explicit
  full-page and separate outgoing-link opt-ins. Research defaults and collection
  ceilings are operator settings, not hardcoded request behavior.
- Literal text and optional generated prose neutralize active image markup;
  plaintext source indentation is preserved.
- Optional configured command processors can rerank evidence or attach cited
  synthesis on explicit request. Processing runs at the client/host edge,
  remains bounded and cancellation-safe, and never invokes a model from core.
- Add offline useful-evidence evaluations and `./x eval` for labeled live/saved
  packet scoring. Fixture success is not a claim of managed-engine parity.

- Failed fetches keep the HTTP status in their per-URL result (`status: 404`
  rather than `0`) in JSON and MCP output; `0` means no HTTP response.
- `search fetch -json` exits 1 when any URL fails, as text mode already did. The
  per-URL `error` stays in the JSON output.
- **Research packets.** `web_research` (MCP), `POST /v1/research` (HTTP) and
  `search research` run one to five intents, read the best distinct sources, and
  return a bounded, cited packet of dated passages with a coverage report
  (`candidates`, `read`, `failed`, `skipped`). Assembly is deterministic and
  model-free: reciprocal-rank fusion across the intents, per-host diversity, and
  per-source and total character ceilings, shared by every surface.
- Result rows carry a `published` date when the engine reports one, so a packet
  can cite when a source was published. The HTTP adapter's `published_pointer`
  locates it (default `/published`; SearXNG maps `/publishedDate`), and the
  command protocol accepts an optional `published`.
- **Read whole pages.** Extraction no longer cuts raw HTML at 120,000 characters,
  which lost the article on large pages (Wikipedia returned its contents list,
  GitHub its "Skip to content" link) without reporting truncation. Readability
  no longer gives up past 40,000 elements. `fetch.max_response_bytes` (now 16 MiB)
  bounds the download only; a body cut there reports `truncated`.
- Articles are returned as Markdown: headings, lists, tables and fenced code
  keep their structure, and code keeps its indentation and literal brackets
  (`vec![1]`, `#![allow(...)]`). Listing pages fall back to plain text without
  navigation, sidebars, site header/footer or scripts, and keep their `<title>`.
- Decode pages from their declared charset (header or `<meta>`) instead of
  forcing UTF-8, so Shift_JIS, GBK, windows-1252 and similar pages read correctly.
- Read PDF text layers. Refuse images, archives and other binaries by content
  type instead of returning them as replacement-character noise; text types
  (JSON, XML, source, plain text) are returned as text.
- Follow only prompt `<meta http-equiv="refresh">` redirects. Search no longer
  guesses redirects from scripts or lone links, which replaced error pages with
  their "Go home" target and app shells with an arbitrary URL from their scripts.
- Stop removing visible content: hidden-text and boilerplate filters match whole
  class tokens and segments and parse inline styles, so `overflow-hidden`,
  `lead-paragraph`, negative margins and responsive `hidden md:block` content are
  kept, and a furniture-named wrapper never removes the article inside it.
- Fix the image exfiltration guard: images nested in links (the README badge
  pattern `[![alt](img)](link)`) and inside brackets survived extraction. Image
  syntax is now neutralized everywhere, and link reference definitions are broken
  so reference-style images cannot resolve.
- Read pages in windows without losing anything: `web_fetch` and `search fetch`
  return `next_offset` when a page continues past `max_characters`, and accept
  `offset` (`-offset`) to read on. The 40,000-character ceiling on
  `max_characters` is removed. Focused passages match whole words (`is` no longer
  matches `this`), match inside CJK text, and are returned in reading order. The
  CLI and MCP share one implementation, `core::text::focus`.
- The paired HTTPS host has one JSON API. `POST /v1/execute` is removed; paired
  CLI and stdio MCP clients use `GET /v1/search`, `POST /v1/fetch` and
  `GET /v1/status`. `POST /v1/fetch` returns `{"pages":[...]}` in input order.
- An omitted `limit` on `GET /v1/search` and MCP `web_search` uses the host's
  `search.max_results` instead of a fixed 10; requests stay capped at 50.
- CLI, MCP and HTTP share one set of input bounds (1–512-byte queries, 1–10 URLs
  of at most 8192 bytes). Local `search fetch` now refuses more than 10 URLs.
- **Breaking:** engines are defined in settings instead of installed packages.
  `engines.use` selects IDs; `engines.config.ID` overrides the built-in `mwmbl`
  and `searxng` presets or defines a custom `http`/`command` adapter. Settings are
  trusted operator configuration: a command entry runs that program. Command
  engines take an absolute `command` or a bare name on `PATH`, and an optional
  absolute `cwd` (default: the command's directory, else its private scratch
  directory). Remove `search install`, `update`, `remove`, `engines available`,
  `enable --trust`, and the package store (`SEARCH_HOME/engines`), manifests,
  receipts and leases; a running agent no longer blocks engine changes. To keep a
  custom package, move its manifest `adapter` into `engines.config.ID`.
- Engine commands are `search engines [list]`, `configure`, `enable`, `disable`
  and `test`; `test` prints results like a search (`-json` for JSON).
- **Breaking Rust API:** the engine runtime moved to `search::core::engines`;
  `Query`, `Answer` and `Link` are defined in `search::core`, removing the
  core↔engines dependency cycle. The `Engine` trait returns `Found`.
- One invalid result row (such as an empty Mwmbl title) no longer fails the whole
  engine: invalid rows are skipped and reported as `skipped` in engine status.
- URL deduplication keeps the `ref` query parameter, which selects branches on
  code hosts. A panicking engine is reported under its own name.
- A leading `-config PATH` applies to the command that follows
  (`search -config PATH fetch URL`).
- Reject every local-use NAT64 address (`64:ff9b:1::/48`): network-specific
  translation layouts cannot be inferred safely. Public destinations encoded in
  this range now require the existing explicit private-network override too.
- Protect Unix trust storage against replacement through writable homes or
  ancestors, including when no trust directory exists; unsafe storage fails
  instead of silently selecting local execution. Bind a host's listener before
  renewing its pairing code, so failed duplicate starts preserve the live code.
  Windows validates the same boundaries with protected user ACLs.
- Neutralize terminal and Unicode direction controls in human search/fetch
  output without modifying JSON data.
- Follow prompt redirect stubs even when they carry no readable text, preserve
  the requested URL in `Page.url` (`final_url` names the destination), and fail
  textless stubs whose redirect cannot be followed as empty.
- Count each normalized result URL once per engine during reciprocal-rank
  fusion, so duplicate rows cannot manufacture independent agreement.
- The CLI's omitted `-limit` uses `search.max_results` instead of a fixed 10,
  matching the documented default; every surface still caps requests at 50.
- Count blank-line separators against focused-passage budgets so joined
  passages fit their character limit.
- `truncated` reports only a download cut at `fetch.max_response_bytes`; a
  `max_characters` window signals continuation through `next_offset` alone.

- Add native x86_64/ARM64 CI and release builds for Linux, macOS, and Windows;
  publish Windows ZIPs and a checksummed PowerShell installer, and test installers
  on their native systems. Use runner-local temporary paths for security tests.
- Tighten the SSRF guard: refuse the `0.0.0.0/8` "this network" block, local-use
  NAT64 `64:ff9b:1::/48` (RFC 8215), and the `3fff::/20` documentation range
  (RFC 9637), with counterexample
  tests beside the existing ones.
- Follow exactly the configured number of HTTP redirects: a budget of `N` now
  allows `N` redirects, not `N` more taken than intended.
- Recover engine-store mutations when a crash interrupts the cleanup of a
  replaced package tree and its lease file is already gone — recreating the
  lease when the old tree is restored — instead of wedging all later installs,
  updates, and removals.
- Route executable-engine scratch files through a per-user private directory
  under the OS temp root via `TMPDIR`, `TMP`, and `TEMP`, so shared hosts cannot
  collide or lock each other out of the default location. Configure `temp_dir`
  to override the location; unsafe directories fail closed. Files are not
  automatically cleaned up, and executable engines remain unsandboxed.
- Match fetched-page passages case-insensitively for non-ASCII letters (such as
  `Ü`/`ü`), not only ASCII, so accented queries find their paragraphs.
- Add focused navigation READMEs for configuration, fetching, and CLI internals,
  and expand the existing engine module guide.
- Consolidate shipped engine assets and their Rust implementation under
  `src/engines/`; move the runnable custom POST starter into `docs/examples/`.
  Installed engine locations and package contracts are unchanged.
- Make `./x` argument handling consistent: show command help, forward formatting
  arguments, and reject unexpected arguments to shell and guard checks.
- Keep search orchestration opt-in and bounded: only Mwmbl runs by default,
  additional engines require explicit activation, the per-engine deadline is
  five seconds, and unavailable selections produce an explicit engine error.
- Preserve literal square brackets in fetched code and prose while continuing
  to neutralize Markdown link/image targets.
- Show failed and timed-out engines in terminal search output without exposing
  program stderr or credential values.
- Focused fetches with no matching passages return bounded page text rather than
  labelling the opening as a match; preserve truncation metadata for that fallback.
- **Breaking Rust API:** consolidate into one root `search` package and library,
  with the `search` CLI binary. Import `search::core::{Search, Config, Query,
  Answer, Page, Link}`, `search::engines::{Engine, Pool}` and package APIs,
  `search::client::Client`, and `search::mcp::Server` directly; old facade imports
  and separate crate names are removed. Default `cli` includes MCP and hosting;
  minimal and MCP-only library builds are supported. Modules replace compilation
  boundaries; conventions and boundary tests enforce dependency direction.
  Manifest schema 1, command protocol 1, and on-disk formats remain unchanged.
  Binary upgrades do not replace installed engines, settings, or credentials.

- Refresh onboarding, engine integration, hosting, and contributor documentation;
  prefer `search help` and `search version`, clarify setup prerequisites, and
  align CLI help and diagnostics with the primary commands.
- Add `./x help` and make installer help work when read from stdin as well as a
  saved script. Keep existing help/version flag aliases for compatibility.
- Define manifest schema 1 and command protocol 1 as stable engine contracts
  across Search releases. Document the shared store/runtime for shipped and
  custom engines and protect installed custom packages with a compatibility test.
- Bound paired-host responses with `remote.max_response_bytes` (default 64 MiB)
  and align CLI focused reads with the 6000-character MCP default. Unmatched
  queries fall back to page text.
- Engine management commands reserve their first word; use `search -- QUERY`
  when a literal query begins with a command name.
- Bound ephemeral fetch caching with `fetch.cache_bytes` (default 32 MiB),
  accounting for string payloads and entry overhead. Zero disables storage;
  oversized pages skip caching, and full caches clear all entries.

- Re-check HTTP redirect destinations before dialing and ignore environment
  proxies in the page reader, so neither can bypass the SSRF guard.
- **Breaking:** remove the persistent local corpus, index/refresh commands, and
  corpus API. Reject `dir`, `search.local_weight`, `fetch.max_stored_chars`, and
  `index` settings; remove `DIR`/`-dir`. Existing files and data are untouched.
  Search now uses live web engines and clean fetch with a transient memory cache.
- **Breaking:** rename user selection/attribution fields from `providers` to
  `engines` and CLI `-providers` to `-engines`. Rename core `discovery` to
  `engines`, `Provider` to `Engine`, `Finding` to `Link`, `Response` to `Answer`,
  `Fetched` to `Page`, `Registry` to `Pool`, and `AdapterProvider` to `Adapter`.
  The service remains `Search`; package transport configuration remains `adapter`.
- Move shared local/remote execution into `search::client`;
  MCP owns only tools and transports. Retain optional paired HTTPS hosting and
  remote routing without local fallback.
- MCP fetch with no query or character limit returns the full clean page under
  the fetch transport body bound, with query-focused reads defaulting to 6000
  characters per page.
- Reduce the shipped catalog to Mwmbl and SearXNG. Remove vendor API and scraper
  packages and their helpers; existing installed packages/settings are untouched.
  Custom HTTP and command engines still merge concurrently. Document AI/API
  integrations with a tested, copyable authenticated JSON POST starter outside
  the catalog. `search install NAME|./PATH` is the primary installation command;
  `search engines install` remains an alias.
- Document language-independent executable engines, an offline starter, declared
  runtimes/credentials, and native-binary packaging.
- Replace special built-in engines with installable packages and local lifecycle
  commands; shipped assets live under `src/engines/`. `SEARCH_HOME` selects packages, settings, and
  separate private trust storage. Updates preserve user settings and package
  edits; executables inherit only declared credentials and a minimal environment.
  Release builds target the root package with the default `cli` feature.
- Add configurable JSON HTTP and executable engine transports, bounded output,
  guarded endpoints, ranking/deadlines, and local `search engines list/test`
  diagnostics. Reject missing explicit settings files and invalid zero bounds;
  zero cache/redirect settings disable those features. Require current-user
  ownership of trust storage and remove unused search-cache/log-level settings.

- Add `search pair-code` to renew one-use host pairing codes without restarting.
  Persist only code hashes and admission bounds under a process lock.

- Replace shared HTTP tokens with persistent paired HTTPS identities and revocable
  per-device credentials. Add `remote pair/use/list/off/remove`, `devices`, and
  `revoke`; selected remotes execute CLI and stdio MCP operations without fallback.
- Remove `token`/`token_env` with no compatibility aliases.

- Validate MCP tool inputs, run batch searches concurrently, and return structured
  results. Fetched pages report their fetch time. Runtime settings use
  `settings.json` and consistent snake_case names without compatibility aliases.
- Enforce the configured maximum result count and whole-search deadline, fail startup if the
  guarded HTTP client cannot be built, and make the fetch-cache expiry test deterministic.
- Organize the single `search` package around protocol-free `core`, runtime
  and package `engines`, shared `client`, and optional `mcp`/`cli` modules.
  The engine exposes `search::core::Search`; minimal library builds omit MCP.
- Added theme-aware black and white lockup/logo SVGs, and used the lockup at the
  top of the README.
- Reframed the README around Search's core promise and clarified getting
  started, adoption, configuration, and security.
- Extraction keeps the article and drops the cruft: ads, cookie banners,
  related rails, and hidden text are removed, and links and images are
  neutralized so page content cannot exfiltrate. A page that only redirects
  elsewhere is now followed to its target.
- `web_fetch` supports query-focused passages and optional character limits;
  remove the unused `objective` parameter.
- Fetched pages carry byline, published time, and site for citation.
- A link's `engines` attribution is a JSON array.

## Unreleased (before this change)

- The first Rust implementation: one binary serving multi-provider discovery, a
  hardened fetcher, a private full-text index, and MCP over stdio and HTTP.
- Release tooling: `scripts/release.sh` (name the changelog section, then tag),
  `scripts/formula.sh` (the Homebrew formula), and `install.sh` (the
  checksum-verified installer), with CI and a tag workflow that builds archives,
  attests provenance, updates the formula, and installs the release.
- Documentation and contribution: `CONTRIBUTING.md`, `docs/` for users and
  `docs/contributing/` for the project, issue and pull-request templates, and a
  `./x check` that includes a shell-syntax check.
