# Usage

Search runs selected web engines concurrently, merges their results, and reads
pages through a guarded fetcher. Recent pages are cached transiently in memory.
Install Search and make it available on your `PATH` first; see
[installation](install.md). Local commands need no server or pairing.

```sh
search help
search version
search "rust async runtime"
search fetch https://www.rust-lang.org
search fetch https://www.rust-lang.org -query "async"
search research "rust async runtime"
```

Engines are defined in settings; see [engines](engines.md). Mwmbl and SearXNG
are built in. Before selecting SearXNG, run your own instance with JSON output
enabled, then configure and enable it. For an instance already listening on
`127.0.0.1:8080`:

```sh
search configure searxng url http://127.0.0.1:8080/search
search configure searxng allow_private_networks true
search enable searxng
search "rust async runtime" -engines mwmbl,searxng -json
```

That private-network permission applies only to the SearXNG adapter, not page
fetching. With a selected remote, set up engines on the host instead.

Machine-first examples (see `search help` for all flags):

```sh
search "Rust ownership" -include-domains doc.rust-lang.org -json
search research "Rust ownership" -query "Rust borrowing" -sources 3 -total-chars 9000
search read https://www.rust-lang.org -max-chars 2000
search fetch https://www.rust-lang.org -full
search crawl https://www.rust-lang.org -max-pages 5 -max-depth 1
search map https://www.rust-lang.org -max-pages 5
```

Domain filters are not magic operators: check unsupported-filter reports for
your enabled adapters. In-memory cache/session reuse requires a long-lived
process (MCP, library, or paired host); independent local CLI invocations do not
persist state between runs.

Command names are reserved as the first argument. Use `search -- test driven development`
to search for words that would otherwise select a command.
Engine management and diagnostics always run locally, even with a selected remote.

## MCP

With no arguments, Search serves MCP over stdin/stdout. This common template
applies to clients that accept `mcpServers`; consult your client's documentation
for the configuration file location and schema. Ensure its process can find
`search` on `PATH`, or replace `command` with the binary's absolute path:

```json
{ "mcpServers": { "search": { "command": "search", "args": [] } } }
```

- **`web_search`** takes one to five queries, an optional result limit, and an
  optional `engines` list. An omitted limit uses `search.max_results` (normally
  10), which also caps larger requests; no request exceeds 50. It returns ranked
  titles, URLs, and snippets, plus each engine's status, distinguishing
  successful empty results from failures and timeouts.
- **`web_fetch`** takes one to ten URLs and reads HTML, plain text (including
  JSON, XML and source files) and PDF text layers; images and other binaries are
  refused by type. Machine reads default to `research.read_characters` (6000);
  explicit `full` opts into the whole clean page. `fetch.max_response_bytes`
  (16 MiB) bounds the download, and a
  page cut there reports `truncated`. `max_characters` reads a window instead: a
  page that continues reports `next_offset`, which you pass back as `offset` to
  read on. A `query` returns the matching passages in reading order (6000
  characters per page unless `max_characters` says otherwise); when nothing
  matches, the opening of the page is returned instead.
- **`web_research`** takes one to five intents and returns a bounded, cited
  `Packet`: it fans the intents across the engines, reads the best distinct
  sources (default 5, at most 2 per host), and returns each source's dated
  passages with the engines that surfaced it and a `coverage` report
  (`candidates`, `read`, `failed`, `skipped`). Passage budgets default to 4000
  characters per source and 12000 for the packet, and never exceed 50000 and
  200000. Use it instead of chaining `web_search` and `web_fetch` when you need
  evidence in one call. It is deterministic and makes no model call.
- **`web_crawl`** reads a same-origin collection under page, depth, byte, time,
  and politeness limits. Robots rules and optional path scope apply to every
  discovered page. It returns pages, discovered URLs, and explicit failures.
  Machine page text also has per-page and total character budgets. `windows`
  records returned/available characters and a resumable offset for every page;
  a page omitted by the text budget can be read separately from offset zero.
  `full` explicitly opts into full crawled text, still within the byte budget.
- **`web_map`** discovers the same bounded URL graph without returning page
  bodies. Discovery still requires reading robots, sitemaps, and linked pages;
  it is not a free inventory of an entire website.

Search and research accept `filters`: domain inclusion/exclusion, freshness or
date range, language, region, category, safe search, and exact phrase. Adapters
declare the fields they support; check `unsupported_filters` in engine reports
before assuming every engine honored a request.

Research packets retain page and engine dates, final URLs, source hashes,
download truncation, and passage offsets. Coverage distinguishes fetching from
included evidence, duplicate or previously seen content, failures, budget
exclusions, and unvisited candidates. A source fetch failure can be replaced by
another candidate, within the attempt/time limits. A `session` identifier
excludes previously returned evidence for subsequent calls; sessions expire
and cannot be shared across paired device identities. Optional `synthesize`
and `rerank` require host configuration and explicit request opt-in.

To resume a clipped focused passage, use its offset in a **plain** read without
`query`. Query plus nonzero offset is rejected rather than silently repeating
the same passage selection.

Fetched pages include `fetched_at`, the Unix timestamp of Search's fetch, distinct
from the page's publication date. Reusing a cached page retains its fetch time.

## Paired HTTPS

```sh
search serve
```

The default listener is `127.0.0.1:8642`; set `address` for a private interface.
The JSON API and MCP endpoint share this listener. [Pair clients](remote.md) to
route CLI and stdio MCP to the host; paired clients use the same JSON API.
Remote failures are errors with no fallback.

All routes below, including health, require a per-device bearer credential over
HTTPS. Search clients pin the host certificate; `/pair` is the only public route:

```text
GET  /healthz                          {"status","version"}
GET  /v1/status                        {"version","engines"}
GET  /v1/search?q=&limit=&engines=a,b  search answer: results and engine status
POST /v1/fetch {"urls":[...]}          {"pages":[...]} in input order
POST /v1/research {"queries":[...]}    one bounded, cited evidence packet
POST /v1/search {"text":...,"filters":{...}} directional discovery
POST /v1/crawl {"url":...,"options":{...}} bounded collection reading
POST /v1/map {"url":...,"options":{...}} bounded URL inventory
POST /mcp                              MCP over streamable HTTP
```

`q` is required. An omitted `limit` uses the host's `search.max_results`
(normally 10), which also caps larger values; no request exceeds 50. `engines`
is an optional comma-separated list of package IDs. A fetch reports per-URL
failures in each page's `error` field. The host applies its engine
configuration, credentials, and fetch policy.

Every surface (CLI, MCP, and HTTP) shares the same input bounds: a query is
1–512 bytes after trimming, a fetch takes 1–10 URLs of at most 8192 bytes each,
`web_search` takes 1–5 queries, and `web_research` takes 1–5 queries and clamps
its packet to the ceilings in its options. The HTTP API answers violations with
`400 {"error": "..."}`.

## Clean pages

Extraction finds the article and returns it as Markdown — headings, lists,
tables and fenced code with its indentation intact — without navigation, ads,
cookie banners, related rails, or visually hidden text. Pages without an
article, such as listings, are read whole as plain text without their site
chrome. Text is decoded from the page's declared charset. Links become text and
images become alt text, and link reference definitions are broken, so page
content cannot carry an active image link to a consuming client.
A prompt `<meta http-equiv="refresh">` redirect is followed through the same
fetch guard; scripted redirects are not guessed at. The requested URL stays in
`url` and the retrieved destination is reported in `final_url`. Private and link-local
destinations are refused; see [security](security.md).

From the CLI, `search fetch URL -max-chars N` reads a window and prints where it
continues; `-offset N` reads on from there.
