#!/usr/bin/env bash
# Puts the macOS release signing identity where codesign finds it and hands it
# to the Tauri build (ADR 0127).
#
#     P12=<base64 .p12> P12_PASSWORD=<password> bash ci/import-macos-signing-cert.sh
#
# The certificate is self-signed and fixed: what matters is that every release
# is signed by the same one, because macOS files a Screen Recording or
# Accessibility grant against the bundle's designated requirement (identifier
# + certificate). tauri-macos-sign imports APPLE_CERTIFICATE only when the
# certificate is named like one of Apple's, so the keychain is set up here and
# Tauri gets the identity by hash through APPLE_SIGNING_IDENTITY instead.
set -euo pipefail

if [[ -z "${P12:-}" || -z "${P12_PASSWORD:-}" ]]; then
  echo "::error::MACOS_SIGNING_P12 / MACOS_SIGNING_P12_PASSWORD are not set; an unsigned Mac build loses its Screen Recording grant on every update (ADR 0127)" >&2
  exit 1
fi

SCRATCH="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
KEYCHAIN="${SCRATCH}/lumepeer-signing.keychain-db"
KEYCHAIN_PASSWORD="$(openssl rand -hex 16)"
# `security import` goes by the file name unless told the format, and a name
# without .p12 is "Unknown format in import". Both, to be sure.
P12_FILE="${SCRATCH}/lumepeer-signing.p12"
trap 'rm -f "${P12_FILE}"' EXIT

printf '%s' "${P12}" | tr -d ' \r\n' | base64 --decode > "${P12_FILE}"
echo "certificate bundle: $(wc -c < "${P12_FILE}" | tr -d ' ') bytes"
security create-keychain -p "${KEYCHAIN_PASSWORD}" "${KEYCHAIN}"
security set-keychain-settings -lut 21600 "${KEYCHAIN}"
security unlock-keychain -p "${KEYCHAIN_PASSWORD}" "${KEYCHAIN}"
security import "${P12_FILE}" -f pkcs12 -k "${KEYCHAIN}" -P "${P12_PASSWORD}" -T /usr/bin/codesign
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "${KEYCHAIN_PASSWORD}" "${KEYCHAIN}" >/dev/null

# codesign looks for an identity only in the search list.
EXISTING=()
while IFS= read -r LINE; do
  LINE="${LINE#"${LINE%%[![:space:]]*}"}"
  LINE="${LINE//\"/}"
  [[ -n "${LINE}" ]] && EXISTING+=("${LINE}")
done < <(security list-keychains -d user)
security list-keychains -d user -s "${KEYCHAIN}" "${EXISTING[@]}"

# Self-signed means untrusted, so `find-identity -v` would list nothing.
IDENTITY="$(security find-identity -p codesigning "${KEYCHAIN}" | sed -n 's/^ *1) \([0-9A-F]\{40\}\) .*/\1/p')"
if [[ -z "${IDENTITY}" ]]; then
  echo "::error::MACOS_SIGNING_P12 holds no code-signing identity" >&2
  security find-identity -p codesigning "${KEYCHAIN}" >&2
  exit 1
fi

echo "Signing identity ${IDENTITY}"
if [[ -n "${GITHUB_ENV:-}" ]]; then
  echo "APPLE_SIGNING_IDENTITY=${IDENTITY}" >> "${GITHUB_ENV}"
fi
