---
name: slate-wrap
description: >
  Conclude an Omni Slate session so a fresh one can continue: sweep un-encoded learnings into
  the right slate/ docs, decide or confirm the next step (via $slate-next when open), rewrite
  slate/docs/STATE.md, commit, and push. Use when ending a Slate session or on "wrap up Slate",
  "$slate-wrap"; not for Omni server work.
---

# Slate Wrap (Conclude The Session)

After a wrap, the session is disposable: everything it learned is in `slate/`, the next step
is recorded with its why, and a fresh session boots from `slate/docs/STATE.md`
(per `slate/AGENTS.md`). Never let a session's value live only in its context window.

## 1. Sweep Learnings Into Their Homes

| Kind of learning | Home |
|---|---|
| Design decisions, spec changes, deviations | the relevant `slate/docs/*.md` (decisions, electrical, firmware, protocol, infrastructure, power-budget, mechanical, build) |
| Part selection, order, receipt changes | `slate/bom/bom.csv` |
| Test results and sign-offs | `slate/testing/test-matrix.csv` |
| Measured values (currents, energies, panel behaviour) | the doc holding the provisional value, replacing or annotating it, plus the matrix row |
| A primary-source check of a datasheet value | `slate/docs/sources.md` Verified column |
| Firmware behaviour, toolchain gotchas | code comments in `slate/firmware/` or `slate/docs/firmware.md` |
| A repeatable procedure | a skill under `.agents/skills/` |
| Cross-cutting rules, preferences | `slate/AGENTS.md` |

Write the content there; STATE.md gets only pointers. A dead end is a finding too.

## 2. Decide Next

- Previous Next still the plan? Carry it forward, updated with progress; verify from the
  repo rather than asking.
- Previous Next completed, or the picture changed? Run **`$slate-next`**.

## 3. Rewrite STATE.md

Rewrite `slate/docs/STATE.md` wholesale (git holds history), keeping only:

- **Now**: the current work picture in 3 to 6 bullets.
- **Next**: from step 2, with the two-line why and doc pointer.
- **Candidates Not Chosen**: carried forward, pruned.
- **Learned Recently**: one-line pointers to what step 1 wrote.
- The "Last updated" date.

No git, tool or machine snapshots.

## 4. Commit And Push

Run `sh slate/scripts/check.sh` if code or PCB source changed, then commit and push to
`main` per the root `AGENTS.md`. Slate-only pushes do not deploy Omni.
