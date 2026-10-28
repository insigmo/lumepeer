//! The `--measure` body: runs as `LocalSystem` in the console session on
//! `WinSta0\Default`, performs the nine pack-01 measurements, and writes
//! `session-probe.txt`. Each measurement is isolated with `catch_unwind` so one
//! failure never costs the others.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HGLOBAL, HWND};
use windows::Win32::Security::{
    GetTokenInformation, ImpersonateLoggedOnUser, RevertToSelf, TOKEN_LINKED_TOKEN,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel, TokenLinkedToken,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcessToken,
};
use windows::Win32::UI::Controls::{
    TASKDIALOG_COMMON_BUTTON_FLAGS, TASKDIALOGCONFIG, TaskDialogIndirect,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY, VK_CONTROL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetForegroundWindow, GetWindowTextW, SetForegroundWindow,
};
use windows::core::PCWSTR;

use crate::win;

/// The TaskDialog window title; the poke finds the dialog by it.
pub const DIALOG_TITLE: &str = "Lumepeer Session Probe";

const CF_UNICODETEXT: u32 = 13;
const TDCBF_OK_BUTTON: TASKDIALOG_COMMON_BUTTON_FLAGS = TASKDIALOG_COMMON_BUTTON_FLAGS(0x0001);
const VK_F: VIRTUAL_KEY = VIRTUAL_KEY(0x46);

/// Runs all nine, then exits.
pub fn run() -> u32 {
    let report = win::report_path();
    let _ = std::fs::write(
        &report,
        format!(
            "Lumepeer pack-01 session-host probe\n\
             (LocalSystem launched into the console session on WinSta0\\Default)\n\
             started: {}\n\
             ----------------------------------------------------------------\n",
            wallclock()
        ),
    );

    each(&report, "2.1 identity/integrity/session", m_identity);
    each(&report, "2.2 DXGI capture of Default", m_capture);
    each(&report, "2.3 WASAPI loopback audio", m_audio);
    each(&report, "2.4 clipboard read + write", m_clipboard);
    each(&report, "2.5 TaskDialog + medium poke", m_dialog_poke);
    each(&report, "2.6 shell as the user", m_shell_as_user);
    each(&report, "2.7 linked-token impersonation", m_linked_token);
    each(&report, "2.8 SendInput into an elevated window", m_elevated_input);
    each(&report, "2.9 Shell_NotifyIcon tray", m_tray);

    win::append(&report, "----------------------------------------------------------------");
    win::append(&report, &format!("finished: {}", wallclock()));
    0
}

/// Runs one measurement under `catch_unwind`, writing its header then its
/// outcome lines.
fn each(report: &std::path::Path, header: &str, f: fn() -> Vec<String>) {
    win::append(report, "");
    win::append(report, &format!("== {header} =="));
    let lines = std::panic::catch_unwind(AssertUnwindSafe(f))
        .unwrap_or_else(|_| vec!["RESULT: ERROR — the measurement panicked".into()]);
    for l in lines {
        win::append(report, &format!("  {l}"));
    }
}

// ---- 2.1 -------------------------------------------------------------------

fn m_identity() -> Vec<String> {
    let mut out = Vec::new();
    let sid = win::current_user_sid().unwrap_or_else(|| "?".into());
    let integrity = integrity_rid()
        .map(|rid| format!("{rid:#06x}"))
        .unwrap_or_else(|| "?".into());
    let mut session = 0u32;
    // SAFETY: `pid` is this process; `session` outlives the call.
    let pid = unsafe { GetCurrentProcessId() };
    // SAFETY: `session` outlives the call.
    let _ = unsafe { ProcessIdToSessionId(pid, &raw mut session) };
    let console = win::console_session();

    let is_system = sid == "S-1-5-18";
    let is_system_integrity = integrity_rid() == Some(0x4000);
    let in_console = console == Some(session);
    out.push(format!(
        "user SID = {sid} ({})",
        if is_system { "LocalSystem" } else { "NOT LocalSystem" }
    ));
    out.push(format!(
        "integrity = {integrity} ({})",
        if is_system_integrity { "System" } else { "NOT System" }
    ));
    out.push(format!(
        "session = {session}, console session = {console:?} ({})",
        if in_console { "in the console session" } else { "NOT the console session" }
    ));
    out.push(verdict(is_system && is_system_integrity && in_console));
    out
}

