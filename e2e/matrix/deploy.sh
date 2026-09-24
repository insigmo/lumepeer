#!/usr/bin/env bash
# Builds the pilot app for machines of the e2e matrix and puts it where
# hosts.toml expects it (e2e/matrix/README.md).
#
#     e2e/matrix/deploy.sh [win] [beta] [mac] [linux]      (default: all four)
#
# Run from Git Bash on the Windows machine. win and beta share one Windows
# build; linux is built in WSL Debian (the `debian` VM has no toolchain) and
# copied over; mac gets this working tree, uncommitted changes included, and
# builds there.
#
# Every build is a debug build with the `pilot` feature (the tauri-pilot
# bridge exists nowhere else), with the frontend embedded rather than pointed
# at the vite devUrl so view windows get their IPC, and with the CSP override
# of pilot_config.py.
set -euo pipefail
cd "$(dirname "$0")/../.."

BETA=${E2E_BETA_SSH:-bberb@beta}
MAC=${E2E_MAC_SSH:-betal@betals-mac}
LINUX=${E2E_LINUX_SSH:-beta@debian}
WIN_FEATURES=pilot,capture-windows,encode-mf,encode-openh264,decode-openh264
MAC_FEATURES=pilot,capture-screencapturekit,encode-videotoolbox,encode-openh264,decode-openh264,audio-capture-screencapturekit,audio-playout-coreaudio
SIDECARS="lumepeer-decoder-worker lumepeer-service lumepeer-terminal-worker"
PY=$(command -v python3 || command -v python)
mkdir -p target/e2e
"$PY" e2e/matrix/pilot_config.py > target/e2e/pilot.conf.json

# PowerShell on a Windows machine over ssh, without quoting through cmd.exe.
remote_ps() {
  local script="\$ProgressPreference = 'SilentlyContinue'; $2"
  ssh -o BatchMode=yes "$1" powershell -NoProfile -NonInteractive -EncodedCommand \
    "$(printf '%s' "$script" | iconv -f UTF-8 -t UTF-16LE | base64 -w0)"
}

# Stops a running e2e app on this machine: its exe cannot be overwritten.
stop_win_e2e() {
  powershell -NoProfile -NonInteractive -Command \
    "Get-CimInstance Win32_Process | Where-Object { \$_.ExecutablePath -like '*\\target\\e2e\\win\\*' } | ForEach-Object { Stop-Process -Id \$_.ProcessId -Force }"
}

win() {
  echo "== win: building"
  stop_win_e2e
  # audiopus_sys wants cmake; this machine's only one lives in a venv.
  local venv_cmake=/c/Users/beta_win/AppData/Local/hermes/hermes-agent/venv/Scripts/cmake.exe
  if ! command -v cmake >/dev/null && [ -x "$venv_cmake" ]; then export CMAKE=$venv_cmake; fi
  export CMAKE_POLICY_VERSION_MINIMUM=3.5
  cargo build $(printf -- '-p %s ' $SIDECARS)
  mkdir -p apps/desktop/src-tauri/binaries target/e2e/win
  for bin in $SIDECARS; do
    cp "target/debug/$bin.exe" "apps/desktop/src-tauri/binaries/$bin-x86_64-pc-windows-msvc.exe"
  done
  (cd apps/desktop && npx tauri build --debug --no-bundle --features "$WIN_FEATURES" --config ../../target/e2e/pilot.conf.json)
  for bin in lumepeer-desktop $SIDECARS; do cp "target/debug/$bin.exe" target/e2e/win/; done
  echo "   target/e2e/win/lumepeer-desktop.exe"
}

beta() {
  [ -f target/e2e/win/lumepeer-desktop.exe ] || win
  echo "== beta: copying the Windows build to $BETA"
  remote_ps "$BETA" '$d = "$env:LOCALAPPDATA\lumepeer-e2e\app"
    Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -like "$d\*" } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
    New-Item -ItemType Directory -Force $d | Out-Null'
  scp -q -o BatchMode=yes target/e2e/win/*.exe "$BETA:AppData/Local/lumepeer-e2e/app/"
  echo "   $BETA:AppData/Local/lumepeer-e2e/app/lumepeer-desktop.exe"
}

mac() {
  echo "== mac: syncing this tree to $MAC and building there"
  local index=.git/lumepeer-sync-index-e2e tree
  rm -f "$index"
  GIT_INDEX_FILE=$index git add -A
  tree=$(GIT_INDEX_FILE=$index git write-tree)
  rm -f "$index"
  git archive --format=tar "$tree" | ssh -o BatchMode=yes "$MAC" "mkdir -p ~/lumepeer && tar -xf - -C ~/lumepeer"
  scp -q -o BatchMode=yes target/e2e/pilot.conf.json "$MAC:lumepeer/target-e2e-pilot.conf.json"
  ssh -o BatchMode=yes "$MAC" "bash -s" <<EOF
set -euo pipefail
. ~/lumepeer-env.sh
export CMAKE_POLICY_VERSION_MINIMUM=3.5
cd ~/lumepeer
pkill -f "\$HOME/lumepeer/target/debug/bundle/macos/Lumepeer.app/Contents/MacOS" || true
triple=\$(rustc -vV | sed -n 's/^host: //p')
cargo build $(printf -- '-p %s ' $SIDECARS)
mkdir -p apps/desktop/src-tauri/binaries
for bin in $SIDECARS; do cp "target/debug/\$bin" "apps/desktop/src-tauri/binaries/\$bin-\$triple"; done
cd apps/desktop
npm install --no-audit --no-fund >/dev/null
npx tauri build --debug --bundles app --features "$MAC_FEATURES" --config ../../target-e2e-pilot.conf.json
EOF
  echo "   $MAC:lumepeer/target/debug/bundle/macos/Lumepeer.app"
  echo "   A rebuilt app is a new ad-hoc signature: grant it Screen Recording and"
  echo "   Accessibility again (System Settings > Privacy & Security) at the Mac."
}

linux() {
  echo "== linux: building in WSL Debian, copying to $LINUX"
  (cd apps/desktop && npm run build >/dev/null)
  MSYS_NO_PATHCONV=1 wsl -d Debian -u root -- bash e2e/matrix/build-linux.sh
  ssh -o BatchMode=yes "$LINUX" 'pkill -f "$HOME/.lumepeer-e2e/app/lumepeer-desktop" || true; mkdir -p ~/.lumepeer-e2e/app'
  scp -q -o BatchMode=yes target/e2e/linux/lumepeer-* "$LINUX:.lumepeer-e2e/app/"
  # Copied off NTFS, the files arrive without their execute bit.
  ssh -o BatchMode=yes "$LINUX" 'chmod +x ~/.lumepeer-e2e/app/lumepeer-*'
  echo "   $LINUX:.lumepeer-e2e/app/lumepeer-desktop"
}

for machine in ${*:-win beta mac linux}; do
  "$machine"
done
