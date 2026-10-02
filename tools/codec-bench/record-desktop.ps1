# Records a lossless 1080p desktop clip on a Windows host while scripted input
# types, scrolls and drags (docs/research/software-av1.md, stage 1 material).
#
# Run it in the interactive session (a scheduled task with /IT when driven
# over SSH): Desktop Duplication has nothing to capture from session 0.
#   segment 1 ( 0-10 s): typing into an editor window
#   segment 2 (10-20 s): scrolling a Wikipedia article in Edge
#   segment 3 (20-30 s): moving the Edge window over the desktop
# The cursor is not recorded: lumepeer's guest draws its own.
param(
    [string]$Dir = $PSScriptRoot,
    [string]$Url = 'https://en.wikipedia.org/wiki/AV1',
    [int]$Seconds = 32,
    # raw: uncompressed BGRA (fine at 1080p); x264rgb: lossless RGB H.264 for
    # screens whose raw rate the disk cannot take (4K is ~1 GB/s).
    [ValidateSet('raw', 'x264rgb')][string]$Output = 'raw',
    # Edge where it exists; elsewhere Chrome. Either runs as its own instance
    # with a throwaway profile, so no window of the user's browser is touched.
    [string]$Browser = 'msedge.exe'
)
$ErrorActionPreference = 'Continue'
Set-Location $Dir
$log = Join-Path $Dir 'record.log'
"start $(Get-Date -Format o)" | Out-File $log -Encoding utf8

Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class W {
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint f);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, int data, UIntPtr extra);
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint f, UIntPtr extra);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc f, IntPtr l);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] static extern int GetWindowTextLength(IntPtr h);
  // The first visible, titled top-level window owned by process `pid`.
  public static IntPtr WindowOf(uint pid) {
    IntPtr found = IntPtr.Zero;
    EnumWindows((h, l) => {
      uint owner; GetWindowThreadProcessId(h, out owner);
      if (owner == pid && IsWindowVisible(h) && GetWindowTextLength(h) > 0) { found = h; return false; }
      return true;
    }, IntPtr.Zero);
    return found;
  }
}
"@

# A hash of the middle of the screen: whether an action visibly did anything.
function ScreenHash {
    $bmp = New-Object System.Drawing.Bitmap 800, 400
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen(400, 300, 0, 0, $bmp.Size)
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Bmp)
    $g.Dispose(); $bmp.Dispose()
    return [BitConverter]::ToString([System.Security.Cryptography.MD5]::Create().ComputeHash($ms.ToArray()), 0, 4)
}
# Physical pixels everywhere: a DPI-unaware editor would be bitmap-stretched on
# a scaled screen (250% here), which is not what a real application looks like.
[void][W]::SetProcessDPIAware()
$screen = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$SW, $SH = $screen.Width, $screen.Height

function Log($m) { "$([math]::Round($script:sw.Elapsed.TotalSeconds,2)) $m" | Out-File $log -Append -Encoding utf8 }
function Focus($h) {
    # A background process may not take the foreground unless it sent the
    # last input event. F24 rather than the usual Alt: an Alt tap on a window
    # that already is in front opens its system menu, and the modal menu loop
    # then never lets DoEvents return.
    [W]::keybd_event(0x87, 0, 0, [UIntPtr]::Zero)
    [W]::keybd_event(0x87, 0, 2, [UIntPtr]::Zero)
    [void][W]::SetForegroundWindow($h)
    Start-Sleep -Milliseconds 200
    return ([W]::GetForegroundWindow() -eq $h)
}

$script:sw = [Diagnostics.Stopwatch]::StartNew()

# Stage the scene before the recording starts: Edge with the article loaded
# behind a maximised editor window. The editor is this script's own: Windows
# 11's Notepad ignores synthetic key presses (SendInput returns success, no
# character appears), so the typing is driven from inside the window instead
# - the same picture changes a person typing would make, no input injection.
$profile = Join-Path $env:TEMP "codec-bench-browser-$PID"
$browserProc = Start-Process $Browser -PassThru -ArgumentList "--user-data-dir=$profile", '--no-first-run',
    '--no-default-browser-check', '--new-window', '--start-maximized', $Url
