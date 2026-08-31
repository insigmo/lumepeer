; Registers/unregisters the Ctrl+Alt+Del helper service from the installer
; and uninstaller (ADR 0043; docs/bugs/12-service-lifecycle.md, task 1).
;
; The service binary is staged next to the app as a Tauri `externalBin`
; sidecar (`bundle.externalBin` in tauri.conf.json), so it sits at
; "$INSTDIR\lumepeer-service.exe" on both the installer and uninstaller side
; -- the same path `service_control.rs::service_exe()` derives at runtime
; from `current_exe()`.
;
; `installMode: perMachine` (tauri.conf.json) makes the installer and
; uninstaller both run elevated already, so calling `--install`/`--uninstall`
; here needs no second UAC prompt: the elevation the user already granted the
; installer covers the service-control-manager calls those flags make in
; `crates/service/src/install.rs`.
;
; Both calls are best-effort. A service that fails to (un)install here must
; not fail the whole (un)installation: the settings panel's manual
; Install/Remove button (`service_control.rs`) is always the fallback D6
; keeps in place, exactly the way a missing service already degrades the
; Ctrl+Alt+Del feature rather than the app (ADR 0043).

!macro NSIS_HOOK_POSTINSTALL
  ; Runs after files are copied, so the sidecar is already at $INSTDIR.
  ExecWait '"$INSTDIR\lumepeer-service.exe" --install'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Runs before files are removed, so the sidecar is still there to call.
  ExecWait '"$INSTDIR\lumepeer-service.exe" --uninstall'
!macroend
