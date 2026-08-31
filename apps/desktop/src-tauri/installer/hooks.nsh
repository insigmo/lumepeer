; Installs and removes the Lumepeer helper service alongside the app itself
; (docs/bugs/12-service-lifecycle.md #1; ADR 0043; DECISIONS.md D6).
;
; The service's only purpose is Ctrl+Alt+Del delivery (ADR 0043). Before this
; hook existed, uninstalling the app left `LumepeerHelper` registered with the
; service control manager forever -- a defect on its own, per D6.
;
; `installMode: perMachine` in tauri.conf.json means both the installer and
; the generated uninstaller already run elevated, so no second UAC prompt is
; needed here. `--install`/`--uninstall` are flags on the sidecar binary
; itself (crates/service), never a command line built out of a path and
; handed to a shell -- see `service_control.rs` and ADR 0043 for why that
; matters. `$INSTDIR\lumepeer-service.exe` is the only path named here, and it
; is a fixed literal, not something assembled at run time.
;
; Uninstall runs in NSIS_HOOK_PREUNINSTALL, before any files are removed, so
; the sidecar this hook calls is still on disk. Install runs in
; NSIS_HOOK_POSTINSTALL, after files are copied, so it exists by then too.
; Both operations are idempotent (crates/service/src/install.rs), so running
; this hook again on an update cannot fail because of what an earlier run
; already did.

!macro NSIS_HOOK_POSTINSTALL
  IfFileExists "$INSTDIR\lumepeer-service.exe" lumepeer_install_service lumepeer_skip_install_service
  lumepeer_install_service:
    ExecWait '"$INSTDIR\lumepeer-service.exe" --install'
  lumepeer_skip_install_service:
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  IfFileExists "$INSTDIR\lumepeer-service.exe" lumepeer_uninstall_service lumepeer_skip_uninstall_service
  lumepeer_uninstall_service:
    ExecWait '"$INSTDIR\lumepeer-service.exe" --uninstall'
  lumepeer_skip_uninstall_service:
!macroend
