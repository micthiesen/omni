---
name: slate-next
description: >
  Decide what the Omni Slate hardware project's next step should be. Reads slate/docs/STATE.md, the release gates in
  slate/docs/decisions.md, the build sequence in slate/docs/build.md, and recent git history, enumerates 2-4
  candidate next steps with a gating analysis (what each unblocks, size, risk), recommends one, and
  records the decision + runners-up in slate/docs/STATE.md. Use when unsure what to work on next, when
  the previous Next step completed, or from $slate-wrap when concluding a session. TRIGGER on "what's
  next for Slate", "pick the next Slate step", "$slate-next"; not for Omni server work.
---

# Next (Decide The Next Step)

Turn "what should we do now?" from an open-ended re-derivation into a short, recorded decision.
The output is a **recommendation with its why** and an updated `slate/docs/STATE.md`. When `$slate-next` is part
of an active request to continue the project, start the chosen repository work after recording it.
Pause only at a physical gate or a consequential choice the repo cannot resolve.

## 1. Gather Ground Truth (Cheap, Parallel)

- Read [`slate/docs/STATE.md`](../../../slate/docs/STATE.md) — the previous Next and the candidates not chosen
  (your shortlist starts here).
- Read the gating structure: [`slate/docs/decisions.md`](../../../slate/docs/decisions.md) (release gates) and
  [`slate/docs/build.md`](../../../slate/docs/build.md) (build sequence + procurement gates). Check
  [`slate/bom/bom.csv`](../../../slate/bom/bom.csv) purchase status if a candidate depends on parts.
- `git log --oneline -- slate` since STATE's "Last updated" date — what actually landed since the decision
  was recorded.

Don't re-read the deep spec docs unless a candidate genuinely hinges on a detail; the point of
STATE.md is that you shouldn't have to.

**STATE.md candidates can be stale.** Before starting a "build this charted thing" step,
`git grep`/`git log` to confirm it isn't already done — fix the stale entry as part of the
decision.

## 2. Enumerate Candidates (2–4)

For each candidate, one tight block:

- **What it is** — one sentence, concrete enough to start.
- **What it unblocks** — its place in the gating chain (release gates, build phases). Unblocking
  the critical path outranks polish.
- **Hardware-gated or not** — needs a purchased part in hand, a fabricated part, or bench access
  → note the gate. Pure docs/CAD/KiCad/firmware work is always startable.
- **Size + risk** — rough effort, and the biggest unknown that could sink it.

## 3. Recommend One

Pick with these biases, in order:

1. **Unblock the critical path.** Work that advances STATE's Now/Next beats work that widens the
   surface.
2. **De-risk the biggest unknown early.** A cheap probe that could invalidate a plan beats
   building on the unproven plan (this project's gates encode exactly this: confirm the panel
   pinout before capture, measure energy before buying the pouch).
3. **Parallelize the independent.** The recommendation can be "start X AND kick off Y in the
   background" — candidates aren't mutually exclusive.

State the recommendation in two sentences: what, and why it beats the runner-up. For a genuinely
contentious or expensive direction (a pivot, a multi-week bet), get an independent design review
before recording it.

## 4. Record It

Rewrite `slate/docs/STATE.md`:

- **Next** — the chosen step, its two-line why, the doc pointer, and any hardware gate.
- **Candidates Not Chosen** — the runners-up and the one-line reason each waits. This is what
  makes the *next* run of this skill cheap.

Commit if mid-session; leave it for `$slate-wrap` if wrapping anyway.