$edge = [IntPtr]::Zero
$deadline = (Get-Date).AddSeconds(20)
while ($edge -eq [IntPtr]::Zero -and (Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 300
    $edge = [W]::WindowOf([uint32]$browserProc.Id)
}
Log "browser pid $($browserProc.Id) hwnd $edge"
if ($edge -eq [IntPtr]::Zero) { Log 'no browser window, giving up'; exit 1 }
Start-Sleep -Seconds 6

function Pump($ms) {
    $end = $script:sw.Elapsed.TotalMilliseconds + $ms
    while ($script:sw.Elapsed.TotalMilliseconds -lt $end) {
        [System.Windows.Forms.Application]::DoEvents()
        Start-Sleep -Milliseconds 5
    }
}
$form = New-Object System.Windows.Forms.Form
$form.Text = 'notes.txt - Editor'
$form.WindowState = 'Maximized'
$form.TopMost = $true
$form.BackColor = [System.Drawing.Color]::FromArgb(32, 32, 32)
$box = New-Object System.Windows.Forms.TextBox
$box.Multiline = $true
$box.WordWrap = $true
$box.Dock = 'Fill'
$box.BorderStyle = 'None'
$box.Font = New-Object System.Drawing.Font('Consolas', 11)
$box.BackColor = [System.Drawing.Color]::FromArgb(32, 32, 32)
$box.ForeColor = [System.Drawing.Color]::FromArgb(230, 230, 230)
$form.Controls.Add($box)
$form.Show()
Log 'editor shown'
[void](Focus $form.Handle)
$box.Focus() | Out-Null
Pump 800
Log 'starting ffmpeg'

$sink = if ($Output -eq 'raw') { @('-f', 'rawvideo', (Join-Path $Dir 'desktop.bgra')) }
        else { @('-c:v', 'libx264rgb', '-preset', 'ultrafast', '-qp', '0', '-threads', '16', (Join-Path $Dir 'desktop-x264rgb.mkv')) }
$ff = Start-Process -FilePath (Join-Path $Dir 'ffmpeg.exe') -PassThru -WindowStyle Hidden `
    -RedirectStandardError (Join-Path $Dir 'ffmpeg.log') -ArgumentList (@(
        '-hide_banner', '-y', '-f', 'lavfi',
        '-i', 'ddagrab=output_idx=0:framerate=30:draw_mouse=0,hwdownload,format=bgra',
        '-t', "$Seconds") + $sink)
Pump 1500
Log 'recording'
$t0 = $script:sw.Elapsed.TotalSeconds

# Segment 1: typing, about 9 characters a second, a few words per frame burst.
$text = "Remote desktop latency is mostly a question of what the encoder does with the frames it gets. " +
        "A screen is not a camera: flat fills, sharp text and long runs of identical pixels between frames. " +
        "This paragraph is typed one key at a time so that each frame carries only a few changed glyphs. " +
        "The rest of the screen does not move at all, which is exactly what a desktop codec should notice."
$typed = 0
foreach ($ch in $text.ToCharArray()) {
    if ($script:sw.Elapsed.TotalSeconds - $t0 -ge 10) { break }
    $box.AppendText([string]$ch)
    $typed++
    Pump 110
}
Log "typed $typed characters"
while ($script:sw.Elapsed.TotalSeconds - $t0 -lt 10) { Pump 20 }
$form.Close()
Log 'segment 2: scroll'

# Segment 2: Edge in front, wheel scrolling down the article.
[void](Focus $edge)
[void][W]::SetCursorPos([int]($SW * 0.5), [int]($SH * 0.55))
$before = ScreenHash
$wheels = 0
while ($script:sw.Elapsed.TotalSeconds - $t0 -lt 20) {
    [W]::mouse_event(0x0800, 0, 0, -120, [UIntPtr]::Zero)
    $wheels++
    if ($wheels -eq 6) { Log "after 6 wheel notches screen changed: $((ScreenHash) -ne $before)" }
    Start-Sleep -Milliseconds 180
}
Log 'segment 3: move window'

# Segment 3: un-maximise Edge and move it around over the desktop.
[void][W]::ShowWindow($edge, 9)
$start = $script:sw.Elapsed.TotalSeconds
while ($script:sw.Elapsed.TotalSeconds - $t0 -lt 30) {
    $t = $script:sw.Elapsed.TotalSeconds - $start
    $x = [int]($SW * (0.156 + 0.135 * [math]::Sin($t * 1.3)))
    $y = [int]($SH * (0.130 + 0.093 * [math]::Sin($t * 1.9)))
    [void][W]::SetWindowPos($edge, [IntPtr]::Zero, $x, $y, [int]($SW * 0.625), [int]($SH * 0.704), 0x0004)
    Start-Sleep -Milliseconds 16
}
Log 'done moving'

$ff.WaitForExit(15000) | Out-Null
Log 'ffmpeg done'

# Leave the desktop as it was: close the browser instance this script opened.
[void][W]::PostMessage($edge, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)
Start-Sleep -Seconds 2
Remove-Item -Recurse -Force $profile -ErrorAction SilentlyContinue
Log 'cleaned up'
"end $(Get-Date -Format o)" | Out-File $log -Append -Encoding utf8