/// This process's integrity-level RID (0x4000 == System), or `None`.
fn integrity_rid() -> Option<u32> {
    let mut token = HANDLE::default();
    // SAFETY: pseudo-handle; `token` outlives the call.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }.ok()?;
    let mut len = 0u32;
    // SAFETY: sizing call.
    let _ = unsafe { GetTokenInformation(token, TokenIntegrityLevel, None, 0, &raw mut len) };
    if len == 0 {
        // SAFETY: close the token.
        unsafe {
            let _ = CloseHandle(token);
        }
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    // SAFETY: `buf` is exactly `len` bytes.
    let got = unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            len,
            &raw mut len,
        )
    };
    // SAFETY: close the token.
    unsafe {
        let _ = CloseHandle(token);
    }
    got.ok()?;
    let label = buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
    // SAFETY: `buf` holds a TOKEN_MANDATORY_LABEL the OS wrote.
    let psid = unsafe { (*label).Label.Sid };
    // SAFETY: the integrity SID has one sub-authority: the RID.
    unsafe {
        let count = windows::Win32::Security::GetSidSubAuthorityCount(psid);
        if count.is_null() || *count == 0 {
            return None;
        }
        let last = u32::from(*count) - 1;
        let rid = windows::Win32::Security::GetSidSubAuthority(psid, last);
        (!rid.is_null()).then(|| *rid)
    }
}

// ---- 2.2 -------------------------------------------------------------------

fn m_capture() -> Vec<String> {
    use lumepeer_media::capture::CaptureTarget;
    let mut out = Vec::new();
    let mut cap = match lumepeer_media::capture::platform_capturer() {
        Ok(c) => c,
        Err(e) => {
            out.push(format!("platform_capturer() failed: {e:?}"));
            out.push(verdict(false));
            return out;
        }
    };
    if let Err(e) = cap.start(CaptureTarget::PrimaryDisplay) {
        out.push(format!("start(PrimaryDisplay) failed: {e:?}"));
        out.push(verdict(false));
        return out;
    }
    let start = Instant::now();
    let frame = loop {
        if let Some(f) = cap.snapshot() {
            break Some(f);
        }
        if let Ok(Some(f)) = cap.next_frame() {
            break Some(f);
        }
        if start.elapsed() > Duration::from_secs(8) {
            break None;
        }
        sleep(Duration::from_millis(100));
    };
    cap.stop();
    let Some(frame) = frame else {
        out.push("no frame within 8s".into());
        out.push(verdict(false));
        return out;
    };
    let data = match frame.as_cpu() {
        Ok(d) if !d.is_empty() => d.to_vec(),
        _ => {
            out.push("frame carried no CPU pixels".into());
            out.push(verdict(false));
            return out;
        }
    };
    out.push(format!(
        "frame = {}x{} {:?}, {} bytes",
        frame.width,
        frame.height,
        frame.format,
        data.len()
    ));
    // Mean luma to tell a real desktop from a black frame.
    let (mean, nonblack) = brightness(&data, frame.format);
    out.push(format!("mean brightness = {mean:.1} ({})", if nonblack { "non-black" } else { "BLACK" }));
    match save_png(&data, frame.width, frame.height, frame.format) {
        Ok(()) => out.push(format!("saved {}", win::capture_png_path().display())),
        Err(e) => out.push(format!("png save failed: {e}")),
    }
    out.push(verdict(nonblack));
    out
}

fn brightness(data: &[u8], format: lumepeer_media::capture::PixelFormat) -> (f64, bool) {
    use lumepeer_media::capture::PixelFormat;
    let mut sum = 0f64;
    let mut n = 0u64;
    match format {
        PixelFormat::Bgra8 => {
            for px in data.chunks_exact(4) {
                sum += f64::from(px[0]) + f64::from(px[1]) + f64::from(px[2]);
                n += 3;
            }
        }
        PixelFormat::Nv12 => {
            // Luma plane is the first width*height bytes; sampling all bytes is
            // a fine proxy for "is it black".
            for &b in data {
                sum += f64::from(b);
                n += 1;
            }
        }
    }
    let mean = if n == 0 { 0.0 } else { sum / n as f64 };
    (mean, mean > 2.0)
}

