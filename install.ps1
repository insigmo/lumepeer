# Install Lumepeer from the latest GitHub Release (Windows amd64/arm64).
# Usage:
#   irm https://raw.githubusercontent.com/insigmo/lumepeer/refs/heads/master/install.ps1 | iex
#   or: .\install.ps1 [-Version v0.0.3]

param(
    [string]$Repo = "insigmo/lumepeer",
    [string]$Version = $env:LUMEPEER_VERSION
)

$ErrorActionPreference = "Stop"

function Resolve-LumepeerVersion {
    param([string]$Repo, [string]$Version)
    if (-not [string]::IsNullOrWhiteSpace($Version)) {
        return $Version.Trim()
    }
    $headers = @{
        Accept = "application/vnd.github+json"
        "Cache-Control" = "no-cache"
    }
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers $headers
    if (-not $release.tag_name) {
        throw "Could not resolve latest release tag for $Repo"
    }
    return [string]$release.tag_name
}

function Get-LumepeerArch {
    # OSArchitecture reflects the real OS, unlike PROCESSOR_ARCHITECTURE which
    # reports the *process's* architecture and lies under x64-on-ARM64 emulation.
    switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
        "Arm64" { return "arm64" }
        default { return "x64" }
    }
}

$Version = Resolve-LumepeerVersion -Repo $Repo -Version $Version
$arch = Get-LumepeerArch

$headers = @{
    Accept = "application/vnd.github+json"
    "Cache-Control" = "no-cache"
}
$release = $null
try {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/tags/$Version" -Headers $headers
} catch {
    throw "Could not find release $Version for $Repo — publish a GitHub Release (tag v*) so it has assets. $_"
}

# Prefer the NSIS installer (supports silent /S); fall back to MSI.
$asset = $release.assets | Where-Object { $_.name -match "_${arch}-setup\.exe$" } | Select-Object -First 1
$installerKind = "nsis"
if (-not $asset) {
    $asset = $release.assets | Where-Object { $_.name -match "_${arch}_en-US\.msi$" } | Select-Object -First 1
    $installerKind = "msi"
}
if (-not $asset) {
    throw "No Windows installer found for arch '$arch' in $Repo release $Version"
}

$tmp = Join-Path $env:TEMP ("lumepeer-install-" + [guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $tmp | Out-Null
$installerPath = Join-Path $tmp $asset.name

Write-Host "Downloading $($asset.name)…"
try {
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $installerPath -UseBasicParsing -Headers @{ "Cache-Control" = "no-cache" }
} catch {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    throw "Failed to download $($asset.browser_download_url): $_"
}

Write-Host "Installing Lumepeer $Version ($installerKind)…"
if ($installerKind -eq "nsis") {
    $proc = Start-Process -FilePath $installerPath -ArgumentList "/S" -Wait -PassThru
} else {
    $proc = Start-Process -FilePath "msiexec.exe" -ArgumentList "/i", "`"$installerPath`"", "/quiet", "/norestart" -Wait -PassThru
}

Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue

if ($proc.ExitCode -ne 0) {
    throw "Installer exited with code $($proc.ExitCode)"
}

Write-Host ""
Write-Host "Installed Lumepeer $Version ($arch)."
Write-Host "Launch it from the Start Menu."
