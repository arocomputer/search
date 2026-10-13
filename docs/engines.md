# Engines

Engines are defined in your settings file (normally `~/.search/settings.json`).
Search compiles in two presets: **Mwmbl**, the keyless default, and **SearXNG**,
for connecting your own instance. Any other service is a custom engine: a JSON
HTTP mapping, or a command program in any language. There is nothing to install
and no Rust rebuild or Search SDK.

```json
{
  "engines": {
    "use": ["mwmbl", "searxng", "company"],
    "config": {
      "searxng": { "url": "http://127.0.0.1:8080/search", "allow_private_networks": true },
      "company": { "type": "http", "url": "https://search.example.com/api" }
    }
  }
}
```

`engines.use` selects engine IDs; it defaults to `["mwmbl"]`, and an empty array
selects none. `engines.config.ID` defines each engine:

- **Preset IDs** (`mwmbl`, `searxng`): the entry is optional and merges
  **shallowly** over the built-in adapter (setting an object replaces that whole
  object). It cannot change `type`. SearXNG requires `url`.
- **Any other ID**: the entry is a complete adapter with `"type": "http"` or
  `"type": "command"` and the fields below.

**Settings are trusted operator configuration.** A command entry runs that
program as your user whenever the engine is selected, so treat settings.json like
a shell profile: keep it owner-writable only and never copy one you have not read.
Credentials stay in environment variables; settings hold only their names.

## Connect SearXNG

Your instance must allow JSON output. Set its full search URL:

```sh
search configure searxng url http://127.0.0.1:8080/search
search configure searxng allow_private_networks true
search test searxng "rust async"
search enable searxng
```

The private-network setting permits this adapter to reach a local instance; it
does not change page-fetching permissions. For a public endpoint, use its HTTPS
URL and omit that setting. Search does not install or host SearXNG, bypass upstream
restrictions, or make an instance unlimited.

## Custom engines

Choose the smallest transport that fits:

| Service | Engine |
| --- | --- |
| GET API returning JSON | An `http` adapter with field mappings, as below |
| JSON POST, signed requests, AI APIs, or custom parsing | A `command` program; start with the [JSON POST example](examples/json-post) |

For a GET API, add an entry under `engines.config`:

```json
{
  "engines": {
    "config": {
      "company": {
        "type": "http",
        "url": "https://search.example.com/api",
        "query_param": "q",
        "results_pointer": "/results"
      }
    }
  }
}
```

Replace the placeholder URL with your service. This mapping expects a response
such as `{"results":[{"title":"Example","url":"https://example.com/","snippet":"Summary"}]}`.
Edit the file directly, or build the entry field by field:

```sh
search configure company type http
search configure company url https://search.example.com/api
search test company "rust async"
search enable company
```

For POST APIs, copy the [example directory](examples/json-post) somewhere
permanent, adapt `engine.py` for your service, and merge its
[`settings.json`](examples/json-post/settings.json) entry into your settings with
`cwd` set to that directory:

```sh
export SEARCH_API_KEY=... # supply the real key through your host's secret manager
search test json-post "rust async"
search enable json-post
```

