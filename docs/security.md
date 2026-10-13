# Security

Search is built to run on a private machine and be reached over a private
network. It still treats every URL and every page as hostile.

## The fetcher

The guard is in `src/core/fetch/guard.rs`. A URL is model-chosen, so the fetcher
assumes it is hostile:

- **Private destinations are refused.** Loopback, RFC1918, link-local (including
  cloud metadata at `169.254.169.254`), CGNAT, multicast, and IPv6 unique-local
  are refused by literal, by name, and — through a resolver installed on the
  client — by every address a name resolves to. A DNS answer that changes between
  the check and the dial (rebinding) is caught, because the socket opens to the
  address that passed the guard.
- **Every IPv6 form that embeds such an IPv4 is refused**, including NAT64
  (`64:ff9b::/96`), 6to4 (`2002::/16`), and IPv4-compatible (`::/96`), and the
  bracketed forms `url` produces.
- **Local-use NAT64 is refused entirely.** `64:ff9b:1::/48` (RFC 8215) permits
  network-specific translation layouts; a public-looking prefix can translate
  to a private IPv4 destination. Search cannot safely infer those layouts.
- **Redirects are re-checked per hop**, bounded by `max_redirects`. A
  client-side `<meta http-equiv="refresh">` redirect is followed only through
  the same guard, so page content cannot steer the fetcher inside the network.
  Scripts are never executed or mined for URLs.
- **Image markup never survives extraction.** Inline images, images nested in
  links (the badge pattern), and reference-style images are neutralized in
  prose and code alike, so a rendered answer cannot autoload an attacker URL.
- **Bodies are size-capped** and **requests carry a deadline**. Extraction,
  including PDF parsing, runs on a blocking thread over at most
  `fetch.max_response_bytes`; a parser failure fails that page, not the process.
  PDF parsing decompresses page streams in memory, so a hostile PDF can use more
  memory than its download size; lower `max_response_bytes` on constrained hosts.

The one escape hatch, `allow_private_networks`, disables the guard for tests and
air-gapped mirrors. Never set it on a reachable service.

Byte limits bound retained response bodies, not peak parser memory or internal
PDF decompression. Deadlines cancel callers and queued parsing, but cannot kill
an already-running blocking parser; it retains its concurrency slot until it
finishes. Hard CPU/memory isolation requires OS/process limits around the service.

Collection traversal reuses this boundary for robots files, sitemaps, pages,
HTTP redirects and meta refreshes. Every hop must also remain within the root
origin and applicable path/robots scope. Traversal is bounded by total response
bytes, page count, link depth, delay and the whole-call deadline. Missing robots
files allow traversal; failed or incomplete policy reads fail closed. Robots
rules are a politeness policy, not an authorization or content-trust mechanism.
Outgoing links are returned only in a separate opt-in field, not reinserted into
prose; image targets are excluded. Consumers should not render that URL inventory
as automatically loaded resources.

## Untrusted content

Engines and settings are operator-owned, not model-chosen request fields.
Settings are trusted configuration: a command engine entry runs that program.
Engine management is local-only, and listing or configuring engines never runs
engine code. Search never discovers engines from the current working directory.
JSON HTTP adapters use the same host and DNS guard as the fetcher, prohibit
redirects and proxies, and require per-adapter opt-in for private networks.
Executable adapters are **trusted code running as the host user**, without a
sandbox; paired devices may trigger them through searches. Only explicitly declared
credentials and a minimal runtime environment are passed through, which is not
filesystem isolation. Their descendants are the adapter's responsibility.
See [engines](engines.md). Credentials
should come from host environment variables, never settings values.

A fetched page is untrusted content, and the cheapest place to hide an
instruction aimed at a model is text a person never sees. Extraction removes
visually hidden text — inline `display:none`/`visibility:hidden`/`opacity:0`,
the `hidden` attribute, `aria-hidden`, screen-reader class names, and off-screen
positioning — and neutralizes Markdown image/reference targets, including nested
syntax and non-HTML text responses. Extracted prose also loses ordinary link
targets; literal source text keeps code layout and breaks active HTML image tags.
The remaining text is passed to the model
as data, in a clearly delimited field. This is defense in depth, not a guarantee
against prompt injection; consumers must not interpret source text as instructions.
Human CLI output also strips terminal control characters from untrusted fields;
JSON retains the original data.

Search can fetch public pages and caches recent pages transiently in memory.
Paired devices are trusted to initiate searches and fetches; extracted content
still must be treated as data by the consuming model.

Research sessions retain only bounded, expiring identities/hashes, not an index
or page corpus. Paired HTTP session keys are namespaced by device identity; MCP
transport sessions are bound to the credential that created them. Sharing a
human-readable session label does not grant another device access to its state.

Optional rerankers and synthesizers have the same trusted-host-code limitations
as command engines. They receive a packet only on explicit request, have bounded
I/O and shared concurrency admission, and their direct child is killed on cancellation.
Admission and both processing stages share a budget within the research deadline.
Declared citations must belong to successfully returned evidence, but that is
not a proof that generated claims follow from those passages. Source text and
processor output remain untrusted data.

## Authentication

Every API and MCP route, including `/healthz`, requires a revocable per-device
secret over HTTPS. Only `/pair` is public, and it requires a one-use code.
The host stores device credential hashes and compares them with `subtle`.
The one-use pairing code expires after 15 minutes and locks after 20 attempts.
`search pair-code` renews it locally without restarting the host. Only the code
hash is stored, with expiry, consumed state and failed attempts, under the same
cross-process lock used for admission. Consumption commits before a
device is issued; renewal invalidates earlier codes. Clock rollback before code
creation fails closed.
Clients verify the public certificate fingerprint before transmitting the code.
TLS checks the exact leaf certificate pin and delegates chain, hostname, expiry,
and handshake signatures to Rustls WebPKI; no insecure certificate acceptance
is used. Credential-bearing clients refuse redirects, proxies and plaintext.
Browser Origin headers are refused. Trust files are owner-only and separate
from settings; a corrupt registry fails closed. See [remote](remote.md).

Unix private storage uses current-user ownership and restrictive permissions.
Search also refuses trust storage whose home or ancestors permit replacement
by other users. This check applies before accepting an absent trust directory
as local mode. Root-owned sticky temporary directories are allowed with private
user-owned descendants. Shareable settings may remain readable by other users;
the storage boundary must not be writable by them.
Windows uses protected current-user DACLs, validates security on opened handles,
and rejects reparse points/junctions and hard-linked private files. Use a local
NTFS location; network shares and filesystems without these guarantees are not
supported for private storage. Windows administrators, like Unix root, are not
a sandbox boundary.
Atomic private-file replacement permits write sharing only on its validated
owner-only destination directory; higher ancestors retain strict sharing pins,
and directory deletion/renaming stays blocked throughout publication.

The MCP transport also validates the inbound `Host` header, to prevent DNS
rebinding against a locally running server, so a deployment names the authority
it is reached by.

## What is not a vulnerability

See [SECURITY.md](../SECURITY.md): an engine's accuracy, execution requests without a global rate limit, and `allow_private_networks` reaching private
addresses are all by design.
