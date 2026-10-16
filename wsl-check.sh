#!/bin/bash
# Temporary: Linux clippy + tests for the dependency bump (not committed).
export PATH=/root/.cargo/bin:$PATH
export CARGO_TARGET_DIR=/root/lp-target CARGO_INCREMENTAL=0
cd /mnt/c/Users/beta_win/git/lumepeer-deps || exit 1
step() { echo "=== $*"; "$@" 2>&1 | grep -E ": (error|warning)|^error|^test result|FAILED|panicked|^test .* \.\.\. FAILED" | sort | uniq -c | head -60; echo "exit ${PIPESTATUS[0]}"; }
step cargo clippy --workspace --all-targets --keep-going --message-format short
step cargo clippy -p lumepeer-media --all-targets --features capture-x11,capture-portal,encode-vaapi-zero-copy,encode-aom,decode-aom,audio-opus,audio-capture-pipewire --message-format short
step cargo clippy -p lumepeer-net --all-targets --features secret-service --message-format short
step cargo clippy -p lumepeer-desktop --all-targets --features capture-x11,capture-portal,encode-vaapi-zero-copy,encode-aom,pilot --keep-going --message-format short
step cargo +nightly check --manifest-path tests/fuzz/Cargo.toml
# The CI media job's test steps.
step cargo test -p lumepeer-media --features encode-aom,decode-aom --lib -- encode:: decode::aom
step cargo test -p lumepeer-runtime --features encode-aom --lib
step xvfb-run -a cargo test -p lumepeer-integration-tests --test media_pipeline
step xvfb-run -a cargo test -p lumepeer-integration-tests --test error_matrix
step dbus-run-session -- cargo test -p lumepeer-net --features secret-service
step cargo test -p lumepeer-core -p lumepeer-terminal -p lumepeer-broker -p lumepeer-service
echo ALLDONE
