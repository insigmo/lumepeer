#!/bin/sh
# Disables the per-user autostart entry for a deb/rpm removal
# (docs/bugs/12-service-lifecycle.md #4; ADR 0042; DECISIONS.md D6).
#
# Mirrors postinst.sh -- see that file for why this shells out to the app
# binary instead of writing the autostart file itself, and why SUDO_USER is
# the only signal used to find the target user.
#
# Runs before the package's files are removed (preRemoveScript/%preun), while
# lumepeer-desktop is still on disk to be called. Never fails the removal.
set -u

target_user="${SUDO_USER:-}"

if [ -z "${target_user}" ] || [ "${target_user}" = "root" ]; then
  exit 0
fi

if ! command -v lumepeer-desktop >/dev/null 2>&1; then
  exit 0
fi

su -s /bin/sh -c 'lumepeer-desktop --disable-autostart' "${target_user}" >/dev/null 2>&1 || true

exit 0
