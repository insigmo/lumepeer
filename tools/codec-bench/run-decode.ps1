# Runs decode-bench.html in a headless Edge instance with its own profile and
# writes the page's result line to decode-result.txt.
param([string]$Dir = $PSScriptRoot, [string]$Browser = "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe")
# A failure must fail the task: without this, a broken line left Edge unstarted
# and the scheduler still reported 0.
$ErrorActionPreference = 'Stop'
$profile = Join-Path $Dir 'edge-profile'
$log = Join-Path $Dir 'edge.log'
# A literal replace: `-replace` takes a regex, and a lone '\' is not one.
$page = 'file:///' + (Join-Path $Dir 'decode-bench.html').Replace('\', '/')
$p = Start-Process $Browser -PassThru -RedirectStandardError $log -ArgumentList "--headless=new", "--user-data-dir=$profile",
    '--no-first-run', '--enable-logging=stderr', '--v=0', $page
$deadline = (Get-Date).AddMinutes(15)
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 3
    if ((Get-Content $log -Raw -ErrorAction SilentlyContinue) -match 'DECODE-BENCH-DONE') { break }
}
Get-CimInstance Win32_Process -Filter "Name='msedge.exe'" | Where-Object { $_.CommandLine -like "*$profile*" } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
(Select-String -Path $log -Pattern 'DECODE-BENCH \{.*' | ForEach-Object { $_.Matches[0].Value }) | Out-File (Join-Path $Dir 'decode-result.txt') -Encoding utf8
(Get-ItemProperty 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}' -ErrorAction SilentlyContinue).pv | Out-File (Join-Path $Dir 'webview2-version.txt') -Encoding utf8
'done' | Out-File (Join-Path $Dir 'decode-done.txt')
