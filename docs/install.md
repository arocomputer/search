# Install

Search ships only Mwmbl and SearXNG, with Mwmbl ready to use by default.
Installing Search does not require Python or a SearXNG instance. Custom command
engines bring their own runtime requirements. Use `search engines` after
installation. Settings and private trust live under `~/.search` or the explicit
`SEARCH_HOME`; see [engines](engines.md).

Release targets are Linux, macOS, and Windows, each on arm64 and x86_64.
Native CI checks each target; release builds and installation smoke tests also
run on each native platform. No language interpreter is required for the binary
or its shipped HTTP engines.

| System | Binary / requirements |
| --- | --- |
| Linux | `search`, GNU/glibc builds on Ubuntu 24.04; older glibc and musl distributions need a source build |
| macOS | `search`, release deployment target macOS 11; CI runs current macOS runners, not every older OS version |
| Windows | `search.exe`, Windows 10/11 with local NTFS storage for private settings, engine scratch, and trust; release builds statically link the C runtime |

These targets are not a promise to support every OS version, filesystem, or CPU.
The native CI matrix must pass before release; cross-compilation alone is not
runtime verification.

## The installer

### Linux and macOS

Download and review the script before running it:

```sh
curl -fsSL https://raw.githubusercontent.com/fschrhunt/search/main/install.sh -o install-search.sh
less install-search.sh
sh install-search.sh
export PATH="$HOME/.local/bin:$PATH"
search version
search help
```

It downloads the archive for this computer from the latest GitHub release,
checks it against the release's `checksums.txt`, and puts `search` in
`~/.local/bin` (or `$SEARCH_INSTALL_DIR`). Run it again to update. Pin a version
with `sh install-search.sh --version vX.Y.Z`, or select a folder with
`sh install-search.sh --dir DIR`. `SEARCH_VERSION` also selects a version;
these are installer options, separate from Search's CLI commands. Use
`sh install-search.sh help` for its own reference. The installer needs curl or
wget, tar, and sha256sum or shasum. It refuses to replace a symlink
or an installation marked as package-manager-owned; update that installation
through its package manager.

The checksum verifies the archive against the release's checksum file; review
the script and trust the release source before running downloaded code.

### Windows

In PowerShell, download and inspect the installer before running it:

```powershell
Invoke-WebRequest https://raw.githubusercontent.com/fschrhunt/search/main/install.ps1 -OutFile install-search.ps1
Get-Content ./install-search.ps1
./install-search.ps1
```

It verifies the native x64/ARM64 ZIP against the release checksums and installs
`search.exe` under `$env:LOCALAPPDATA/Search/bin`. Add that directory to your user
`PATH`, or invoke the executable by its full path. Run the script again to update;
use `-Version vX.Y.Z` to pin a release or `-Dir C:/your/bin` to select a folder.
`SEARCH_VERSION` and `SEARCH_INSTALL_DIR` provide the same defaults as the Unix
installer. PowerShell 5.1 or later is required. Follow your machine's execution
policy for reviewed scripts; the installer does not change it.
Stop running Search hosts before replacing `search.exe`; Windows may refuse
replacement of an executable that is still in use.

Search defaults to `.search` under the user's home/profile directory. Settings,
engines, and pairing work locally on Windows; use NTFS paths with private
ACLs. `SEARCH_HOME` can select another absolute location.

## Homebrew

```sh
brew tap fschrhunt/search https://github.com/fschrhunt/search
brew install search
```

## From source

Install Git, rustup, and your platform's C linker/build tools first. Rustup
installs the toolchain pinned in `rust-toolchain.toml` on the first build.

```sh
git clone https://github.com/fschrhunt/search
cd search
./x build --release
./target/release/search version
./target/release/search help
```

The binary is `target/release/search`. Add it to your `PATH`, or use its full
path in the usage examples. The root Cargo package builds both the `search`
library and binary; there is no workspace or separate CLI package. Default
features enable `cli`, including MCP and optional hosting/authentication
dependencies.

On Windows, install Visual Studio Build Tools with the C++ workload and Windows
SDK. Build in PowerShell with `cargo build --locked --release` and run
`./target/release/search.exe`. Contributor checks use `./x check` from Git Bash;
ordinary users do not need Bash or Python.

For library builds without CLI or protocol dependencies, or with MCP only:

```sh
cargo build --no-default-features
cargo build --no-default-features --features mcp
```

These build the library without the CLI binary. See the
[Rust example](../README.md#one-engine-more-surfaces) and
[module/feature map](contributing/architecture.md).

Local CLI and stdio MCP work immediately without credentials. To host one
machine for paired devices, run `search serve` and follow [remote hosting](remote.md).
