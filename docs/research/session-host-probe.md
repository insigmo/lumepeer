# Session-host probe — pack 01 measurements (ADR 0169)

What a `LocalSystem` process the helper service launches into the **console
session on `WinSta0\Default`** can actually do — measured before any product
code, to decide whether [ADR 0169](../adr/0169-the-host-is-a-system-process-in-the-session-and-the-client-runs-unelevated.md)
holds. Pack: [`docs/unelevated-client/01-system-in-session-probe.md`](../unelevated-client/01-system-in-session-probe.md).

## How it was measured

A throwaway standalone crate `tools/session-probe` (not a workspace member, not
in any release build; removed after the run). It installs a temporary
`LocalSystem` service `LumepeerSessionProbe`, which — exactly as
`system_injector_launch.rs` does — duplicates its own token with
`duplicate_own_token_for_session`, stamps it for the console session, and
`CreateProcessAsUserW`s the probe again onto `WinSta0\Default` as the measuring
process. The DXGI capture (2.2) and WASAPI loopback (2.3) reuse the product's
own `lumepeer-media` backends, so a pass means the product's path works, not a
hand-rolled one. The service installed, launched the measure process into
session 1, waited for it, and removed itself; `sc query LumepeerSessionProbe`
reports error 1060 (gone) afterwards.

- **Host:** this PC — `DESKTOP-EFHI52Q`, console session 1, 3840×2160 primary
  display, Windows 11.
- **Date:** 2026-10-28.
- **Account:** `beta_win` (a UAC-filtered administrator — has a linked elevated
  token; `BUILTIN\Administrators` is deny-only in the limited token).

## Results

| # | Measurement | Result | How verified |
|---|---|---|---|
| 2.1 | Account / integrity / session of the probe | **YES** | Token user SID `S-1-5-18` (LocalSystem); integrity RID `0x4000` (System); session 1 = the active console session (`ProcessIdToSessionId` vs `WTSGetActiveConsoleSessionId`). |
| 2.2 | DXGI Desktop Duplication of `Default` | **YES** | Product capturer (`platform_capturer`) captured a 3840×2160 BGRA8 frame, mean brightness 83.7 → non-black (desktop windows present); saved to `session-probe-capture.png`. |
| 2.3 | WASAPI loopback of the default device | **YES** | Product capturer (`platform_audio_capturer`) over 200 chunks / 384000 samples with audio playing: peak \|s\| = 26722, RMS = 6537 → clearly non-zero. |
| 2.4 | Read and write the clipboard | **YES** | Read the console user's clipboard text; wrote a marker and read it back identically (`OpenClipboard` works on `WinSta0`). Paste: `Ctrl+V` via `SendInput` reached the foreground window (System→medium input allowed). |
| 2.5 | `TaskDialogIndirect` the user sees; a **medium** probe cannot press it | **YES** | The dialog appeared on `Default`. A medium-integrity child (launched with the user's limited token) tried `SendInput` + `PostMessage(BM_CLICK)` and the dialog stayed up (UIPI blocked it). The user's mouse then closed it (button id 1). |
| 2.6 | Start a shell as the **user** via `WTSQueryUserToken` | **YES** | `WTSQueryUserToken` + `CreateProcessAsUserW` ran `cmd /c whoami /all` (standing in for `lumepeer-terminal-worker`, same path): exit 0, user = `DESKTOP-EFHI52Q\beta_win`, token medium integrity, `Administrators` deny-only. |
| 2.7 | `TokenLinkedToken` impersonation reads an admin-only folder | **YES** | Impersonating the **elevated** linked token read `C:\ProgramData\Lumepeer\users` (admin-only); impersonating the **limited** token was DENIED — the expected contrast. |
| 2.8 | `SendInput` into an elevated window | **YES** (regedit) · VMware deferred | Found an elevated (High-integrity) `regedit` and sent a harmless `Ctrl+F` that opened its Find dialog — System→High input landed. No `VMware` guest (System integrity) runs on this PC, so that half is deferred to pack 02 on `beta`, as the pack allows. |
| 2.9 | `Shell_NotifyIcon` tray icon | **YES** (added) + design note | `NIM_ADD` succeeded (icon shown). Callbacks received from explorer = 0, as expected: explorer is medium, the probe window is System, so UIPI drops the messages without `ChangeWindowMessageFilterEx`. |

## Verdict

**ADR 0169 holds; pack 02 may proceed.** Every load-bearing measurement
(2.1–2.7) passed, and the two new risks the ADR introduces over ADR 0114/0126 —
DXGI capture of `Default` from `LocalSystem` (2.2) and WASAPI loopback from a
session-0-launched process (2.3) — both succeed with the product's own
backends. The session host can be the `LocalSystem` process on `Default` that
captures, injects, reaches the stores through the linked token, and starts the
user's shell.

### Notes for later packs (not blockers)

- **VMware (System integrity) input is unmeasured here** — no guest on this PC.
  ADR 0169 §consequences relies on the session host driving a System-integrity
  `VMware` window (the ADR 0114 injector did historically). Pack 02 must confirm
  it live on `beta`.
- **Tray clicks (2.9):** a `LocalSystem` host window does not receive tray
  callbacks from explorer without `ChangeWindowMessageFilterEx`. Whoever wires
  the host's own tray/host-bar either calls that, or the tray stays in the
  client (ADR 0169 §5 keeps the host bar in the client anyway).
- **Consent dialog manifest:** `TaskDialogIndirect` resolves only with a
  Common-Controls v6 manifest (without it the process dies at load with
  `STATUS_ENTRYPOINT_NOT_FOUND`). ADR 0169 §5 already assumes the session host
  carries that manifest — confirmed necessary.
