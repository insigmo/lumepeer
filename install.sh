#!/usr/bin/env bash
# Install Lumepeer from the latest GitHub Release (native package).
set -uo pipefail

REPO="${LUMEPEER_REPO:-insigmo/lumepeer}"
VERSION="${LUMEPEER_VERSION:-}" # e.g. v0.0.3; empty = latest

usage() {
  cat <<EOF
Usage: install.sh [--version vX.Y.Z]

Downloads the Lumepeer installer matching this machine's OS/arch from the
latest GitHub Release (or a pinned version with --version) and installs it
with the system package manager (dpkg/apt or rpm/dnf/zypper on Linux, the
.dmg on macOS).

Env:
  LUMEPEER_REPO     GitHub repo (default: ${REPO})
  LUMEPEER_VERSION  Pin a release tag (default: latest)
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown arg: $1" >&2; usage; exit 1 ;;
  esac
done

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "Missing required command: $1" >&2
    exit 1
  }
}

need_cmd curl
need_cmd uname

# Resolve empty VERSION to the current GitHub "latest" release tag.
resolve_version() {
  if [ -n "${VERSION}" ]; then
    echo "${VERSION}"
    return
  fi
  local tag
  tag="$(
    curl -fsSL -H "Accept: application/vnd.github+json" \
      -H "Cache-Control: no-cache" \
      "https://api.github.com/repos/${REPO}/releases/latest" \
      | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
      | head -1
  )"
  if [ -z "${tag}" ]; then
    echo "Could not resolve latest release tag for ${REPO}" >&2
    exit 1
  fi
  echo "${tag}"
}

# Read a release's JSON (by tag) from stdin-independent fetch.
fetch_release_json() {
  local ver="$1"
  curl -fsSL -H "Accept: application/vnd.github+json" -H "Cache-Control: no-cache" \
    "https://api.github.com/repos/${REPO}/releases/tags/${ver}"
}

# Print the browser_download_url of the first asset matching a regex, reading
# a release's JSON from stdin (relies on GitHub's API returning one field per
# line, same assumption resolve_version makes on tag_name).
asset_url() {
  local pattern="$1"
  grep -o '"browser_download_url"[[:space:]]*:[[:space:]]*"[^"]*"' \
    | sed -n 's/.*"\(https:[^"]*\)"/\1/p' \
    | grep -E "${pattern}" \
    | head -1
}

download_asset() {
  local url="$1" dest="$2"
  echo "Downloading $(basename "${url}")…"
  curl -fL --progress-bar -H "Cache-Control: no-cache" -o "${dest}" "${url}"
}

install_linux() {
  local arch deb_pat rpm_pat
  arch="$(uname -m)"
  case "${arch}" in
    x86_64|amd64) deb_pat='_amd64\.deb$'; rpm_pat='\.x86_64\.rpm$' ;;
    aarch64|arm64) deb_pat='_arm64\.deb$'; rpm_pat='\.aarch64\.rpm$' ;;
    *) echo "Unsupported Linux arch: ${arch}" >&2; exit 1 ;;
  esac

  local ver release_json pkg url tmp file
  ver="$(resolve_version)"
  release_json="$(fetch_release_json "${ver}")"

  if command -v dpkg >/dev/null 2>&1; then
    pkg="deb"
    url="$(printf '%s' "${release_json}" | asset_url "${deb_pat}")"
  elif command -v rpm >/dev/null 2>&1; then
    pkg="rpm"
    url="$(printf '%s' "${release_json}" | asset_url "${rpm_pat}")"
  else
    echo "Neither dpkg nor rpm found — cannot determine a native package for this distro." >&2
    exit 1
  fi

  if [ -z "${url}" ]; then
    echo "No .${pkg} asset found for ${ver} / ${arch} in ${REPO} releases." >&2
    echo "Publish a GitHub Release (tag v*) so /releases/latest has assets." >&2
    exit 1
  fi

  tmp="$(mktemp -d)"
  trap 'rm -rf "'"${tmp}"'"' EXIT
  file="${tmp}/lumepeer.${pkg}"
  download_asset "${url}" "${file}"

  echo "Installing Lumepeer ${ver} (may ask for sudo)…"
  if [ "${pkg}" = "deb" ]; then
    sudo dpkg -i "${file}" || sudo apt-get install -f -y
  else
    if command -v dnf >/dev/null 2>&1; then
      sudo dnf install -y "${file}"
    elif command -v zypper >/dev/null 2>&1; then
      sudo zypper --non-interactive install "${file}"
    else
      sudo rpm -Uvh "${file}"
    fi
  fi

  echo
  echo "Installed Lumepeer ${ver}."
  echo "Launch it from your applications menu, or run: lumepeer-desktop"
}

install_macos() {
  local arch dmg_pat
  arch="$(uname -m)"
  case "${arch}" in
    arm64) dmg_pat='\.dmg$' ;;
    *) echo "No macOS build for arch '${arch}' yet (Apple Silicon only)." >&2; exit 1 ;;
  esac

  need_cmd hdiutil

  local ver release_json url tmp file mount_point app_path app_name
  ver="$(resolve_version)"
  release_json="$(fetch_release_json "${ver}")"
  url="$(printf '%s' "${release_json}" | asset_url "${dmg_pat}")"
  if [ -z "${url}" ]; then
    echo "No .dmg asset found for ${ver} in ${REPO} releases." >&2
    exit 1
  fi

  tmp="$(mktemp -d)"
  trap 'rm -rf "'"${tmp}"'"' EXIT
  file="${tmp}/lumepeer.dmg"
  download_asset "${url}" "${file}"

  mount_point="$(mktemp -d)"
  hdiutil attach "${file}" -nobrowse -quiet -mountpoint "${mount_point}"
  app_path="$(find "${mount_point}" -maxdepth 1 -name '*.app' | head -1)"
  if [ -z "${app_path}" ]; then
    hdiutil detach "${mount_point}" -quiet || true
    echo "No .app found inside the disk image" >&2
    exit 1
  fi
  app_name="$(basename "${app_path}")"

  echo "Installing ${app_name} to /Applications…"
  rm -rf "/Applications/${app_name}"
  cp -R "${app_path}" /Applications/
  hdiutil detach "${mount_point}" -quiet

  echo
  echo "Installed Lumepeer ${ver} to /Applications/${app_name}."
  echo "Unsigned build — first launch: right-click the app → Open, to bypass Gatekeeper."
}

os="$(uname -s | tr '[:upper:]' '[:lower:]')"
case "${os}" in
  linux) install_linux ;;
  darwin) install_macos ;;
  mingw*|msys*|cygwin*)
    echo "On Windows use install.ps1 instead of install.sh" >&2
    exit 1
    ;;
  *)
    echo "Unsupported OS: ${os}" >&2
    exit 1
    ;;
esac
