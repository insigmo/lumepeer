"""Sound of the lumepeer e2e matrix, for every [guest, host] pair (ADR 0147).

1. sound: the host plays an 880 Hz tone on its default output for three
   seconds. The guest has to receive it, decode it as an 880 Hz tone and hand
   it to its own speakers. The guest counts what it got where it plays it
   (`connection_stats` `audio_in`), so a failure says which stage lost it: the
   host's capture (its own `audio_out` heard no tone), the stream (the guest
   received nothing), or the guest's playback device (decoded, never played).

2. mic: the guest turns its microphone on, as the view's mic button does.
   The host has to receive the stream and play it on its speakers, and what
   it plays must not be digital silence, which is what a muted microphone or
   one the OS keeps from the app delivers. The guest's real microphone is
   used, so a guest machine without one fails with the reason, and so does a
   guest whose build has no microphone capture (macOS).

Both run on a session of their own, connected the way test_matrix.py's are,
so `python -m pytest e2e/matrix/test_audio.py` checks sound alone.
"""

import json
import time

import pytest

from harness import IPC_JS, HostError, Refused

TONE_HZ = 880
TONE_SECS = 3.0
# Half a second of the tone, in 20 ms chunks at -30 dBFS or louder.
HEARD = 25
# A second of microphone, in 20 ms chunks.
MIC_CHUNKS = 50


def fail(msg):
    pytest.fail(msg, pytrace=False)


def note(request, text):
    request.node.user_properties.append(("note", text))


def media(session):
    """The session, when it got as far as a picture: sound rides the same
    media connection, which a session without a picture may not have."""
    if session.connect_error:
        fail(f"no session: {session.connect_error}")
    if session.picture_error:
        fail(f"no media connection to carry sound ({session.picture_error})")
    return session


def host_label(s):
    """The host as the guest names it: the view window is `view-<label>`."""
    return s.view.removeprefix("view-")


def audio(machine, peer):
    """`(audio_in, audio_out)` of `machine`'s connection to `peer`."""
    try:
        rows = machine.ipc("connection_stats") or []
    except (HostError, Refused):
        return {}, {}
    row = next((r for r in rows if r.get("peer_label") == peer), {})
    return row.get("audio_in") or {}, row.get("audio_out") or {}


def gained(after, before, key):
    return (after.get(key) or 0) - (before.get(key) or 0)


def describe(m, before):
    if not m:
        return "nothing (no media session)"
    device = m.get("device")
    if m.get("device_error"):
        device += f" ({m['device_error'][:120]})"
    hz = f" {m['loud_hz']}Hz" if m.get("loud_hz") else ""
    return (f"streams={m.get('streams')} chunks=+{gained(m, before, 'chunks')} "
            f"nonzero=+{gained(m, before, 'nonzero')} loud=+{gained(m, before, 'loud')}{hz} "
            f"played=+{gained(m, before, 'played')} dropped=+{gained(m, before, 'dropped')} "
            f"peak={m.get('peak')} device={device}")


def wait_for(check, timeout):
    deadline = time.monotonic() + timeout
    while True:
        value = check()
        if value or time.monotonic() > deadline:
            return value
        time.sleep(0.5)


# ── 1 ───────────────────────────────────────────────────────────────────────


def test_sound(session, request):
    s = media(session)
    g, h, label = s.guest, s.host, host_label(s)

    # Sound starts with the picture (ADR 0137): the host's stream should be
    # there by now, or within a few seconds of it.
    if not wait_for(lambda: (audio(g, label)[0].get("streams") or 0) >= 1, 10):
        _, sent = audio(h, s.peer)
        fail(f"the host's sound stream never reached the guest | host sending: {describe(sent, {})}")

    g_before, _ = audio(g, label)
    _, h_before = audio(h, s.peer)
    try:
        played = h.agent.call("play_tone", timeout=60, hz=TONE_HZ, secs=TONE_SECS)
    except HostError as error:
        fail(f"{h.name} could not play a tone on its own speakers: {error}")

    heard = wait_for(lambda: gained(audio(g, label)[0], g_before, "loud") >= HEARD, 6)
    g_after, _ = audio(g, label)
    _, h_after = audio(h, s.peer)
    where = (f"{h.name} played via {played['player']} in {played['secs']}s | host captured: "
             f"{describe(h_after, h_before)} | guest received: {describe(g_after, g_before)}")

    if gained(h_after, h_before, "loud") < HEARD:
        fail(f"the host's capture never heard its own tone: {where}")
    if gained(g_after, g_before, "chunks") == 0:
        fail(f"the host captured the tone but nothing reached the guest: {where}")
    if not heard:
        fail(f"the guest received sound but not the tone: {where}")
    hz = g_after.get("loud_hz") or 0
    if abs(hz - TONE_HZ) > TONE_HZ * 0.05:
        fail(f"the guest heard {hz} Hz, not the host's {TONE_HZ} Hz: {where}")
    if gained(g_after, g_before, "played") == 0:
        fail(f"the guest decoded the tone but its speakers never took it: {where}")
    note(request, f"{hz} Hz heard: loud +{gained(g_after, g_before, 'loud')}, "
                  f"played +{gained(g_after, g_before, 'played')}, dropped +{gained(g_after, g_before, 'dropped')}"
                  f" (host via {played['player']})")


# ── 2 ───────────────────────────────────────────────────────────────────────


def mic_toggle(s, on):
    """The view's own mic button, as an IPC call from the view window."""
    args = {"args": {"peer": host_label(s), "on": on}}
    answer = s.guest.js(IPC_JS % (json.dumps("mic_toggle"), json.dumps(args)), window=s.view)
    if "err" in answer:
        raise Refused("mic_toggle", answer["err"], answer.get("detail", ""))


def test_mic(session, request):
    s = media(session)
    g, h, label = s.guest, s.host, host_label(s)
    _, g_before = audio(g, label)
    h_before, _ = audio(h, s.peer)
    try:
        mic_toggle(s, True)
    except Refused as error:
        if error.code == "NO_MICROPHONE":
            fail(f"{g.name} cannot send its microphone: this build has no microphone capture on {g.os}")
        fail(f"the mic button was refused: {error}")
    try:
        arrived = wait_for(lambda: gained(audio(h, s.peer)[0], h_before, "chunks") >= MIC_CHUNKS, 10)
        time.sleep(1)  # a second more, for played and nonzero to settle
        _, g_after = audio(g, label)
        h_after, _ = audio(h, s.peer)
    finally:
        try:
            mic_toggle(s, False)
        except (Refused, HostError):
            pass
    where = f"guest mic: {describe(g_after, g_before)} | host received: {describe(h_after, h_before)}"

    if g_after.get("device") == "failed":
        fail(f"{g.name}'s microphone did not open: {where}")
    if gained(g_after, g_before, "chunks") == 0:
        fail(f"{g.name}'s microphone captured nothing: {where}")
    if not arrived:
        fail(f"the guest's microphone never reached the host: {where}")
    if h_after.get("device") == "failed" or gained(h_after, h_before, "played") == 0:
        fail(f"the host received the microphone but its speakers did not play it: {where}")
    if gained(h_after, h_before, "nonzero") == 0:
        fail(f"the host received only digital silence: {g.name}'s microphone is muted or "
             f"the OS keeps it from the app: {where}")
    note(request, f"+{gained(h_after, h_before, 'chunks')} chunks reached {h.name}, "
                  f"played +{gained(h_after, h_before, 'played')}, peak {h_after.get('peak')}")
