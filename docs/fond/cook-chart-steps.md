# Fond cook mode: steps in the chart

What a step cell of the chart does in cook mode
(`crates/omni-recipes-web/src/ui/cook.rs`, `src/ui/chart.rs`, "Cook mode: chart
step sheet" in `style/app.css`). The chart's lines and pinned column are in
`docs/fond/chart-borders.md` and are unchanged. The recipe page's chart is
unchanged too: a tap opens its read-only step sheet and nothing can be held.

## What a touch does

| Touch | On | Does |
| --- | --- | --- |
| Tap | a step or set-up cell | opens the step sheet |
| Press and hold, about 450 ms | a step or set-up cell | marks the step done, or un-marks a done one, without the sheet |
| Tap | an ingredient or "from above" cell | crosses the input off, as before |

Before this, a tap on a step cell marked it done. That made the chart the one
tab where a cook could not read a step, start its timer or reach the amounts,
and a stray touch marked a step with no way to see what it was.

**Why hold and not a checkbox in the cell.** A step column is as narrow as
64px (`logic::chart_widths`). A 44px checkbox in every cell would widen every
column, so more recipes would scroll sideways, and it would put a second
target inside a cell whose whole area is already the tap target. The hold
costs no width and no layout.

The hold is a shortcut. Everything it does is also in the sheet, which is how
a keyboard or a screen reader marks a step: the cell is a button (`role`,
`tabindex`, Enter and Space open the sheet) named by its label, and an input
cell reports `aria-pressed`. The empty right half of a stretched cell takes
taps but is not a second button.

## The hold

`Hold` in `chart.rs`, pointer events, so touch and mouse behave the same:

- `pointerdown` starts a 450 ms timer. When it fires the step is toggled and
  the cell fades once from the accent colour to its new colour (class `held`,
  colour only, off with reduced motion).
- The click that follows the lift is swallowed, so a hold never also opens the
  sheet. The flag clears 400 ms after the lift, because iOS may send no click
  after a long press and a later click with no press of its own (a screen
  reader) must still open the sheet. A touch sends `pointerleave` as it lifts
  and its click after that, so leaving the cell does not clear the flag.
- It was a scroll, not a hold, when the pointer moves more than 10px, when the
  browser sends `pointercancel` or the pointer leaves the cell, or when
  anything on the page scrolled between 300 ms before the press and the timer
  firing. The last one covers a finger that stops a moving page and rests
  there. One capturing `scroll` listener on the document records the time.
- A timer still running when the chart leaves the page (a tab change, a
  rebase) does nothing.

## The sheet

The existing bottom `Sheet`, titled with the step's name. It shows "Step n of
m" (with "done" once it is), then exactly what the Steps tab's card shows,
from the same function (`step_body`): the step's text at the session's scale,
its inputs as chips that cross off, the "Until" cue, the tip, the timer
(start, countdown, Cancel), "Amounts" and "Recipe notes". Those two close the
step sheet before they open theirs, so there is one sheet at a time.

At the bottom is one large button. **Mark done** marks the step and closes the
sheet: the cook is back on the chart, sees the cell dimmed and the next ready
step ringed. On a done step the button reads **Undo**; it un-marks the step
and leaves the sheet open, because a step that is still to do is one the cook
wants to read. The button sticks to the bottom of the sheet, so on a long step
it is on screen without scrolling.

The sheet is built while it is open and follows the session: a change of state
patches it in place and the countdown is a text node of its own, so the sheet
keeps its scroll position. If the session moves to a revision without the
step, or the cook ends, it closes.

## The timer in the cell

A step with a running timer shows a small timer mark and the countdown on one
more line under its label, in the colour of the running strip, red once it
has run out. It follows `store.now` like the strip and the card. As in the
strip, a done step's timer is not shown.

Cost: no width. Columns come from the labels alone and the table layout is
fixed. The line is 18px, the chart's line height, so rows stay whole pixels; a
step that spans one short row grows by that line while its timer runs. In a
64px column a countdown with hours ("2:30:00") does not fit beside the mark,
so the mark wraps onto the line above it instead of overflowing.

## Left out on purpose

- No checkbox, swipe or double tap in a cell (see above).
- No progress ring while the finger is down: the hold is short, and the cell's
  change of colour is the answer.
- No haptic tick: Safari on iOS has no vibration API for web pages.
- The hold does not start timers or cross off inputs. An input is one tap.
- No Previous and Next in the sheet. The chart is the way around; the Steps
  tab has the arrows.
- Mark done in the sheet does not move the Steps tab's open step.

## WebKit pitfalls

- **Callout and selection.** A long press on text selects it and raises the
  callout. Cook-mode cells set `user-select: none` and
  `-webkit-touch-callout: none`; the cell also cancels `contextmenu`.
  Playwright's WebKit does not know `-webkit-touch-callout`, so only the
  selection is tested.
- **Scrolling.** `touch-action` is left alone, so the chart scrolls both ways
  from a step cell. WebKit sends `pointercancel` when it takes a touch for a
  scroll and stops sending `pointermove`; both paths cancel the hold.
- **The click after a hold.** With a mouse a click always follows; on iOS it
  may not. The swallow flag is set by the hold and cleared by time, never left
  waiting for a click.
- **Transforms.** The chart tests refuse any transform inside a chart, so the
  acknowledgement animates a colour, not a scale.
- **Sticky inside a padded scroller.** Sticky offsets are measured inside the
  sheet's padding. The button bar uses a negative `bottom` to cover the
  sheet's bottom padding, as `.sheet-head` does at the top.

## Tests

`e2e/cook-chart.spec.js`: the sheet's contents, Mark done and Undo against the
server's session, Enter on a cell, the hold with the mouse and with touch
pointer events (short, moved, cancelled, scrolled, chart gone), an ingredient
tap, the timer in the sheet, the cell and the strip, the way to the notes and
amounts, a chip and its chart cell, the shell with the sheet open and its
scroll position, and the widest recipe with a timer on every step (columns
unchanged, whole-pixel rows, every countdown inside its cell).
`FOND_SHOTS` writes `cook-chart-*` screenshots. The cook-mode test in
`fond.spec.js` and the cook-mode part of `chart-borders.spec.js` mark steps
through the sheet and the hold.

Only on an iPhone: the feel of 450 ms, the callout, a real scroll's
`pointercancel`, and a finger stopping a moving page.
