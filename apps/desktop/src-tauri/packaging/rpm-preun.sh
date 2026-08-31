#!/bin/sh
# %preun (docs/bugs/12-service-lifecycle.md task 4; D6): rpm passes $1 = 0
# when this is the last copy being removed, and 1 when the old package's
# %preun runs as part of an upgrade to a new one. Only a true removal should
# turn autostart off -- see deb-prerm.sh for why an upgrade must leave it
# alone.
set -e

if [ "$1" != "0" ]; then
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
