#!/bin/sh
# Turns on per-user autostart for a fresh install (docs/bugs/
# 12-service-lifecycle.md task 4; D6) and installs the sign-in screen's
# supervisor unit (ADR 0151). Spliced by tauri-bundler into its own generated
# postinst and run with the arguments dpkg gives that script:
# `configure <most-recently-configured-version>`. The second argument is
# empty on a fresh install and set on an upgrade/reconfigure -- autostart
# must only be turned on the first time, never re-armed silently on every
# upgrade, or a person who turned it off from the settings panel would see it
# come back on its own the next time the package updates.
set -e

# The sign-in screen's supervisor (ADR 0151): a system service, run as root,
# that keeps lumepeer-desktop hosting the greeter whenever nobody is signed in
# and an account has enrolled its credentials. Installed and enabled on every
# configure, including upgrades, because it is a system unit and not a user
# choice -- it does nothing at all until an account opts in from the settings
# panel, so enabling it costs a once-a-second loginctl poll and no more. Only
# ever removed on a true uninstall (deb-prerm.sh).
install_logon_supervisor() {
    [ -d /run/systemd/system ] || return 0
    command -v systemctl >/dev/null 2>&1 || return 0
    cat > /lib/systemd/system/lumepeer-logon.service <<'UNIT'
[Unit]
Description=Lumepeer sign-in screen host supervisor (ADR 0151)
Documentation=https://github.com/insigmo/lumepeer
After=systemd-logind.service
Wants=systemd-logind.service

[Service]
Type=simple
ExecStart=/usr/bin/lumepeer-service --logon-supervisor
Restart=on-failure
RestartSec=5
RuntimeDirectory=lumepeer
RuntimeDirectoryMode=0755
StateDirectory=lumepeer
StateDirectoryMode=0755

[Install]
WantedBy=multi-user.target
UNIT
    systemctl daemon-reload || true
    systemctl enable --now lumepeer-logon.service || true
}

install_logon_supervisor

if [ -n "$2" ]; then
    # Upgrade or reconfigure: leave whatever the user currently has alone.
    exit 0
fi

# Autostart is a per-user mechanism (`autostart.rs`, ADR 0042): a file under
# that person's own home directory, written by the same app binary the
# settings panel's toggle calls into (`--enable-autostart`,
# `apps/desktop/src-tauri/src/main.rs`) so there is exactly one
# implementation of "how autostart is turned on", not a second one here.
# `postinst` runs as root with no session of its own, so the write has to
# happen as the person who will actually run the app -- best effort, and
# skipped rather than guessed at when no such person can be identified.
target_user="${SUDO_USER:-}"
if [ -z "$target_user" ] || [ "$target_user" = "root" ]; then
    target_user="$(logname 2>/dev/null || true)"
fi
if [ -z "$target_user" ] || [ "$target_user" = "root" ]; then
    echo "lumepeer: no non-root user found to enable autostart for; turn it on from the app's own settings instead" >&2
    exit 0
fi

su -l "$target_user" -c '/usr/bin/lumepeer-desktop --enable-autostart' || \
    echo "lumepeer: could not enable autostart for $target_user; turn it on from the app's own settings instead" >&2

exit 0
