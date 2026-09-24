"""Fixtures and the report of the lumepeer e2e matrix (e2e/matrix/README.md).

`machines` (session scope) brings every machine up: its agent over ssh, the
pilot build of the app, the tracker in the app's main window. `session`
(one per [guest, host] pair) connects the two through lumepeer and hands the
tests a live session. The summary at the end is the part meant to be pasted
into another session: one line per machine, one line per pair and scenario.
"""

import datetime
import os
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

import pytest

sys.path.insert(0, os.path.dirname(__file__))

import harness  # noqa: E402
from harness import OUT, REPO, Host, HostError, Refused, connect, disconnect, load_config, log  # noqa: E402

CONFIG = load_config(os.environ.get("LUMEPEER_E2E_HOSTS"))
PAIRS = [tuple(p) for p in CONFIG["matrix"]["pairs"]]
RESULTS = []
MACHINES = {}


def pytest_configure(config):
    harness.REPORTER = config.pluginmanager.getplugin("terminalreporter")


def pytest_addoption(parser):
    parser.addoption("--only", default="", help="comma-separated machines to use; pairs with any other are skipped")
    parser.addoption("--keep", action="store_true", help="leave the apps running after the run")


def short(error):
    text = str(error).strip().splitlines()
    return (text[-1] if text else type(error).__name__)[:200]


@pytest.fixture(scope="session")
def machines(request):
    only = {n for n in request.config.getoption("--only").split(",") if n}
    names = sorted({n for pair in PAIRS for n in pair if not only or n in only})
    OUT.mkdir(parents=True, exist_ok=True)

    def up(name):
        machine = Host(name, CONFIG["hosts"][name])
        log(f"{name}: starting the app")
        try:
            machine.start(CONFIG["matrix"].get("rust_log", "info"))
        except (HostError, Refused, OSError) as error:
            machine.down = short(error)
            log(f"{name}: DOWN - {machine.down}")
        return machine

    with ThreadPoolExecutor(len(names) or 1) as pool:
        MACHINES.update({m.name: m for m in pool.map(up, names)})
    yield MACHINES
    for machine in MACHINES.values():
        if machine.agent and not machine.down:
            lines = machine.log_lines(limit=1_000_000)
            (OUT / f"{machine.name}.log").write_text("\n".join(lines), encoding="utf-8")
        if not request.config.getoption("--keep"):
            machine.stop()
        elif machine.agent:
            machine.agent.close()


@pytest.fixture(scope="module", params=PAIRS, ids=[f"{g}_to_{h}" for g, h in PAIRS])
def session(request, machines):
    guest, host = request.param
    for name in (guest, host):
        if name not in machines:
            pytest.skip(f"{name} not selected")
        if machines[name].down:
            pytest.skip(f"{name} is down")
    matrix = CONFIG["matrix"]
    for name in (guest, host):
        try:
            machines[name].ensure_healthy(matrix.get("rust_log", "info"))
        except (HostError, Refused, OSError) as error:
            machines[name].down = short(error)
            pytest.skip(f"{name} is down after a restart")
    timeout = matrix["wayland_picture_timeout"] if machines[host].wayland else matrix["picture_timeout"]
    log(f"{guest} -> {host}: connecting")
    s = connect(machines[guest], machines[host], timeout)
    yield s
    disconnect(s)


# ── the report ──────────────────────────────────────────────────────────────


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_makereport(item, call):
    outcome = yield
    rep = outcome.get_result()
    if rep.when == "call" or (rep.when == "setup" and not rep.passed):
        pair = item.callspec.id.replace("_to_", "->") if hasattr(item, "callspec") else ""
        name = item.originalname.removeprefix("test_")
        if rep.passed:
            status, text = "PASS", dict(item.user_properties).get("note", "")
        elif rep.skipped:
            status, text = "SKIP", rep.longrepr[2].removeprefix("Skipped: ")
        else:
            status = "FAIL" if rep.when == "call" else "ERROR"
            crash = getattr(rep.longrepr, "reprcrash", None)
            text = crash.message if crash else str(rep.longrepr)
            text = text.removeprefix("Failed: ")
        RESULTS.append((pair, name, status, " ".join(text.split())))


def build_id():
    def git(*args):
        return subprocess.run(["git", *args], cwd=REPO, capture_output=True, text=True).stdout.strip()

    return git("rev-parse", "--short", "HEAD") + ("+dirty" if git("status", "--porcelain") else "")


def pytest_terminal_summary(terminalreporter):
    if not RESULTS and not MACHINES:
        return
    stamp = datetime.datetime.now().strftime("%Y-%m-%d %H:%M")
    lines = [f"== lumepeer e2e matrix | {stamp} | {build_id()} =="]
    lines += ["hosts: " + " ".join(m.describe() for m in MACHINES.values())]
    lines += [f"{pair:<12} {name:<8} {status:<5} {text}" for pair, name, status, text in RESULTS]
    lines += [f"app logs: {OUT.relative_to(REPO).as_posix()}/<host>.log   guest->host = guest controls host"]
    report = "\n".join(lines)
    (OUT / "report.txt").write_text(report + "\n", encoding="utf-8")
    terminalreporter.write_line("")
    for line in lines:
        terminalreporter.write_line(line)
