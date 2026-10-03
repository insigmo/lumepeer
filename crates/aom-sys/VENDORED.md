# libaom, vendored (ADR 0141)

`libaom/` is **libaom 3.15.1**, the release the stage-1 measurement ran
(`docs/research/software-av1.md`), taken from the official release tarball:

- URL: <https://storage.googleapis.com/aom-releases/libaom-3.15.1.tar.gz>
- SHA-256: `8ca0c52746174603500f0adb6f2a215d69c9ca2aab2acb3caa06fb791d8d01bf`

Unchanged except for what was left out. Nothing the encoder library is built
from was removed; what went is what only libaom's own tests, documentation,
example programs and command-line applications use:

| removed | why it is not needed |
|---|---|
| `doc/`, `aomedia_logo_200.png` | documentation (`ENABLE_DOCS=0`) |
| `test/*.cc`, `test/*.h` | unit tests (`ENABLE_TESTS=0`); `test/*.cmake` stays, the top `CMakeLists.txt` includes it |
| `third_party/googletest/` | the unit tests' framework |
| `examples/` except `encoder_util.*` and `multilayer_metadata.*` | example programs (`ENABLE_EXAMPLES=0`); the two kept are named in `CMakeLists.txt` |
| `apps/`, `stats/`, `common/webm*` | `aomenc`/`aomdec` (`ENABLE_APPS=0`) |
| `third_party/libyuv/`, `third_party/libwebm/` | used by the applications only (`CONFIG_LIBYUV=0`, `CONFIG_WEBM_IO=0`) |
| `third_party/highway/` | `CONFIG_HIGHWAY=0`, libaom's default |
| `tools/txfm_analyzer/`, `tools/auto_refactor/`, every `*.py` | developer tools |
| `.gitattributes`, `.gitignore` | the upstream repository's own |

The trimmed tree, built as `build.rs` builds it, encodes byte for byte what the
whole release built realtime-only does (checked on a 60-frame 1080p screen
capture at speed 10, the stage-1 shim's settings).

To move to another release: run `./vendor.sh <version> <sha256>` from this
directory, then build, run `cargo test -p lumepeer-media --features
encode-aom --lib encode::` and re-measure (`codec-bench`'s `lumepeer-aom`
variant) before anything ships with it. The numbers ADR 0141 relies on are
this release's.
