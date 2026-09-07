# ADR 0064 — A quality preset is a target, not a ceiling

Status: accepted
Date: 2026-09-07

Amends ADR 0037 (the three-knob adaptive ladder) and D7 /
`docs/bugs/13-stream-resolution.md` (the guest's manual scale choice). See
`docs/bugs/17-remote-hotkeys.md` §1.

## Context

A guest reported that the picture "keeps flickering — better, then worse"
for the whole of a session, whatever the quality selector said. The selector
was not lying about what it had asked for. It was lying about what that ask
meant.

**A preset was defined as a ceiling.** D7 settled the interaction between the
guest's manual choice and the host's adaptive controller as
`effective_scale = min(chosen, adaptive)`, with the reasoning that "a manual
choice and the adaptive ladder must never fight over the same variable". The
formulation reads carefully and gets the conclusion backwards: a `min` does
not stop the two from fighting, it just names the winner of each round. The
ladder went on moving underneath.

**And what it moves is visible.** ADR 0037's controller walks three knobs.
Recovery climbs 5% of the bitrate per adjustment; degradation takes 10% back.
On a link that is neither perfect nor plainly bad — which is most links —
those two do not converge, they oscillate. An oscillating bitrate is exactly
what a person sees as the quality changing on its own. The scale ceiling
being honoured throughout is no comfort: the ceiling was never the knob doing
the flickering.

**The preset also never reached the host at all** until someone touched the
selector. `mountToolbar` sent `view_set_scale` on a change and on a monitor
switch, and on nothing else, so `manual_cap` stayed `None` for every session
nobody adjusted — which is every session — and the adaptive controller was
the only thing driving the picture.

**And the cheap preset was unreadable.** `performance` was a flat
`ABR_MIN_SCALE_PERCENT`: half of whatever the host's screen is. On a 1366x768
laptop that is 384 lines. The name promises to be *faster*, not illegible.

## Decisions

**A preset pins the whole target.** `lumepeer_media::abr::effective_scale` is
replaced by `pinned_target`, which turns the guest's chosen percentage into a
complete `QualityTarget` — bitrate, frame rate and scale together. While a
guest is naming one, `AbrController::on_feedback` is not consulted at all.

All three knobs and not only the one the preset names, because a bitrate that
walks under a held scale is the same flicker by another name. Half a fix here
would have looked exactly like no fix.

**The adaptive controller still runs for a peer that names nothing.** An
older guest, or one whose `view_set_scale` the host refused, gets ADR 0037
unchanged. Nothing about the ladder is deleted; it stops being a second
opinion on a picture somebody already chose.

**The guest sends its preset once at mount**, after asking for the monitor
list — the list is what turns a preset into a percentage. A host that refuses
to announce its monitors still gets the preset, computed against an unknown
screen, which is the uncapped picture: a percentage worked out from a height
nobody knows would cap by an arbitrary factor, and that is worse than not
capping.

**The presets are defined by picture height.**

| Preset | Meaning | 1080p | 1440p | 4K |
|---|---|---|---|---|
| `performance` | 720 lines | 67% | 50% | 50% (the ABR floor) |
| `balance` | midway between 720 and the screen | 83% | 75% | 67% |
| `quality` | the host's own resolution | 100% | 100% | 100% |

`balance` is averaged in *lines*, not in percent: a percentage is a ratio of
the very number being halved, so averaging percentages lands somewhere that
depends on the screen rather than between the two pictures a person is
choosing among. The floor of `ABR_MIN_SCALE_PERCENT` still binds, so a 4K
host cannot express 720p and gets 1080p — which is still at or above the 720
lines the preset promises.

**`StreamScaleRequest` keeps its wire shape.** No new message, no minor-
version bump, no feature string: what changed is what the host does with a
number it has always received. A guest that speaks minor 7 and a host that
speaks this one interoperate exactly as before, with the host now holding the
picture still instead of adapting it.

## Consequences

A session no longer changes its own picture quality. That is the whole point,
and the cost is real and deliberate: a link that genuinely cannot carry the
chosen picture now stutters where it used to soften. Softening is the better
failure for a link nobody chose a picture for, and the worse one for a person
who chose `quality` by name and meant it. Choosing the failure is what the
preset now does.

The `performance` preset costs more than it used to on every screen taller
than 1440 lines and less on nothing — it was half the screen and is now
720 lines or the floor, whichever is more. That is the correction, not a
regression: the previous figure was chosen for a bandwidth budget and read by
people as a picture.

What this does **not** do is give each preset its own bitrate. The rate
control of ADR 0059 is variable and the target is a ceiling the encoder does
not have to spend, so a 720p picture already costs a fraction of what a
native one does at the same ceiling. Naming three bitrates would need a new
wire message to carry them and would buy little; if a measured session shows
`performance` spending more than it should, that is the change to make, with
the measurement in hand.
