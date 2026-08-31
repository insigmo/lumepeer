#!/bin/sh
# Enables the per-user autostart entry for a deb/rpm install
# (docs/bugs/12-service-lifecycle.md #4; ADR 0042; DECISIONS.md D6).
#
# This does not write ~/.config/autostart/*.desktop itself: it drops
# privileges to the installing user and asks the app binary to do it, so
# there is exactly one place (autostart.rs) that knows that file's format.
# D6/ADR 0043 forbid a second autostart mechanism, and duplicating the file
# content here in shell would eventually drift from it. No root daemon is
# started anywhere -- this is the same per-user mechanism the settings panel
# already drives, only triggered from the package manager instead of a click.
#
# Finding "the installing user" from a root postinst script has no fully
# reliable answer -- dpkg/rpm do not pass one. This uses the one signal this
# project's own documented install path guarantees: sudo sets SUDO_USER, and
# install.sh always installs via `sudo dpkg -i`/`sudo dnf install`. Any other
# install path (a GUI package manager run directly as root, an unattended
# provisioning tool, ...) is not covered, and this script does nothing rather
# than guess at a user -- the same "an unreachable answer reads as nothing"
# rule autostart.rs already follows for an unreadable registry key or an
# unreachable home directory.
#
# Never fails the package install: a broken autostart wiring is not worth
# blocking `dpkg -i`/`rpm -i` over.
set -u

target_user="${SUDO_USER:-}"

if [ -z "${target_user}" ] || [ "${target_user}" = "root" ]; then
  exit 0
fi

if ! command -v lumepeer-desktop >/dev/null 2>&1; then
  exit 0
fi

su -s /bin/sh -c 'lumepeer-desktop --enable-autostart' "${target_user}" >/dev/null 2>&1 || true

exit 0
