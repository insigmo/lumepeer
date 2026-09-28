#!/usr/bin/env bash
# Fails unless a macOS bundle is signed the way a Screen Recording grant can
# outlive an update (ADR 0128).
#
#     bash ci/check-macos-signature.sh path/to/Lumepeer.app
#
# The main executable and every sidecar in Contents/MacOS must carry a valid
# signature by the same certificate, and the bundle's designated requirement
# must name that certificate. A requirement that is only a code hash (what an
# ad-hoc or linker signature gets) changes with every build, which is exactly
# the grant that was lost on each update.
set -euo pipefail

APP="${1:?usage: check-macos-signature.sh path/to/Lumepeer.app}"
if [[ ! -d "${APP}" ]]; then
  echo "::error::no bundle at ${APP}" >&2
  exit 1
fi

codesign --verify --deep --strict --verbose=2 "${APP}"

REQUIREMENT="$(codesign -d -r- "${APP}" 2>&1 | sed -n 's/^designated => //p')"
echo "designated requirement: ${REQUIREMENT}"
if [[ "${REQUIREMENT}" != *'identifier "io.insigmo.lumepeer"'* || "${REQUIREMENT}" != *'certificate'* ]]; then
  echo "::error::${APP} is not signed by the release certificate; a grant would not survive the next update" >&2
  exit 1
fi

AUTHORITY=
for BIN in "${APP}"/Contents/MacOS/*; do
  INFO="$(codesign -dv --verbose=2 "${BIN}" 2>&1)"
  THIS="$(printf '%s\n' "${INFO}" | sed -n 's/^Authority=//p' | head -1)"
  FLAGS="$(printf '%s\n' "${INFO}" | sed -n 's/^CodeDirectory .*flags=\([^ ]*\).*/\1/p')"
  echo "$(basename "${BIN}"): authority=${THIS:-none} flags=${FLAGS}"
  if [[ -z "${THIS}" ]]; then
    echo "::error::$(basename "${BIN}") carries no certificate signature" >&2
    exit 1
  fi
  if [[ -n "${AUTHORITY}" && "${THIS}" != "${AUTHORITY}" ]]; then
    echo "::error::$(basename "${BIN}") is signed by ${THIS}, the bundle by ${AUTHORITY}" >&2
    exit 1
  fi
  AUTHORITY="${THIS}"
done