fn save_png(
    data: &[u8],
    width: u32,
    height: u32,
    format: lumepeer_media::capture::PixelFormat,
) -> Result<(), String> {
    use lumepeer_media::capture::PixelFormat;
    if format != PixelFormat::Bgra8 {
        return Err("non-BGRA frame; skipping png".into());
    }
    let mut rgba = Vec::with_capacity(data.len());
    for px in data.chunks_exact(4) {
        rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    let file = std::fs::File::create(win::capture_png_path()).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(|e| e.to_string())?;
    w.write_image_data(&rgba).map_err(|e| e.to_string())?;
    Ok(())
}

// ---- 2.3 -------------------------------------------------------------------

fn m_audio() -> Vec<String> {
    let mut out = Vec::new();
    let mut cap = match lumepeer_media::capture_audio::platform_audio_capturer() {
        Ok(c) => c,
        Err(e) => {
            out.push(format!("platform_audio_capturer() failed: {e:?}"));
            out.push(verdict(false));
            return out;
        }
    };
    if let Err(e) = cap.start() {
        out.push(format!("start() failed: {e:?}"));
        out.push(verdict(false));
        return out;
    }
    let start = Instant::now();
    let mut peak = 0i32;
    let mut sumsq = 0f64;
    let mut samples = 0u64;
    let mut chunks = 0u32;
    while start.elapsed() < Duration::from_secs(4) && chunks < 200 {
        match cap.next_chunk() {
            Ok(chunk) => {
                for &s in &chunk.samples {
                    let a = i32::from(s).abs();
                    peak = peak.max(a);
                    sumsq += f64::from(s) * f64::from(s);
                    samples += 1;
                }
                chunks += 1;
            }
            Err(_) => break,
        }
    }
    cap.stop();
    let rms = if samples == 0 { 0.0 } else { (sumsq / samples as f64).sqrt() };
    let heard = peak > 64; // anything above dead silence / dither
    out.push(format!("captured {chunks} chunks, {samples} samples; peak |s| = {peak}, RMS = {rms:.1}"));
    out.push(if heard {
        "audio detected on the default loopback".into()
    } else {
        "no audio detected — was something playing during the run?".into()
    });
    out.push(verdict(heard));
    out
}

// ---- 2.4 -------------------------------------------------------------------

fn m_clipboard() -> Vec<String> {
    let mut out = Vec::new();
    // Read what the user copied.
    match read_clipboard_text() {
        Some(t) => out.push(format!("read user clipboard: \"{}\"", truncate(&t, 60))),
        None => out.push("read user clipboard: (empty or non-text)".into()),
    }
    // Write our marker and read it back.
    let marker = format!("lumepeer-session-probe {}", wallclock());
    let wrote = write_clipboard_text(&marker);
    let readback = read_clipboard_text();
    let ok = wrote && readback.as_deref() == Some(marker.as_str());
    out.push(format!(
        "wrote marker and read it back: {}",
        if ok { "OK" } else { "FAILED" }
    ));
    // Best-effort paste into whatever window is in front (owner opens Notepad).
    if let Some(hwnd) = foreground_window() {
        // SAFETY: `hwnd` live.
        unsafe {
            let _ = SetForegroundWindow(hwnd);
        }
        send_ctrl_key(VK_V);
        out.push(format!(
            "sent Ctrl+V to foreground window \"{}\" — confirm the marker pasted",
            window_title(hwnd)
        ));
    } else {
        out.push("no foreground window to paste into".into());
    }
    out.push(verdict(ok));
    out
}

const VK_V: VIRTUAL_KEY = VIRTUAL_KEY(0x56);

fn read_clipboard_text() -> Option<String> {
    // SAFETY: open/close are paired; the handle is owned by the clipboard.
    unsafe {
        OpenClipboard(None).ok()?;
        let h = GetClipboardData(CF_UNICODETEXT);
        let text = match h {
            Ok(h) if !h.is_invalid() => {
                let ptr = GlobalLock(HGLOBAL(h.0)) as *const u16;
                if ptr.is_null() {
                    None
                } else {
                    let mut len = 0usize;
                    while *ptr.add(len) != 0 {
                        len += 1;
                    }
                    let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                    let _ = GlobalUnlock(HGLOBAL(h.0));
                    Some(s)
                }
            }
            _ => None,
        };
        let _ = CloseClipboard();
        text
    }
}

fn write_clipboard_text(text: &str) -> bool {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * 2;
    // SAFETY: a GMEM_MOVEABLE block the clipboard takes ownership of on success.
    unsafe {
        if OpenClipboard(None).is_err() {
            return false;
        }
        let _ = EmptyClipboard();
        let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, bytes) else {
            let _ = CloseClipboard();
            return false;
        };
        let dst = GlobalLock(hmem) as *mut u16;
        if dst.is_null() {
            let _ = CloseClipboard();
            return false;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
        let _ = GlobalUnlock(hmem);
        let set = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(hmem.0)));
        let _ = CloseClipboard();
        set.is_ok()
    }
}

