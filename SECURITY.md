# Security

## Reporting

Report a vulnerability through GitHub's private vulnerability reporting on
<https://github.com/fschrhunt/search/security/advisories/new>. Do not open a
public issue for a security problem.

Include what you did, what happened, and what you expected. A proof of concept
helps; a working exploit is not required.

## What is a vulnerability here

Search runs on a private machine and is reached over a private network, but it
treats every URL and every page as hostile regardless. These are the properties
worth attacking:

The fetch guard lives in `src/core/fetch/guard.rs`, engine transports in
`src/core/engines/`, and hosting identity/authentication in `src/cli/auth.rs`.
Root `src/lib.rs` applies the production panic-site deny policy across modules.

- **The fetcher reaches only public addresses.** Loopback, RFC1918, link-local
  (including `169.254.169.254`), CGNAT, multicast, unique-local, and every IPv6
  form that embeds such an IPv4 must be refused, on the URL and on every
  redirect hop. A way to reach one is a vulnerability.
- **Every API and MCP route is authenticated over HTTPS, including health.**
  Access without a current paired-device credential is a vulnerability.
  The public pairing endpoint requires a bounded, expiring one-use code.
- **Untrusted HTML cannot crash the service or exhaust it.** A panic, an
  unbounded allocation, or an infinite loop driven by a fetched page is a
  vulnerability.
- **Engines are trusted local settings.** A remote execution request must not
  define engines, run programs not configured by the operator, or change host
  settings. Listing and configuring engines must not execute code. HTTP engines
  must enforce their configured host/DNS guard, independent of page-fetching
  permissions.

## What is not a vulnerability

- Hosting on a private network. Search is meant to sit behind a
  tailnet; the listener itself uses paired HTTPS.
- An engine returning wrong or stale results. Discovery borrows other indexes;
  their accuracy is their own.
- The absence of a global execution rate limit. Search is a personal service behind a private
  network and paired-device trust.
- `fetch.allow_private_networks: true` letting the fetcher reach private addresses. That setting
  is documented as disabling the guard, for tests and air-gapped mirrors only.
- An explicitly trusted executable engine accessing files or network resources
  as its host user. These programs are not sandboxed; declared environment
  credentials reduce accidental exposure but do not isolate the filesystem.
- A configured HTTP engine's explicit `allow_private_networks: true` reaching a
  private search endpoint. This does not disable the separate page-fetch guard.
