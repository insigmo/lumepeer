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

; Stops the helper service and waits for its process to actually exit.
;
; Reinstalling over a running service is what NSIS reports as "Error opening
; file for writing: ...\lumepeer-service.exe": the service control manager
; holds the image open for as long as the process lives, and elevation does
; not lift that lock -- only stopping the service does. `sc stop` merely
; *requests* the stop and returns immediately, so a poll on the process is the
; part that makes this reliable; `KillProcess` is the backstop for a service
; that refuses to drain within the timeout.
;
; `UNIQ` only exists to keep the labels unique between the installer and
; uninstaller copies of this macro.
!macro LUMEPEER_STOP_SERVICE UNIQ
  Push $R7
  Push $R8

  DetailPrint "Stopping the Lumepeer helper service..."
  nsExec::Exec '"$SYSDIR\sc.exe" stop "LumepeerHelper"'
  Pop $R7 ; discarded: not-installed and already-stopped are both fine here

  ; Poll for up to ~10s: FindProcess pushes 0 while the process is still alive.
  StrCpy $R8 0
  lumepeer_wait_${UNIQ}:
    nsis_tauri_utils::FindProcess "lumepeer-service.exe"
    Pop $R7
    StrCmp $R7 0 0 lumepeer_stopped_${UNIQ}
    IntCmp $R8 20 lumepeer_kill_${UNIQ}
    Sleep 500
    IntOp $R8 $R8 + 1
    Goto lumepeer_wait_${UNIQ}

  lumepeer_kill_${UNIQ}:
    DetailPrint "Lumepeer helper service did not stop; terminating it."
    nsis_tauri_utils::KillProcess "lumepeer-service.exe"
    Pop $R7
    Sleep 1000

  lumepeer_stopped_${UNIQ}:
  Pop $R8
  Pop $R7
!macroend

!macro NSIS_HOOK_PREINSTALL
  ; Runs before files are copied, while an older install's service may still
  ; be running and holding "$INSTDIR\lumepeer-service.exe" open.
  !insertmacro LUMEPEER_STOP_SERVICE "install"
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Runs after files are copied, so the sidecar is already at $INSTDIR.
  ExecWait '"$INSTDIR\lumepeer-service.exe" --install'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Runs before files are removed, so the sidecar is still there to call.
  ExecWait '"$INSTDIR\lumepeer-service.exe" --uninstall'
  ; `--uninstall` stops the service asynchronously too, so wait the same way
  ; before the uninstaller starts deleting $INSTDIR.
  !insertmacro LUMEPEER_STOP_SERVICE "uninstall"
!macroend
