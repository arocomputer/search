#Requires -Version 5.1
<#
Install the checksummed native Windows release without changing user settings.
Download and review this script before running it. Run it again to upgrade.
SEARCH_RELEASES overrides the release URL for offline installer tests.
#>
[CmdletBinding()]
param(
    [string]$Version = $env:SEARCH_VERSION,
    [string]$Dir = $env:SEARCH_INSTALL_DIR
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'This installer is for Windows; use install.sh on Linux or macOS.'
}
if ([string]::IsNullOrWhiteSpace($Dir)) {
    $Dir = Join-Path $env:LOCALAPPDATA 'Search/bin'
}
$Dir = [IO.Path]::GetFullPath($Dir)
$releases = $env:SEARCH_RELEASES
if ([string]::IsNullOrWhiteSpace($releases)) {
    $releases = 'https://github.com/fschrhunt/search/releases'
}
$releases = $releases.TrimEnd('/')

# Desktop PowerShell may default to TLS 1.0; use TLS 1.2 only for this request.
function Invoke-SearchRequest([string]$Uri, [string]$OutFile = '') {
    $parameters = @{ Uri = $Uri; UseBasicParsing = $true }
    if ($OutFile) { $parameters.OutFile = $OutFile } else { $parameters.Method = 'Head' }
    $previous = [Net.ServicePointManager]::SecurityProtocol
    try {
        if ($PSVersionTable.PSEdition -eq 'Desktop') {
            [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        }
        Invoke-WebRequest @parameters
    } finally {
        if ($PSVersionTable.PSEdition -eq 'Desktop') {
            [Net.ServicePointManager]::SecurityProtocol = $previous
        }
    }
}

if ([string]::IsNullOrWhiteSpace($Version)) {
    $response = Invoke-SearchRequest -Uri "$releases/latest"
    if ($response.BaseResponse.PSObject.Properties['ResponseUri']) {
        $landing = $response.BaseResponse.ResponseUri.AbsoluteUri
    } else {
        $landing = $response.BaseResponse.RequestMessage.RequestUri.AbsoluteUri
    }
    $Version = $landing.TrimEnd('/').Split('/')[-1]
}
if ($Version -cnotmatch '^v?\d+\.\d+\.\d+$') {
    throw 'Version must be vX.Y.Z.'
}
if (-not $Version.StartsWith('v')) { $Version = "v$Version" }

$architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
switch ($architecture) {
    'X64' { $arch = 'amd64' }
    'Arm64' { $arch = 'arm64' }
    default { throw "Search is built for x64 and ARM64 Windows, not $architecture." }
}
$destination = Join-Path $Dir 'search.exe'
if (Test-Path -LiteralPath (Join-Path $Dir '.search-installed-by')) {
    throw 'This installation belongs to a package manager; update it there.'
}
if (Test-Path -LiteralPath $destination) {
    $item = Get-Item -LiteralPath $destination -Force
    if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Refusing to replace a directory or reparse point at search.exe.'
    }
}

$archive = "search_${Version}_windows_${arch}.zip"
$work = Join-Path ([IO.Path]::GetTempPath()) ("search-install-" + [Guid]::NewGuid())
$next = $null
New-Item -ItemType Directory -Path $work | Out-Null
try {
    Write-Host "Installing Search $Version for windows/$arch"
    $zip = Join-Path $work $archive
    Invoke-SearchRequest -Uri "$releases/download/$Version/$archive" -OutFile $zip
    $checksums = Join-Path $work 'checksums.txt'
    Invoke-SearchRequest -Uri "$releases/download/$Version/checksums.txt" -OutFile $checksums
    $checksumLines = @(Get-Content -LiteralPath $checksums | Where-Object {
        $_ -match ('^[a-fA-F0-9]{64}\s+' + [regex]::Escape($archive) + '$')
    })
    if ($checksumLines.Count -ne 1) { throw "Missing or ambiguous checksum for $archive." }
    $expected = ($checksumLines[0] -split '\s+')[0]
    if ((Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash -ine $expected) {
        throw 'Checksum mismatch; refusing to install.'
    }

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $contents = [IO.Compression.ZipFile]::OpenRead($zip)
    try {
        $names = @($contents.Entries | ForEach-Object { $_.FullName })
        if ($names.Count -ne 2 -or @($names | Where-Object { $_ -ceq 'search.exe' }).Count -ne 1 -or
            @($names | Where-Object { $_ -ceq 'LICENSE' }).Count -ne 1) {
            throw 'Release archive must contain only search.exe and LICENSE.'
        }
    } finally {
        $contents.Dispose()
    }
    Expand-Archive -LiteralPath $zip -DestinationPath (Join-Path $work 'unpacked')
    $binary = Join-Path $work 'unpacked/search.exe'
    $got = & $binary version
    if ($LASTEXITCODE -ne 0 -or $got -cne $Version) {
        throw "Downloaded binary does not report $Version."
    }

    New-Item -ItemType Directory -Path $Dir -Force | Out-Null
    $next = Join-Path $Dir ('.search-' + [Guid]::NewGuid() + '.exe')
    Copy-Item -LiteralPath $binary -Destination $next
    if (Test-Path -LiteralPath $destination) {
        [IO.File]::Replace($next, $destination, [System.Management.Automation.Language.NullString]::Value)
    } else {
        [IO.File]::Move($next, $destination)
    }
    Write-Host "Installed $destination"
    Write-Host "Add $Dir to your user PATH if needed. Next: search help"
} finally {
    if ($next -and (Test-Path -LiteralPath $next)) { Remove-Item -LiteralPath $next -Force }
    Remove-Item -LiteralPath $work -Recurse -Force
}
