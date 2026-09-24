"""Prints the Tauri config override of an e2e build (see deploy.sh).

tauri-pilot's `eval` compiles every script with `new Function`, which WebKit
(macOS, Linux) refuses under the app's CSP. The e2e build gets the same CSP
with 'unsafe-eval' added to script-src, derived from tauri.conf.json so the
two cannot drift apart; the shipped config is never touched.

It also builds no updater archive: signing one needs the release key, and
without it `tauri build` fails after the app bundle is already made.
"""

import json
from pathlib import Path

conf = Path(__file__).resolve().parents[2] / "apps" / "desktop" / "src-tauri" / "tauri.conf.json"
csp = json.loads(conf.read_text(encoding="utf-8"))["app"]["security"]["csp"]
assert "script-src 'self'" in csp, csp
csp = csp.replace("script-src 'self'", "script-src 'self' 'unsafe-eval'", 1)
print(json.dumps({"app": {"security": {"csp": csp}}, "bundle": {"createUpdaterArtifacts": False}}))
