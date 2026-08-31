#!/bin/sh
# %post (docs/bugs/12-service-lifecycle.md task 4; D6): rpm passes $1 = 1 on
# a first install and 2 (or higher) on an upgrade, because rpm installs the
# new package's %post before removing the old one. Only a first install
# should turn autostart on -- see deb-postinst.sh for why upgrades must
# leave it alone.
set -e

if [ "$1" != "1" ]; then
    exit 0
fi

# See deb-postinst.sh: autostart is per-user, so the write has to run as the
# person who will use the app, through the same `--enable-autostart` entry
# point the settings panel's toggle uses.
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
