# Releases

A release is a `v*` tag on main. The release workflow creates the tag after a
manual dispatch, then the tag push starts the build and publication jobs.

## Cutting one

With Git and the authenticated GitHub CLI (`gh`) installed, use a clean,
up-to-date main checkout. Choose an unused version with nonempty `Unreleased`
notes:

```sh
scripts/release.sh v0.2.0    # opens a PR naming CHANGELOG.md's Unreleased section v0.2.0
scripts/release.sh v0.2.0    # after merging it: checks CI and dispatches the Actions workflow
```

The dispatch must run from `main`; Actions verifies the version, changelog entry,
and passing CI, then creates the tag and starts the release workflow.

## What the tag runs

`.github/workflows/release.yml` runs:

1. **check**: the tag is on main and `CHANGELOG.md` has its section.
2. **build**: `./x check` and release compilation on six native runners: Linux,
   macOS, and Windows, each on arm64 and amd64. Unix binaries use `.tar.gz`;
   Windows binaries use `.zip`. No Apple SDK or Windows linker is expected on
   a Linux host.
3. **release**: collect all six archives, generate checksums, attest build
   provenance, and publish the GitHub release with the changelog section as notes.
   Builds target the root package with the default `cli` feature. Root `build.rs`
   stamps release identity; binary upgrades do not rewrite settings or private
   trust files.
4. **formula**: regenerates `search.rb` (`scripts/formula.sh`) for the release and
   opens a bot pull request on `release/formula-vX.Y.Z`. Review and merge it
   manually; Homebrew sees the update after merge. Reruns reuse an unchanged bot
   branch and PR, refuse unexpected edits or closed PRs, and create no PR when
   the formula is already current. CI is dispatched explicitly for the formula branch.
5. **install**: a real installer run on all six native runners: `install.sh` for
   Linux/macOS, `install.ps1` for Windows, followed by version and package-store
   smoke checks.

Both `formula` and `install` depend on `release` and may run in parallel.

## Where people get it

- The [official installer](../install.md#the-installer): review `install.sh` from
  main (or `install.ps1` on Windows) before running it; it downloads the release
  and checks its checksum.
- [Homebrew](../install.md#homebrew): from `search.rb` at the top of this
  repository, which Homebrew reads as a tap.

## Repository references after a transfer

Repository URLs and the Homebrew tap are `fschrhunt/search`. Existing settings,
`SEARCH_HOME`, installer overrides, and protocol names remain compatible. For an
existing Homebrew installation, replace the old tap before upgrading:

```sh
brew untap --force arocomputer/search
brew tap fschrhunt/search https://github.com/fschrhunt/search
```
