# Configuration

Settings are read from `SEARCH_HOME/settings.json` (normally
`~/.search/settings.json`), or the file selected by `-config PATH` or `CONFIG`.
The command-line path takes precedence over `CONFIG`.
Missing explicitly selected files are errors; an absent default file uses built-in
settings. The optional `notes` object holds explanations. Unknown settings are
rejected rather than ignored.

Settings are trusted operator configuration, not shareable data: an
`engines.config` entry can run a program as you. Keep the file writable only by
you, and review any settings you did not write before using them. Credentials
never belong in settings; adapters name the environment variables that hold them.

`SEARCH_HOME` selects the application home (default `~/.search`), independently
of the working directory and settings file. It must be absolute; a leading `~/`
is expanded. It holds the default settings file and owner-only pairing
credentials under `SEARCH_HOME/trust`. Commands without an explicit `cwd` run
beside an absolute executable or, for a bare PATH name, in private scratch.
`ADDRESS` overrides the listener address.
Local CLI and stdio MCP need no pairing; hosting uses [paired HTTPS](remote.md).

## Settings

```json
{
  "notes": { "timeouts": "Timeout and cache_ttl values are milliseconds." },
  "address": "127.0.0.1:8642",
  "search": {
    "max_results": 10,
    "engine_timeout": 5000,
    "cache_ttl": 60000,
    "cache_bytes": 8388608,
    "timeout": 8000
  },
  "fetch": {
    "timeout": 15000,
    "max_response_bytes": 16777216,
    "max_redirects": 5,
    "cache_ttl": 600000,
    "cache_bytes": 33554432,
    "allow_private_networks": false,
    "max_concurrency": 8
  },
  "engines": {
    "enabled": true,
    "use": ["mwmbl"],
    "config": {}
  },
  "remote": { "timeout": 120000, "max_response_bytes": 67108864 },
  "research": {
    "sources": 5,
    "source_characters": 4000,
    "total_characters": 12000,
    "max_per_host": 2,
    "max_attempts": 20,
    "timeout": 30000,
    "session_ttl": 600000,
    "session_entries": 128,
    "read_characters": 6000
  },
  "crawl": {
    "max_pages": 20,
    "max_depth": 2,
    "max_bytes": 16777216,
    "timeout": 30000,
    "delay": 100,
    "max_hosts": 128,
    "host_ttl": 600000
  },
  "processing": {
    "timeout": 15000,
    "max_output_bytes": 1048576,
    "max_concurrency": 2
  }
}
```

Omitted fields use defaults. Deadlines, result/body limits, and concurrency must
be positive. `cache_ttl: 0` or `cache_bytes: 0` disables the transient in-memory
fetch cache; `max_redirects: 0` follows no redirects.

- `address` selects the listener. `user_agent` overrides page-fetch and HTTP-engine identity;
  by default it identifies Search and its version.
- `search.engine_timeout` bounds one engine; `search.timeout` bounds the whole
  query (defaults: 5000 ms and 8000 ms). `max_results` is the default result count
  when a request omits its limit, and caps explicit limits. HTTP and MCP also
  cap requests at 50 results. With a selected remote, the host's setting supplies
  the default rather than the client's.
- `remote.max_response_bytes` caps a paired host's JSON response (default 64 MiB).
  Raise it for larger full-page batches; exceeding it returns an error, never a
  local fallback. `remote.timeout` bounds the entire request.
- `fetch.timeout` and `max_response_bytes` bound page retrieval (defaults: 15 s
  and 16 MiB). The byte bound limits memory and work, not how much of a page a
  reader gets: extracted text is never cut short, and a download cut at the bound
  reports `truncated`. Only tests and
  air-gapped mirrors should set `allow_private_networks: true`, which disables
  the page-fetch SSRF guard.
- `fetch.cache_bytes` bounds approximate cached string payloads (text, URLs,
  metadata, and keys) plus entry structs, defaulting to 32 MiB. Hash-table slack
  and allocator overhead are excluded. The cache clears all entries when an
  insertion would exceed this budget or 2048 entries. Pages larger than the
  budget are returned without being cached; cached pages expire after `cache_ttl`.
- `engines.use` selects engine IDs; the default is just `mwmbl`.
  An empty list or `engines.enabled: false` returns no discovery results.
  `engines.config.ID` defines each engine: shallow overrides for the `mwmbl` and
  `searxng` presets (SearXNG needs `url`), or a complete `http`/`command` adapter
  for any other ID. See [engines](engines.md) for fields, setup and trust.
- `search.cache_ttl` and `search.cache_bytes` bound the transient query cache.
  Only successful answers are cached, keyed by the entire query, filters, limit,
  and selected engines. Zero disables caching. Error/timeout answers are retried
  normally on the next call rather than replayed as successful cached discovery.

The remote connection timeout is 10000 ms; the overall deadline and response
cap are controlled by the `remote` settings above.

`fetch.timeout` covers a complete page read, including concurrency admission,
HTTP and meta-refresh redirects, and extraction—not a fresh budget for each
hop. Running parser work may finish after cancellation, but keeps its shared
concurrency slot until it finishes; queued work is aborted.

