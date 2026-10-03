#!/usr/bin/env bash
# Replaces libaom/ with another libaom release, trimmed exactly as
# VENDORED.md describes (ADR 0141).
#
#   ./vendor.sh 3.15.1 8ca0c52746174603500f0adb6f2a215d69c9ca2aab2acb3caa06fb791d8d01bf
set -euo pipefail

VERSION="${1:?usage: vendor.sh <version> <sha256>}"
SHA256="${2:?usage: vendor.sh <version> <sha256>}"
HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

TARBALL="libaom-${VERSION}.tar.gz"
curl -sSfL "https://storage.googleapis.com/aom-releases/${TARBALL}" -o "${WORK}/${TARBALL}"
echo "${SHA256}  ${WORK}/${TARBALL}" | sha256sum -c -
tar -xzf "${WORK}/${TARBALL}" -C "${WORK}"
SRC="${WORK}/libaom-${VERSION}"

rm -rf "${SRC}/doc" "${SRC}/aomedia_logo_200.png" \
  "${SRC}/third_party/googletest" "${SRC}/third_party/libyuv" \
  "${SRC}/third_party/libwebm" "${SRC}/third_party/highway" \
  "${SRC}/apps" "${SRC}/stats" \
  "${SRC}/tools/txfm_analyzer" "${SRC}/tools/auto_refactor" \
  "${SRC}/.gitattributes" "${SRC}/.gitignore"
rm -f "${SRC}"/common/webm*
find "${SRC}/test" \( -name '*.cc' -o -name '*.h' \) -delete
find "${SRC}" -name '*.py' -delete
find "${SRC}/examples" -type f ! -name 'encoder_util.*' ! -name 'multilayer_metadata.*' -delete

rm -rf "${HERE}/libaom"
mv "${SRC}" "${HERE}/libaom"
echo "libaom ${VERSION} vendored; update VENDORED.md and re-measure (ADR 0141)."
