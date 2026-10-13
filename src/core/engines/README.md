# Engines

`search::core::engines` runs the engines selected in settings and merges their
answers. It holds no engine-specific code: Mwmbl and SearXNG are adapter presets
in `core::config::engines`, resolved exactly like custom engines.

| File | Responsibility |
| --- | --- |
| `mod.rs` | `Engine`, `EngineState`, `EngineStatus`, `EngineError`, `Found` |
| `adapter.rs` | HTTP and command transports and result-row mapping |
| `pool.rs` | Concurrent execution, deadlines, reciprocal-rank fusion across and within queries, and URL dedup |
| `cache.rs` | Bounded TTL cache of fully successful answers keyed by complete queries |
| `scratch.rs` | Private command scratch directories |

`Pool::new` resolves each ID in `engines.use` through
`EngineSettings::adapter`, which merges a preset with its `engines.config` entry
(or takes a custom entry as is), validates it, and gives command engines an
absolute `cwd`. Construction reads no credentials and runs no engine code; HTTP
engines build one guarded client each and reuse it. Reciprocal-rank fusion
counts each normalized URL once per engine, using its best rank; duplicate rows
cannot substitute for agreement between engines. URL identities remove fragments
and tracking parameters while preserving significant path slashes and the raw
encoding of other query parameters.

Adapters map an upstream JSON answer into `Found`. An invalid row (empty title,
missing or non-HTTP(S) URL, URL credentials, non-text snippet) is skipped and
counted; the count surfaces as `EngineState.skipped`. A missing results array,
or a non-empty one with no valid row, is a malformed answer. Error messages are
fixed strings that never carry upstream output, URLs, or credential values.

HTTP adapters dial only the configured endpoint, check its host with the SSRF
guard and guard DNS at connect time, refuse redirects and proxies, and cap the
body. Command adapters spawn without a shell, clear the environment except a
minimal runtime set and declared `env` names, write one request to stdin, cap
stdout, discard stderr, and kill the direct child when the query is dropped.
They are trusted host code, not a sandbox: settings that define them are trusted
operator configuration.

`scratch::prepare` creates the command `temp_dir` exported as `TMPDIR`, `TMP` and
`TEMP`. On Unix it walks ancestors with no-follow `openat`, requires root- or
caller-owned ancestors that others cannot write (allowing a root-owned sticky
`/tmp` with private descendants), and creates or accepts only a caller-owned
0700 directory. On Windows it pins ancestors without following reparse points
and creates the directory with a protected current-user DACL (`src/private_fs.rs`).
Scratch files are not removed automatically.

The pool runs every selected engine under `search.engine_timeout` and the whole
query under `search.timeout`. A failed, timed-out, or panicking engine is
reported by name in its `EngineState` beside the other engines' results.

`Engine::search_query` accepts the full `Query`; legacy trait implementations
continue to implement `search` and report every requested filter unsupported.
`QueryFilters::validate` checks bounded text, DNS domains and real calendar dates.
HTTP `filter_params` maps filter field names to distinct query parameters; lists
use repeated parameters and requested values override static settings. Omitted
filters retain configured static defaults. Unmapped
constraints appear in `EngineState.unsupported_filters`, including on failure.
No date filter is claimed merely because results carry date metadata.

Command settings default to `version: 2`: stdin carries `version`, `query`,
`limit`, `filters` and optional `config`. Set `version: 1` explicitly for legacy
executables; their request shape remains unchanged and requested constraints are
reported unsupported. Command v2 programs must honor filters or return `unsupported_filters` in their
response. The pool accepts only requested filter names from that report.
Both transports preserve `snippets`, `published` and `updated`; fusion retains
all distinct snippets. HTTP paths are configurable with `snippets_pointer` and
`updated_pointer`.

The pool stores only answers whose engines all succeeded, keyed by the complete
query text, filters, requested limit and engine selection. `search.cache_ttl`
(default 60000 ms) and `search.cache_bytes` (default 8 MiB) bound reuse; either zero
disables storage. FIFO eviction counts serialized answers, keys and entry overhead.
HTTP 429/503 responses with a valid `Retry-After` (seconds or IMF-fixdate) trigger
one retry only when the full server-requested delay fits `retry_after_max`
(default 1000 ms, zero turns retries off). A larger delay returns the original
HTTP rejection without retrying; delays are never shortened to fit the budget.
Backoff and the retry share existing engine/query deadlines
and are cancelled when the caller drops the query.

For configuration, adapter fields, credentials, and troubleshooting, see the
[engine guide](../../../docs/engines.md). The
[JSON POST starter](../../../docs/examples/json-post) exercises the command
protocol.

## Validation

Run `./x test core::engines` and `./x test core::config`. CLI integration tests
live under `tests/cli/`; preset mapping fixtures under `tests/engines/fixtures/`.
Repository-wide validation remains `./x check`.
