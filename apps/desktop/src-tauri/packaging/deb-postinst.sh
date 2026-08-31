#!/bin/sh
# Turns on per-user autostart for a fresh install (docs/bugs/
# 12-service-lifecycle.md task 4; D6). Spliced by tauri-bundler into its own
# generated postinst and run with the arguments dpkg gives that script:
# `configure <most-recently-configured-version>`. The second argument is
# empty on a fresh install and set on an upgrade/reconfigure -- autostart
# must only be turned on the first time, never re-armed silently on every
# upgrade, or a person who turned it off from the settings panel would see it
# come back on its own the next time the package updates.
set -e

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