## Machine retrieval

`research` sets operator defaults for evidence count, per-source and total
passage budgets, same-host diversity, bounded replacement attempts, and the
whole-call deadline. Requests can select smaller budgets; shared research
safety ceilings still apply. `read_characters` controls bounded machine reads;
explicit full-page reads retain the fetcher's byte limit. Session state is
transient, TTL-expiring, capacity-bounded, and never persisted as a corpus.

`crawl` sets collection ceilings: page count, link depth, total response bytes,
whole-call deadline, and delay between requests. Requests cannot raise the
operator ceilings. Traversal is same-origin, can be path-scoped, and honors
robots rules; a network failure reading robots does not silently grant access.
Host scheduling is shared across concurrent crawl/map calls: one request per
host at a time, with spacing at least the configured/observed robots delay.
`max_hosts` and `host_ttl` bound expiring scheduling records. Idle records can
be evicted once their required delay has elapsed; active or still-delayed hosts
are not evicted, and capacity exhaustion is an explicit failure.
Images and other binary bodies are not crawled as text.
The in-process crawler retains full extracted pages. Client/CLI/HTTP/MCP crawl
responses default to per-page `research.read_characters` and total
`research.total_characters` text windows, reporting omissions/offsets separately
from download truncation. Explicit crawl `full` opts into complete page text;
`max_characters` and `total_characters` request options set smaller or larger
windows within the shared 50,000/200,000 character safety ceilings.

`processing.reranker` and `processing.synthesizer` are optional command adapter
objects with the same executable, arguments, environment allowlist, private
scratch directory, and output-bound contract as command engines. Neither runs
unless a research request explicitly opts in. They execute at the client/host
edge, never in `search::core`. Request-controlled commands are not supported.
The shared adapter's `version` setting applies to search-engine requests only;
packet processors use the separate version-1 envelope below.
Processor admission is shared across requests to a service and bounded by
`max_concurrency`. Queueing, reranking, and synthesis share one `processing.timeout`
budget and cannot extend the request's research deadline. Packet `duration_ms`
includes this edge processing time; failures preserve the evidence packet.

A processor receives one stdin JSON object:

```json
{"version":1,"operation":"synthesize","packet":{"queries":[],"sources":[]},"config":{}}
```

The actual `packet` is the complete research packet, including provenance and
coverage. A reranker returns `{"urls":[...]}`, containing each evidence URL
exactly once; it can reorder but not fabricate or replace evidence. A
synthesizer returns `{"answer":"...","citations":["https://..."]}`. Citations
must refer to packet sources, and the generated prose remains attached to the
original evidence. Citation membership checks do not prove factual entailment;
the caller must still evaluate the generated answer. Failures are reported
without dropping evidence or exposing processor stdout/stderr in diagnostics.

Processors are trusted host programs, not sandboxed plugins. Their own network
activity is outside Search's HTTP SSRF guard. Use configured environment
variables for credentials and never enable a program from fetched content.

## Migration

Machine reads now default to a bounded character window. Use explicit full-page
opt-in when you need the entire clean body, or configure `research.read_characters`
for your usual machine window. Character-window continuation is separate from
download truncation.

Command adapters default to stdin protocol version 2, which carries directional
filters. Set `version: 1` for existing wrappers until they honor the new fields;
version-1 results explicitly report requested filters as unsupported. Existing
files are not rewritten during upgrades. Optional processing is absent by default.

Search now provides live web search and clean fetch, with no persistent local
corpus, index queries, or refresh command. Existing files and data are untouched;
there is no automatic migration or deletion.

Remove `dir`, `search.local_weight`, `fetch.max_stored_chars`, and the entire
`index` object from settings: these fields are rejected. `DIR` and `-dir` are
removed corpus overrides. Use `SEARCH_HOME` for settings and trust.

Unix trust storage now rejects homes and ancestors writable by other users,
including before a clean local startup. Remove group/other write permissions
from directories you own, or choose a private `SEARCH_HOME`; Search does not
repair permissions automatically. Settings can still be shared read-only.
The fetch and HTTP-engine guards now reject all `64:ff9b:1::/48` local-use NAT64
addresses, even those that appear to encode public IPv4 destinations. Prefer
publicly addressable endpoints; the existing explicit private-network overrides
still disable the respective guards and should be used only in trusted deployments.

Request selection and result attribution use `engines`; replace `providers`
and the CLI `-providers` flag with `engines` and `-engines`.

Old XDG paths and inline `engines.custom`/`engines.only` settings are not migrated.
The old `directory`, `token`, and `token_env` fields are also rejected.

Engine packages are gone: engines live in settings. `search install`, `update`,
`remove`, `engines available` and `enable --trust` no longer exist, and Search
ignores `SEARCH_HOME/engines` (delete it when convenient). To keep a former
custom package, move its manifest's `adapter` object into `engines.config.ID`;
for a command, make `command` absolute or a bare name on `PATH` and set `cwd` to
the directory holding its files. Mwmbl and SearXNG need no migration: their
existing `engines.config` overrides still apply to the built-in presets.