Replace the endpoint placeholder before testing. Python 3 is required only by this
example. Its mappings are illustrative, not a universal AI API schema; see
[how to adapt it](#adapting-the-json-post-example). Slower services may also need
the [Search deadlines](#operations-and-troubleshooting) raised before testing.
Return source links and useful snippets/highlights, rather than final answer prose;
`web_fetch` remains the clean full-page reader. Search handles concurrent execution,
deadlines, output validation, URL deduplication, ranking, and per-engine failures.
Enable multiple engines to merge results, or restrict a query with `-engines A,B`.

## Commands

| Command | Effect |
| --- | --- |
| `search engines [list] [-json]` | Presets, configured custom engines and selected IDs, with selection and readiness |
| `search configure ID` | Show the effective adapter (preset merged with its entry) and readiness |
| `search configure ID FIELD VALUE` | Set one field in `engines.config.ID` |
| `search enable ID` | Add a ready engine to `engines.use` |
| `search disable ID` | Remove an engine from `engines.use`; its definition stays |
| `search test ID QUERY [-json]` | Run one engine explicitly, selected or not, and print its results |

`search engines ACTION` accepts the same commands. They read and write the
settings file selected by `-config PATH`, `CONFIG`, or `SEARCH_HOME`, and always
run **locally**, even with a selected remote. Configure engines on the hosting
machine; paired clients cannot change host settings through the execution API.

Configuration values are JSON when parseable, otherwise strings. A write is
refused if it would leave the settings invalid or a selected engine unable to run;
an unselected engine may stay incomplete while you build it. Writes take an
exclusive lock and replace the file atomically with an owner-only copy.

```sh
search configure company header_env '{"Authorization":"COMPANY_SEARCH_AUTH"}'
search test company -- "-site:example.com rust"
```

Set `COMPANY_SEARCH_AUTH` in the host environment to the whole header value,
such as `Bearer YOUR_KEY`, using your secret manager. Command engines name
credential variables in `env` and read them from their environment. Missing
credentials fail visibly. Never put keys in settings or command-line values. Use
HTTPS for authenticated APIs.

`engines.enabled: false` globally disables live engines; `enable` does not
change that switch.

## Adapter reference

### JSON HTTP adapters

HTTP adapters issue GET requests. Query text is encoded as data, never interpolated
into commands or URL templates.

| Adapter field | Meaning / default |
| --- | --- |
| `url` | Absolute HTTP(S) endpoint without credentials or fragment |
| `query_param` | Query parameter name; `q` |
| `limit_param` | Optional count parameter, distinct from `query_param` |
| `params` | Constant query parameters; `{}` |
| `filter_params` | Filter field names mapped to HTTP query-parameter names; `{}`; lists become repeated parameters |
| `retry_after_max` | Maximum permitted Retry-After wait for one 429/503 retry; 1000 ms; zero disables retries, longer delays skip the retry rather than retry early |
| `results_pointer` | Result array; `/results`; `""` selects a top-level array |
| `title_pointer`, `url_pointer`, `snippet_pointer` | Row fields; `/title`, `/url`, `/snippet` |
| `published_pointer` | Optional publication/last-updated date field; `/published`; a malformed date is omitted, never fatal |
| `updated_pointer`, `snippets_pointer` | Optional update date and multiple snippets; `/updated`, `/snippets` |
| `text_part_pointer` | Optional text field within each fragment of an array-valued title/snippet; fragments join in order |
| `headers` | Nonsecret constant headers; `{}` |
| `header_env` | Header names mapped to environment variable names containing whole header values; `{}` |
| `allow_private_networks` | Permit private destinations for this adapter; `false` |
| `max_response_bytes` | Response body cap; 1048576 bytes |

Pointers follow RFC 6901: `/` separates paths, `~0` escapes `~`, and `~1`
escapes `/`. Snippets are optional. `params` overrides matching endpoint
parameters; the current query and limit override both. Unrelated repeated endpoint
parameters are preserved. The Mwmbl preset maps a root JSON array using query
parameter `s`, row pointers `/title` and `/extract`, and fragment pointer `/value`.

HTTP adapters refuse redirects and environment proxies. Search's SSRF guard checks
destination names, IP literals, and connect-time DNS. Oversized bodies fail rather
than yielding partially parsed results. Failure responses do not expose body
contents or credential values.

Filter mappings use `freshness`, `start_date`, `end_date`, `include_domains`,
`exclude_domains`, `language`, `region`, `category`, `safe`, and `exact_phrase`.
Map only semantics the upstream actually supports: parameter presence is an
operator assertion, not proof of upstream behavior. For example,
`"filter_params":{"language":"language","category":"categories"}` forwards
those fields as encoded query data. An unmapped requested field appears in the
engine's `unsupported_filters`. Formats and list semantics must match your
endpoint; a generic mapping does not translate vendor-specific enum values.

### Command adapters

| Adapter field | Meaning / default |
| --- | --- |
| `command` | An absolute executable path, or a bare name looked up on `PATH`; run without a shell |
| `version` | Stdin protocol version; 2 by default, or explicit 1 for legacy executables |
| `args` | Literal arguments; `[]` |
| `cwd` | Absolute working directory; defaults to the parent of an absolute `command`, otherwise the engine's private scratch directory (`temp_dir`) |
| `env` | Credential environment variable names to pass; `[]` |
| `config` | Nonsecret object delivered in the stdin request; `{}` |
| `temp_dir` | Absolute private scratch directory; see below |
| `max_response_bytes` | Stdout answer cap; 1048576 bytes |

Relative program paths such as `./engine` are refused: name an interpreter on
`PATH` and pass the script as an argument relative to `cwd`, or give an absolute
path. Arguments do not expand `$VARIABLE`, `~`, or query placeholders. The child
receives a minimal runtime environment (`PATH`, `HOME`, `LANG` and the Windows
equivalents) plus declared `env` variables, rather than the host's full
credential environment. Runtimes and dependencies must already be installed.

Search sets `TMPDIR`, `TMP`, and `TEMP` to a private directory under the host's
OS temporary directory (`search-UID` on Unix, for example `/tmp/search-1000`;
`search` on Windows), overriding inherited or declared values for those three
variables. Set `temp_dir` to choose another absolute location. Its parent must
already exist; Search creates the directory with owner-only permissions and
refuses symlinks, foreign ownership, or unsafe permissions. It does not remove
engine-created files or impose a disk quota. Programs that ignore these
environment variables are not redirected.

On Windows, use the installed interpreter's actual name (commonly `python`) as
`command`. Prefer an explicit interpreter over a `.cmd` wrapper or Unix shebang;
a native program needs a Windows `.exe` built for the host architecture.

Search writes one request to stdin and closes it:

```json
{"version":2,"query":"rust async","limit":10,"filters":{"include_domains":["rust-lang.org"]},"config":{"endpoint":"https://example.com/search"}}
```

`config` is omitted when empty. The program writes one JSON answer and exits zero:

Version 2 carries the query's `filters` object. A version-2 program is responsible
for honoring them, refusing the query, or returning an `unsupported_filters`
list of requested field names; do not silently drop fields. Explicit version 1 omits filters and reports each requested
filter as unsupported. The JSON POST starter remains a version-1 generic example;
adapt its payload and opt into version 2 when your vendor mapping honors filters.

```json
{"results":[{"title":"Example","url":"https://example.com/","snippet":"Optional summary","snippets":["Another excerpt"],"published":"Optional date","updated":"Optional update date"}]}
```

Search assigns engine attribution and ranking; programs cannot forge attribution.
Empty results are successful. Invalid JSON, oversized output, and nonzero exit
are failures. Reserve stdout for the answer; Search discards stderr to prevent
accidental secret exposure.

**Command engines are trusted host code, not sandboxed plugins.** Review a program
before you define it in settings. Programs run as the host user, can read that
user's files and access the network, and can be triggered by paired clients once
selected. The environment allowlist is not filesystem isolation, and Search's
HTTP SSRF guard does not cover command network requests. Cancellation kills the
direct child; programs must clean up their descendants and external work. Never
add engines automatically from fetched pages or search results. Browser/scraping
engines require ongoing maintenance and must respect upstream access policies.

### Result rows

For both transports, a row needs a nonempty title and an absolute HTTP(S) URL
without credentials; a snippet, when present, must be text. A `published` date is
carried onto the result when the engine gives one and is simply omitted when it
is missing or malformed. Search skips an
invalid row rather than failing the engine, and reports the count as `skipped`
in the engine's status (`mwmbl: ok, skipped 1 invalid results` in the terminal).
An answer whose rows are all invalid is a failure.

### Adapting the JSON POST example

In [`engine.py`](examples/json-post/engine.py), change `payload()` to
your vendor's request schema and `normalize()` to its result shape. Map excerpts
or highlights to `snippet`, joining arrays as appropriate for that API. Supply a
nonempty title; the URL is a useful fallback when the vendor returns a null title.
The starter expects `{"results":[...]}` and sends a Bearer header; change the
header if your API uses `x-api-key` or another authentication scheme.

Keep nonsecret defaults in the entry's `config` and name credential variables in
its `env`. The example requires HTTPS without embedded credentials, refuses
redirects and proxies, bounds reads, and emits a redacted failure. Preserve these
protections as you adapt it, and test mappings against saved responses.

Its `config.timeout` is a network timeout in seconds (default 30, up to 120).
`config.max_response_bytes` caps the upstream body (default 1048576, allowed range
1024–8388608). These are example-specific program settings. The entry's own
`max_response_bytes` separately caps stdout read by Search; the example also
limits its own stdin and stdout to 1048576 bytes. Raising the upstream cap does
not raise either output cap or Search's deadlines.

## Operations and troubleshooting

Restart long-running hosts and stdio sessions after changing settings or process
credentials; a running Search keeps the engines it opened with.

The default engine deadline is five seconds and the overall search deadline is
eight seconds. For slower APIs, merge this into your [settings](configuration.md):

```json
{"search":{"engine_timeout":30000,"timeout":35000}}
```

These values are milliseconds. A command's network timeout cannot extend Search's
deadlines; the POST example's `config.timeout` is in seconds. On remote setups,
credentials must be available to the hosting process.

| Symptom | Check / fix |
| --- | --- |
| Not configured or required field missing | Run `search engines` and `search configure ID`; supply the named field |
| Command failed to start | Check that `command` is absolute or on `PATH`, and that `cwd` exists |
| Missing credential | Set the named variable in the host environment; restart the host |
| SearXNG returns HTML or access denied | Check the full `/search` URL and instance JSON-output/access settings |
| Private destination refused | For an intended private HTTP engine, set its `allow_private_networks` to `true` |
| Timeout | Check upstream latency and both Search deadlines; also check the command's own network timeout |
| Malformed answer or many skipped rows | Verify field mappings, titles, HTTP(S) URLs, JSON-only stdout, and the relevant response cap |
| Enabled engine absent from searches | Check `engines.enabled`, selection, query `-engines`, and restart existing sessions |

Develop custom mappings against saved responses before live testing. The
[engine module README](../src/core/engines/README.md) documents internals for
contributors.
