# Fetch

`Fetcher` retrieves public pages as `Page` results, preserving metadata and
reporting failures without panicking. It does not store a persistent corpus.

| File or directory | Responsibility |
| --- | --- |
| `mod.rs` | HTTP client, byte/concurrency limits, content-type policy, charset decoding, redirects, and ordered batches |
| `guard.rs` | SSRF address classification, hostname checks, and connect-time DNS validation |
| `cache.rs` | Byte- and count-bounded, in-memory TTL cache |
| `extract/mod.rs` | Main-content extraction, cleaned fallback, metadata, meta-refresh detection, and PDF text |
| `extract/visibility.rs` | Hidden-content and boilerplate filtering |
| `extract/serialize.rs` | Whitespace normalization and link/image neutralization |

## Trust boundary

URLs and redirect targets are untrusted. Check schemes and destinations before
dialing; the guarded resolver rechecks DNS answers at connection time. Literal
redirect addresses also require validation. Environment proxies are disabled.
Never weaken private/metadata address classification to make a website work.

Only prompt `<meta http-equiv="refresh">` targets are followed; scripts are never
executed or mined for URLs. Every followed target returns through the guard, with
bounded hops, while `Page.url` remains the requested URL. Explicit
`allow_private_networks` is an operator override, not a property of page content.

Only successful pages enter the transient cache. Zero TTL or byte budget disables
storage; oversized pages bypass it. Full caches clear rather than growing without
bound. Text is never truncated during extraction: the byte bound limits work,
and `core::text::focus` applies a reader's query, character window and offset.

Bodies are decoded under the configured byte cap; extraction runs off the async
executor. One page-read deadline includes admission, all redirects, and extraction.
Cancelling a wait aborts queued extraction; already running blocking
parsers retain their concurrency permit until they finish. Crawl host schedules
record completion on success, failure and cancellation, and honor the previous
request's delay before applying the next request's delay.

Preserve literal text such as `values[index]` while neutralizing Markdown images
across line breaks and reference definitions in HTML and non-HTML responses.
Article metadata receives the same literal-text safety treatment as body text. Filtering is
defense in depth, not a guarantee against prompt injection: consumers must
treat extracted text as source data, not instructions.

Run `./x test core::fetch` and `./x guard` from the repository root, followed by
`./x check`. Preserve the guard's counterexample tests. See
[security](../../../docs/security.md) and [configuration](../../../docs/configuration.md)
for deployment and limit settings.
