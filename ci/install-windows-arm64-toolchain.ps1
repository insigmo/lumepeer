# One-time setup for the windows-arm64 client build (Taskfile.yml
# bootstrap:windows-arm64). Adds two Visual Studio components:
#
#   VC.Tools.ARM64   the arm64 MSVC toolset. The Windows SDK already ships
#                    arm64 import libraries, but without this there is no
#                    arm64 CRT or link.exe and the build dies at the link step.
#   VC.Llvm.Clang    clang. `ring` assembles its aarch64 sources from
#                    perlasm-generated .S files that MSVC's armasm64 cannot
#                    read, so its build script asks cc-rs for "clang" by name
#                    on this target - and on no other.
#
# This lives in its own script rather than inline in Taskfile.yml because of
# the quoting: Start-Process -ArgumentList joins its array with spaces and
# quotes nothing, so an unquoted install path is truncated at its first space
# and vs_installer sees `--installPath C:\Program`. It then fails with exit
# code 1 and, under --quiet, prints nothing at all - which is also why the
# exit code is checked here instead of being assumed.
$ErrorActionPreference = 'Stop'

$installerDir = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer'
$vswhere = Join-Path $installerDir 'vswhere.exe'
$vsInstaller = Join-Path $installerDir 'vs_installer.exe'
foreach ($tool in @($vswhere, $vsInstaller)) {
    if (-not (Test-Path $tool)) { throw "not found: $tool (is Visual Studio installed?)" }
}

$vsPath = & $vswhere -latest -property installationPath
if (-not $vsPath) { throw 'vswhere found no Visual Studio installation' }
Write-Host "Modifying $vsPath"

# The install path is quoted here, inside the array element itself.
$argList = @(
    'modify',
    '--installPath', "`"$vsPath`"",
    '--add', 'Microsoft.VisualStudio.Component.VC.Tools.ARM64',
    '--add', 'Microsoft.VisualStudio.Component.VC.Llvm.Clang',
    '--quiet', '--norestart'
)
$proc = Start-Process -Verb RunAs -Wait -PassThru -FilePath $vsInstaller -ArgumentList $argList
# 3010 is "succeeded, reboot pending", which neither toolset needs.
if ($proc.ExitCode -ne 0 -and $proc.ExitCode -ne 3010) {
    throw "vs_installer exited $($proc.ExitCode) - see the newest %TEMP%\dd_installer_*.log"
}

# Adding a component that is already present is a no-op and still exits 0, so
# the only trustworthy check is whether the files are actually there now.
$missing = @()
if (-not (Get-ChildItem "$vsPath\VC\Tools\MSVC\*\bin\Hostx64\arm64\cl.exe" -ErrorAction SilentlyContinue)) {
    $missing += 'arm64 cl.exe (VC.Tools.ARM64)'
}
$clang = "$vsPath\VC\Tools\Llvm\bin\clang.exe"
if (-not (Test-Path $clang)) { $missing += "clang.exe (VC.Llvm.Clang)" }
if ($missing.Count -gt 0) { throw "still missing after install: $($missing -join ', ')" }

Write-Host "ok: arm64 MSVC toolset and $clang are in place"
