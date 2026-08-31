#!/bin/sh
# Turns off per-user autostart before the package is removed (docs/bugs/
# 12-service-lifecycle.md task 4; D6). Spliced by tauri-bundler into its own
# generated prerm, called as `remove` for an actual uninstall and as
# `upgrade <new-version>` right before dpkg unpacks a new version over this
# one -- only a real removal should touch autostart, or every upgrade would
# briefly turn it off only for the new postinst to turn it back on, and worse,
# would clobber a user's own "off" if that upgrade's postinst were ever
# changed to no longer distinguish fresh installs from upgrades.
set -e

if [ "$1" != "remove" ]; then
    exit 0
fi

target_user="${SUDO_USER:-}"
if [ -z "$target_user" ] || [ "$target_user" = "root" ]; then
    target_user="$(logname 2>/dev/null || true)"
fi
if [ -z "$target_user" ] || [ "$target_user" = "root" ]; then
    exit 0
fi

su -l "$target_user" -c '/usr/bin/lumepeer-desktop --disable-autostart' || true

exit 0