// ---- 2.5 -------------------------------------------------------------------

fn m_dialog_poke() -> Vec<String> {
    let mut out = Vec::new();
    let _ = std::fs::remove_file(win::poke_path());

    // The dialog, on its own thread (TaskDialogIndirect blocks).
    let dialog = std::thread::spawn(show_dialog);

    // Wait for the dialog window to appear.
    let title = win::wide(DIALOG_TITLE);
    let appeared = wait_for_window(&title, Duration::from_secs(15));
    if !appeared {
        out.push("the TaskDialog never appeared".into());
        let _ = dialog.join();
        out.push(verdict(false));
        return out;
    }
    out.push("TaskDialog is up on your desktop".into());

    // Launch the medium-integrity poke with the user's own token.
    let poke_launched = launch_poke();
    out.push(format!(
        "medium-integrity poke launched: {}",
        if poke_launched { "yes" } else { "FAILED to launch" }
    ));
    // Give the poke time to try, then read what it did.
    sleep(Duration::from_secs(6));
    for l in read_lines(&win::poke_path()) {
        out.push(format!("[poke] {l}"));
    }

    // Did the poke dismiss the dialog? It must NOT have.
    let still_up = wait_for_window(&title, Duration::from_millis(100));
    out.push(format!(
        "after the poke, the dialog is {} (expected: still up — UIPI blocks medium->System)",
        if still_up { "still up" } else { "GONE" }
    ));

    // Now the human presses OK with the mouse.
    out.push("waiting up to 120s for you to click OK with the mouse…".into());
    let clicked = dialog
        .join()
        .ok()
        .flatten();
    match clicked {
        Some(id) => out.push(format!("the dialog closed with button id {id} (your mouse click)")),
        None => out.push("the dialog closed without a reported button (or timed out)".into()),
    }
    // Pass when the medium poke did not close it and a human click did.
    out.push(verdict(still_up && clicked.is_some()));
    out
}

/// Shows the probe TaskDialog and returns the pressed button id.
fn show_dialog() -> Option<i32> {
    let title = win::wide(DIALOG_TITLE);
    let instr = win::wide("Проба хоста сессии (пачка 01)");
    let content = win::wide(
        "Сейчас неповышенная программа попробует нажать «ОК» сама — она НЕ должна смочь.\n\
         Затем нажмите «ОК» мышью сами, чтобы закрыть это окно.",
    );
    let mut pressed = 0i32;
    let config = TASKDIALOGCONFIG {
        cbSize: u32::try_from(size_of::<TASKDIALOGCONFIG>()).unwrap_or(0),
        pszWindowTitle: PCWSTR(title.as_ptr()),
        pszMainInstruction: PCWSTR(instr.as_ptr()),
        pszContent: PCWSTR(content.as_ptr()),
        dwCommonButtons: TDCBF_OK_BUTTON,
        ..Default::default()
    };
    // SAFETY: `config` and its borrowed wide strings outlive the blocking call.
    let r = unsafe { TaskDialogIndirect(&config, Some(&raw mut pressed), None, None) };
    r.ok().map(|()| pressed)
}

fn launch_poke() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let path = exe.to_string_lossy().into_owned();
    if path.contains('"') {
        return false;
    }
    let Some(token) = win::console_user_token() else {
        return false;
    };
    let cmd = format!("\"{path}\" --medium-poke \"{DIALOG_TITLE}\"");
    let launched = win::create_process_as_user(token, &cmd, r"WinSta0\Default");
    // SAFETY: closing the user token after the spawn copied what it needed.
    unsafe {
        let _ = CloseHandle(token);
    }
    launched.is_some()
}

