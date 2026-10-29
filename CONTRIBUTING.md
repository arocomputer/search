# Contributing

Search is a self-hosted web search service in Rust. This is how to change it.

## Getting set up

Rust 1.98 is pinned in `rust-toolchain.toml`; `rustup` will install it on first
build. Install Python 3 as well: the package tests use it for offline command
adapter fixtures and the custom engine example. The shipped HTTP engines do
not need Python at runtime. The offline formula PR tests also need Bash, Git,
and jq.

```sh
./x build     # build the development binary
./x check     # what a pull request must pass
```

`./x check` runs, in order:

- `./x fmt --check` — `cargo fmt --all --check`
- `python3 scripts/test-formula-pr.py` — offline formula PR and retry contracts
- `./x lint` — `cargo clippy --locked --all-targets --all-features -- -D warnings`,
  including the all-features build check
- `cargo check --locked --no-default-features` and
  `cargo check --locked --no-default-features --features mcp` — minimal and MCP-only builds
- `./x test` — the package tests
- `./x shell` — syntax checks for `x`, `install.sh`, `scripts/*.sh`, and the Bash formula publisher
- `./x guard` — `scripts/guard.sh`, the security-surface audit

CI runs `./x check` natively on Linux, macOS, and Windows, each on x86_64 and ARM64.
Tests use local fixtures and mock servers, not live engine queries. Unix CI
points `TMPDIR` at the runner's temporary directory to avoid macOS's symlinked
`/var` path. Windows security fixtures use user-profile temporary storage rather
than shared runner directories whose ownership/ACLs may legitimately be refused.

Windows contributors need Visual Studio's C++ build tools/Windows SDK, Python,
and Git Bash for `./x`. Run `cargo build --locked` directly from PowerShell for
a normal build. CI also tests the PowerShell installer against local release
fixtures, including update and checksum failure. Windows ARM64 CI uses GitHub's
native ARM runner; no local Windows device is needed to contribute.

`./x` defaults to `check` and does not rewrite source files or lockfiles.
`./x help` lists commands. Build, fmt, lint, and test forward Cargo arguments;
for example, `./x build --release` or `./x test guard`.
`./x fmt` formats files; `./x fmt --check` only checks them. `check`, `shell`,
and `guard` reject extra arguments.

## What a change needs

- **One concern per pull request.** If the description says "also", split it.
- **A test for each behavior change.** The regression test should fail against
  the unfixed code, for the reason the fix names.
- **A `CHANGELOG.md` entry** under `Unreleased` for anything user-visible.
- **Updated docs and comments.** A stale comment is worse than none.

`scripts/guard.sh` pins promises Search makes — the network hosts it may reach,
the panic-site policy, the SSRF guard, the auth layer. A change that legitimately
moves one of those boundaries updates the guard in the same diff, where review
can see it. Never work around it.

## Where things go

The file map and conventions are in [AGENTS.md](AGENTS.md); module contracts and
layering are in the [architecture guide](docs/contributing/architecture.md).
Built-in presets and custom engines use the same settings-defined adapters and
command protocol. See [engines](docs/engines.md) before adding one.

## Branches, commits and pull requests

- Before the first push, name the branch `<type>/<slug>`, for example
  `fix/rebinding-guard` or `docs/remote-setup`. Never push `main` directly.
- Conventional commit titles, plain language: `fix(search): close the DNS
  rebinding gap`. The type is `feat`, `fix`, `perf`, `refactor`, `docs`, `test`,
  or `chore`; the scope is `search`, `engines`, `cli`, `mcp`, `infra`, or `docs`.
- The title becomes the squash commit on `main`; write it as the one line someone
  reads in `git log`.
- Use the pull request template. Explain the problem, what changed, and why it
  works. List the checks you ran and their results.

## Releasing

See [docs/contributing/releases.md](docs/contributing/releases.md).

## Conduct and security

Be straightforward and kind in issues and reviews. Report a vulnerability the way
[SECURITY.md](SECURITY.md) describes, not in a public issue.
