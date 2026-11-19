# Fond: cook-mode chart polish after the first iPhone test

Session of 2026-11-19. Michael's feedback from the iPhone: "holding to mark a cell as done is
jank because it can accidentally select text easily. Some other stuff feels jank too like the
timers." Design reference: `cook-chart-steps.md` (updated with everything below).

## What changed

**Press and hold**

- Nothing on the cook screen can be selected any more. Only the chart cells were unselectable
  before, and iOS answers a long press on such text by selecting the nearest text it may: the
  hint under the chart, a caption, the title. The sheets keep their text selectable.
- The chart cancels `selectstart` and `contextmenu`, cells have no tap highlight, and a
  selection the browser still made is dropped when the hold is taken.
- The cell fills from the left while the finger is down, then flashes, and a four-second toast
  says "Done: sear" with Undo (the finger covers the cell).
- The hold is 400 ms, down from 450.
- Undo three ways: the toast, a second hold on the cell, or Undo in the step's sheet.
- A press that moves more than 10px, or that the browser takes for a scroll, still marks
  nothing; the fill disappears.

**Timers**

- Running timers are docked under the screen instead of sitting above the chart. Starting,
  stopping or finishing one no longer moves the chart. Before, a hold that finished a timed
  step removed the strip and pulled the chart up under the finger.
- One timer is as wide as the screen; more sit two to a row. A tap opens the step (its sheet
  on the Chart tab).
- Digits are tabular, right-aligned in a fixed width.
- A timer that ran out turns the alert colour and counts up ("+0:12") in the dock, the cell
  and the sheet; the sheet's button then reads Stop.
- The alert tones repeat every ten seconds for a minute until the timer is stopped (they
  played once), and the audio is resumed first, since iOS suspends it in the background.
- Cancel shows "Timer cancelled" with Undo, and is a 44px button instead of a small link.
- Marking a step done stops its timer. Before, the timer went on invisibly and rang later.
  Undo on the toast brings it back.
- The clock is read four times a second (it was once), so a displayed second never skips or
  lingers. Timers were already end times compared with the wall clock, so a locked screen or
  a suspended app loses nothing; the wake lock was already held for the whole of cook mode.
- Notices sit above the dock, not on it.

**Elsewhere**

- No double-tap zoom anywhere in the app (`touch-action: manipulation`): two quick taps on
  two ingredients are two taps. Pinch zoom stays.
- A sheet opens at its top; it used to keep the scroll position of the step read before.
- "While … runs" offers moved to the Steps tab, above the step card.

## Decisions

- **400 ms.** iOS starts its own long press (selection, callout) at about half a second, so
  the hold now ends before it. The fill makes the wait visible, which matters more than the
  50 ms.
- **Dock at the bottom**, not a strip at the top: it is the only place where timers coming
  and going do not move the content.
- **Counting up** after the end instead of a red "0:00": it says how long the pan has been
  over.
- **Done stops the timer.** A done step with a hidden running timer had no good outcome.
- **No pause.** A timer is an end time in the session's event log (`timer`, `cancelTimer`).
  Pause needs a new event kind on the server and in every reader. Say so if you want it.

## Not fixable in a web app

- No haptics and no vibration: Safari on iOS has neither for web pages.
- The ring is Web Audio: the silent switch mutes it, and it cannot play while the phone is
  locked. Omni's own push alert covers that case, as before.

## Tests

- `cargo xtask gate`: 3,837 tests passed, 22 skipped; `cargo deny --locked check` ok. One unit
  test added, for the count-up text.
- Playwright WebKit, iPhone 14 profile, mock server: 75 passed, 2 skipped (on-demand
  screenshots, one masonry test that skips itself). `cook-chart.spec.js` gained: nothing on
  the cook screen is selectable, `selectstart` and `contextmenu` are refused, the press fill,
  the toast and its Undo, the dock outside the scroller, digits that keep their place, done
  stopping a timer and Undo restoring it, Cancel with Undo, and a new test for two timers
  with one run out (two to a row, count-up, alert above the dock, Stop).
- Playwright's WebKit cannot play a finger, a real long press or the iOS selection, and does
  not advance an animation while the mouse is held. Those are the iPhone list below.
- Screenshots (light and dark) are on the Mac in `.local/fond-shots/cookchart-polish/`
  (not in git): `cook-chart-held`, `cook-chart-timer`, `cook-chart-timer-late`,
  `cook-chart-sheet-timer`. The earlier look is in the batch 2 screenshots.

## Production

Deployed by the push of `1eacc78` (CI passed; the deploy restarts only Omni). On
`https://recipes.syas.ca`, `cook-chart.spec.js` in WebKit at iPhone size through the SSH proxy,
cooking "World's Best Lasagna" and "Crispy Cheesy Pan Pizza": 9 of 9 passed. Every test
session was discarded, which logs nothing; no cook session is open afterwards. Screenshots:
`~/Pictures/fond-shots/cookchart-polish-prod/`.

## To check on the iPhone

1. Chart tab: press and hold a step. The cell fills, flashes, and "Done: …" appears with
   Undo. No text is selected anywhere, no magnifier, no menu.
2. Start the hold and slide away: the chart scrolls and nothing is marked.
3. Hold a done step: it is un-marked. Tap Undo on a toast: it takes the change back.
4. Start a timer from a step's sheet: it appears at the bottom and the chart does not move.
   Its digits do not shift.
5. Start a second timer: two side by side. Tap one: its step opens.
6. Let a short timer run out with the app open: tones every ten seconds, the timer turns
   orange and counts up; Stop ends it. Lock the phone during a timer and unlock: the time is
   right.
7. Tap two ingredients quickly: both are crossed off and the page does not zoom.
8. Mark a step with a running timer done: the timer goes; Undo brings it back.