// ---- 2.6 -------------------------------------------------------------------

fn m_shell_as_user() -> Vec<String> {
    let mut out = Vec::new();
    let whoami = win::whoami_path();
    let _ = std::fs::remove_file(&whoami);
    let Some(token) = win::console_user_token() else {
        out.push("WTSQueryUserToken failed".into());
        out.push(verdict(false));
        return out;
    };
    let cmd = format!(
        "cmd.exe /c whoami /all > {} 2>&1",
        whoami.display()
    );
    let proc = win::create_process_as_user(token, &cmd, r"WinSta0\Default");
    // SAFETY: closing the user token after the spawn.
    unsafe {
        let _ = CloseHandle(token);
    }
    let Some(proc) = proc else {
        out.push("CreateProcessAsUser(cmd) refused".into());
        out.push(verdict(false));
        return out;
    };
    let code = win::wait_and_exit_code(proc, 30_000);
    out.push(format!("cmd.exe exit code = {code:?} (standing in for lumepeer-terminal-worker: same WTSQueryUserToken + CreateProcessAsUser path)"));
    let body = std::fs::read_to_string(&whoami).unwrap_or_default();
    let first = body.lines().find(|l| l.contains('\\')).unwrap_or("").trim();
    let medium = body.to_lowercase().contains("mandatory label\\medium")
        || body.to_lowercase().contains("средний обязательный уровень");
    out.push(format!("whoami user line: {first}"));
    out.push(format!("ran at medium integrity: {}", if medium { "yes" } else { "unconfirmed (see file)" }));
    out.push(verdict(code == Some(0) && !first.is_empty()));
    out
}

// ---- 2.7 -------------------------------------------------------------------

fn m_linked_token() -> Vec<String> {
    let mut out = Vec::new();
    let Some(limited) = win::console_user_token() else {
        out.push("WTSQueryUserToken failed".into());
        out.push(verdict(false));
        return out;
    };
    let linked = linked_token(limited);
    let dir = admin_only_dir();
    out.push(format!("admin-only directory under test: {}", dir.display()));

    let elevated_ok = match linked {
        Some(t) => {
            let ok = read_dir_while_impersonating(t, &dir);
            // SAFETY: close the linked token we obtained.
            unsafe {
                let _ = CloseHandle(t);
            }
            out.push(format!(
                "impersonating the ELEVATED (linked) token: read {}",
                if ok { "ALLOWED" } else { "denied" }
            ));
            ok
        }
        None => {
            out.push("no linked token (standard account, or default elevation) — elevated half not available".into());
            false
        }
    };
    let limited_ok = read_dir_while_impersonating(limited, &dir);
    out.push(format!(
        "impersonating the LIMITED token: read {}",
        if limited_ok { "ALLOWED (unexpected)" } else { "DENIED (expected)" }
    ));
    // SAFETY: close the limited token.
    unsafe {
        let _ = CloseHandle(limited);
    }
    out.push(verdict(elevated_ok && !limited_ok));
    out
}

fn linked_token(limited: HANDLE) -> Option<HANDLE> {
    let mut len = 0u32;
    // SAFETY: sizing call.
    let _ = unsafe { GetTokenInformation(limited, TokenLinkedToken, None, 0, &raw mut len) };
    if len == 0 {
        return None;
    }
    let mut lt = TOKEN_LINKED_TOKEN::default();
    // SAFETY: `lt` is the exact struct the class returns.
    unsafe {
        GetTokenInformation(
            limited,
            TokenLinkedToken,
            Some((&raw mut lt).cast::<c_void>()),
            u32::try_from(size_of::<TOKEN_LINKED_TOKEN>()).unwrap_or(len),
            &raw mut len,
        )
    }
    .ok()?;
    (!lt.LinkedToken.is_invalid()).then_some(lt.LinkedToken)
}

fn read_dir_while_impersonating(token: HANDLE, dir: &std::path::Path) -> bool {
    // SAFETY: impersonation is reverted on every path out.
    let ok = unsafe {
        if ImpersonateLoggedOnUser(token).is_err() {
            return false;
        }
        let r = std::fs::read_dir(dir).is_ok();
        let _ = RevertToSelf();
        r
    };
    ok
}

fn admin_only_dir() -> std::path::PathBuf {
    let users = win::logs_dir()
        .parent()
        .map(|p| p.join("users"))
        .unwrap_or_default();
    if users.is_dir() {
        return users;
    }
    std::path::PathBuf::from(r"C:\Windows\System32\config")
}

// ---- 2.8 -------------------------------------------------------------------

fn m_elevated_input() -> Vec<String> {
    let mut out = Vec::new();
    // regedit (elevated/High). Harmless Ctrl+F opens its Find dialog.
    let regedit = win::wide("RegEdit_RegEdit");
    let notepad = win::wide("Notepad");
    if let Some(hwnd) = find_window_class(&regedit) {
        // SAFETY: `hwnd` live.
        unsafe {
            let _ = SetForegroundWindow(hwnd);
        }
        sleep(Duration::from_millis(300));
        send_ctrl_key(VK_F);
        out.push("found elevated regedit; sent Ctrl+F — confirm its Find dialog opened (harmless)".into());
        out.push(verdict(true));
    } else if let Some(hwnd) = find_window_class(&notepad) {
        // SAFETY: `hwnd` live.
        unsafe {
            let _ = SetForegroundWindow(hwnd);
        }
        sleep(Duration::from_millis(300));
        type_text("lumepeer-probe-2.8");
        out.push("regedit not found; typed into a Notepad window instead — confirm the text appeared".into());
        out.push(verdict(true));
    } else {
        out.push("no elevated regedit or Notepad window found — open regedit (as admin) and re-run".into());
        out.push(verdict(false));
    }
    out.push("VMware guest (System integrity): none running on this PC — deferred to pack 02 on `beta`".into());
    out
}

// ---- 2.9 -------------------------------------------------------------------

fn m_tray() -> Vec<String> {
    crate::tray::measure()
}

// ---- shared ----------------------------------------------------------------

fn verdict(pass: bool) -> String {
    format!("RESULT: {}", if pass { "YES" } else { "NO" })
}

fn wallclock() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("epoch+{}s", d.as_secs()))
        .unwrap_or_else(|_| "?".into())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_owned()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn read_lines(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|s| s.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

fn foreground_window() -> Option<HWND> {
    // SAFETY: plain query.
    let h = unsafe { GetForegroundWindow() };
    (!h.is_invalid()).then_some(h)
}

fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: `buf` is a live buffer; the call writes at most its length.
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn find_window_class(class_wide: &[u16]) -> Option<HWND> {
    // SAFETY: `class_wide` is a live null-terminated wide string.
    unsafe { FindWindowW(PCWSTR(class_wide.as_ptr()), PCWSTR::null()) }.ok()
}

fn wait_for_window(title_wide: &[u16], timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        // SAFETY: `title_wide` is a live null-terminated wide string.
        if unsafe { FindWindowW(PCWSTR::null(), PCWSTR(title_wide.as_ptr())) }.is_ok() {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        sleep(Duration::from_millis(150));
    }
}

/// Sends Ctrl+<vk> as a chord.
fn send_ctrl_key(vk: VIRTUAL_KEY) {
    let inputs = [
        key(VK_CONTROL, false),
        key(vk, false),
        key(vk, true),
        key(VK_CONTROL, true),
    ];
    // SAFETY: `inputs` is a live slice of the exact count passed.
    unsafe {
        SendInput(&inputs, size_of::<INPUT>() as i32);
    }
}

fn type_text(text: &str) {
    for ch in text.chars() {
        if let Some(vk) = ascii_vk(ch) {
            let inputs = [key(vk, false), key(vk, true)];
            // SAFETY: live slice of the exact count.
            unsafe {
                SendInput(&inputs, size_of::<INPUT>() as i32);
            }
        }
    }
}

fn ascii_vk(ch: char) -> Option<VIRTUAL_KEY> {
    match ch {
        'a'..='z' => Some(VIRTUAL_KEY(0x41 + (ch as u16 - 'a' as u16))),
        'A'..='Z' => Some(VIRTUAL_KEY(0x41 + (ch as u16 - 'A' as u16))),
        '0'..='9' => Some(VIRTUAL_KEY(0x30 + (ch as u16 - '0' as u16))),
        '-' => Some(VIRTUAL_KEY(0xBD)),
        '.' => Some(VIRTUAL_KEY(0xBE)),
        _ => None,
    }
}

fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
