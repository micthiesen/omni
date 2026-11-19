# Fond: steppers, pinned recipes, masonry, Crouton history, cook-mode chart

Session of 2026-11-19. Brief: five changes to Fond, approved by Michael to build, test and
deploy, with UX and timing decisions delegated where noted. Earlier history: `PROGRESS.md`,
`PROGRESS-crouton.md`, `PROGRESS-requests.md`. References: `docs/recipes.md`,
`docs/recipes-app.md`, `docs/fond/cook-chart-steps.md`, `docs/fond/chart-borders.md`.

The recipe request webhook was left unset, as asked.

## Status: finished

Deployed and verified in production on 2026-11-19: build `4702cd3` on
`https://recipes.syas.ca` (the batch is commit `58f7446`; `4702cd3` only repairs the image
build, see "Production"). Nothing is in progress.

## 1. Unit-aware steppers

**Shipped.** A plus or minus button moves to the next amount its unit is measured in
(`omni_recipe_format::measure::step`). It drives the yield stepper on the recipe page and new
plus and minus buttons either side of the amount in the amount sheet (recipe page and cook
mode). Those are all the steppers the app has.

| Unit | Step |
| --- | --- |
| g | 1 below 20, 5 to 100, 10 to 250, 25 to 1 kg, 50 above |
| ml | 5 up to 60, 25 to 500, 50 above |
| kg, l | 0.05 below 1, 0.1 to 3, 0.25 above |
| tsp, cup | a quarter below 2, a half above |
| tbsp | a half below 4, 1 above |
| oz, lb | a half, a quarter |
| anything counted | 1 (a half below 1); a yield in servings never goes below 1 |

An amount off the grid snaps onto it (326 g goes to 350 or 325, a third of a cup to a half or
a quarter). A step down never jumps over the start of its range (75 ml goes to 60).

**Decision.** Millilitres have no step of ten: the brief suggested 5/10/25/50, but the
measuring rule already in force flags anything between 60 and 500 ml that is not a multiple of
25, so a step of ten would produce amounts the validator warns about. Every amount a stepper
lands on in g, ml, spoons and cups passes that rule (tested).

## 2. Pinned recipes

**Shipped.** "Pin" and "Unpin" in the recipe's menu and in a new menu on every library card
(three dots on the cover). Pinned recipes form a "Pinned" section at the top of the library in
their own order and are left out of the grid below; a search or filter lists every match in one
grid. Reorder by pressing and holding a pinned card (400 ms) and dragging it onto another's
place; without a drag, the card menu has "Move up" and "Move down". The order is stored on the
server (`recipes_pin`, schema version 5), syncs to every device and follows live refresh. For
agents: `recipes_pins` (read, or set the whole ordered list, with dry run and undo), `pinned`
on `recipes_get` and `recipes_search`, and a paragraph in `recipes_guide` (version 6).

**Decisions.**

- While a card is held the others do not shuffle: the card under the finger is outlined as
  the place the held one will take, and the list changes on the lift. Live shuffling moved the
  pressed element to another column, which takes it out of the page and, on a touch screen,
  the touch with it.
- The app's list is taken as far as it goes (a recipe that has since gone is left out) rather
  than refused, because the app sends it from what it holds, possibly offline.
- At most 60 pins. Archived recipes keep their pin and are not shown.
- Not built: scrolling the screen while dragging near its edge.

## 3. Masonry grid

**Shipped.** The library is columns of cards at their own heights, 22px apart, ragged at the
bottom: two columns below 620px, three above (the page is at most 760px wide). Cards are dealt
across the columns in order, so reading row by row follows the sort. Lazy cover loading from
`2820653` and the scroll restore are unchanged.

**Decisions.** Dealing in order rather than filling the shortest column: it keeps the reading
order exact, needs no measuring (so the layout is there on the first frame and scroll restore
is immediate), and with 4:3 covers the columns stay within a card of each other. Covers keep
their 4:3 shape. In the DOM the columns follow one another, so a screen reader reads down a
column.

## 4. Crouton import history

**Shipped.** A one-time step at boot (`Recipes::remove_import_history`) removes from every
recipe: `crouton` as the source kind (and "Crouton" as its name), the block the importer
appended to the notes ("Imported from Crouton on …" and its "Changes: …" paragraph), and the
Crouton text kept as the stored original. Michael's own notes and the recipe stay. Each
recipe gets one ordinary revision for its source (tool `system`, "Remove Crouton import
provenance") and one change-log entry for notes and original; `recipes_undo` takes either
back for 30 days, after which the purge deletes the discarded Crouton texts.

Dry run on a copy of the production database (2026-11-19, `tests/prod_copy.rs`): 145 recipes,
141 changed (141 sources, 141 notes, 141 stored originals), 0 skipped; for every recipe the
document differs in `source` alone, the notes are exactly what stood before the block, and
tags, images, state and cook counts are unchanged. Two examples from it:

*Homemade Vegetarian Chili* (`rcp_1hbnj5am`)

- Source before: `{"kind":"crouton","name":"Cookie and Kate","url":"https://cookieandkate.com/vegetarian-chili-recipe/"}`
- Source after: `{"kind":"web","name":"Cookie and Kate","url":"https://cookieandkate.com/vegetarian-chili-recipe/"}`
- Notes before: three paragraphs of the source's own notes ("Recipes consulted…", "MAKE IT
  VEGAN/DAIRY FREE…", "IF YOU LOVE THIS RECIPE…"), then "Imported from Crouton on
  2026-11-18." and "Changes: garbled tomato line … -> 1 can (796 ml); 15 oz bean cans -> 398 ml
  cans; …".
- Notes after: the same three paragraphs, nothing else.

*Light Tomato & Basil Sauce* (`rcp_1znffh63`)

- Source before: `{"kind":"crouton","name":"Crouton"}`; after: `{"kind":"original"}`.
- Notes before: "Designed for about 2 servings of ricotta ravioli. Keep the sauce smooth,
  light, and only gently seasoned so the ricotta filling remains prominent." then the import
  block ("Changes: 4-6 basil leaves -> 6; black pepper had a unit but no amount in Crouton
  …").
- Notes after: the first paragraph only.

**Decisions.**

- A source with a page address becomes `web`; one with only a name, author or detail becomes
  `person` (one recipe, Beanut Burger, "Recipe courtesy of Veganuary"); one with nothing left
  becomes `original` (40 recipes whose only source was Crouton).
- Nothing inside a recipe's ingredients or steps was treated as import history. The import
  block was the only place the importer wrote change lists and swap explanations. One
  ingredient note still reads "the source uses double; add more to taste" (Lettuce Wedges with
  Cucumber Dressing, salt): it is cooking advice, so it stays.
- The importer was agents following a brief outside this repository, not code here. What
  remains is the guide every agent reads: it now says to put no import date, change list or
  swap explanation in notes, summary, source or ingredient notes, and that `crouton` is a
  retired source kind, read and never written.
- The step's code can be deleted once it has run in production; the `crouton` kind stays
  readable for old revisions.

## 5. Cook-mode chart: step details

**Shipped.** In cook mode's Chart tab, a tap on a step cell opens a step details sheet with
what the Steps tab's card has: step number, full text at the cook's scale, the step's
ingredients and amounts as chips that cross off, the "Until" cue and tip, the timer (start,
countdown, cancel), "Amounts" and "Recipe notes", and one large button: **Mark done**, or
**Undo** on a done step. A running timer shows in its chart cell (a timer mark and the
countdown under the label). Ingredient cells still cross off on a tap. Design note:
`docs/fond/cook-chart-steps.md`.

**Decisions.**

- The quick path without the sheet is **press and hold** a step cell (450 ms) to mark it done
  or un-mark it. Not a checkbox in the cell: step columns are as narrow as 64px, so a 44px
  checkbox would widen every chart and push more recipes into sideways scrolling, and it would
  put a second target inside a cell that is already one. The hold costs no width. Everything
  it does is also in the sheet, which is how a keyboard or screen reader marks a step.
- Mark done closes the sheet (back on the chart, the cell dimmed, the next step ringed); Undo
  leaves it open, since a step still to do is one to read.
- The Mark done button sticks to the bottom of the sheet, so it is on screen on a long step.
- A hold is ignored when the finger moved 10px, the browser took the touch for a scroll, or
  the page scrolled just before (a finger stopping a moving page).
- A done step's timer is not shown in its cell, as in the running strip.
- No haptic tick: Safari on iOS offers none to web pages.

## Screenshots

WebKit at iPhone 14 size, light and dark, in
`~/.claude/projects/-Users-michael-Code-omni/fond-batch2-2026-11-19/`:

- `cook-chart/`: `cook-chart-cell` (chart, first step ringed), `cook-chart-sheet` (step
  sheet), `cook-chart-timer` (timer running in the cell and the strip), `cook-chart-sheet-timer`,
  `cook-chart-done` (done cell), `cook-chart-sheet-done` (sheet with Undo),
  `cook-chart-wide-timers-*` (the widest recipe with a timer on every step).
- `library/`: `library-masonry-2`, `library-masonry-2-end`, `library-masonry-3`,
  `library-masonry-3-end`, `pins-section`, `pins-card-menu`, `pins-card-menu-pinned`,
  `pins-dragging`, `stepper-amount`.
- `production/`: the 145-recipe library in production at phone width and wider
  (`library-masonry-2`, `-2-end`, `-3`, `-3-end`), and the pin screens there.

## Tests

Before the push:

- `cargo xtask gate`: 3,836 tests passed, 22 skipped (fmt, clippy native and wasm32,
  deps-check). `cargo deny --locked check`: ok.
- New server tests: `recipes_pins.rs` (2), `recipes_import_history.rs` (3), unit tests for the
  stepping grid in every unit family, and the production-copy check above.
- Playwright WebKit at iPhone size on the mock: 74 passed, 2 skipped (on-demand screenshots;
  the masonry scroll-restore test, which needs a library taller than the mock's and runs in
  production). New: `library.spec.js` (9: masonry at two widths, scroll restore, pin from both
  menus, drag reorder, menu reorder, a list set elsewhere, both steppers) and
  `cook-chart.spec.js` (8).
- An independent review of the pins, cleanup and stepper code found no data-loss or run-once
  bug in the cleanup. Fixed from it: a pin list naming a deleted recipe was refused whole and
  left the app out of step; pin positions could tie on a second device after a pinned recipe
  was trashed; the drag could lose the touch when cards reshuffled; the cleanup now also
  cleans notes when a source write is refused, and leaves a stored text another recipe shares.

## Production

- **The deploy.** The first image build of `58f7446` failed, not on this batch: the CAM++
  speaker model's release asset was replaced upstream with a file one byte longer, so its
  pinned SHA-256 stopped matching as soon as the build cache no longer held the old download.
  `4702cd3` takes that file from the same pinned earlier image the parakeet model already
  comes from (checked on boris: that image holds it with the pinned checksum). CI then passed
  every job. Adopting either new upstream export stays a separate decision.
- **Crouton history.** Boot log: "Removed Crouton import history: 141 recipes changed (141
  sources, 141 notes, 141 stored originals), 0 left as they were". The library before and after
  was compared recipe by recipe from the Mac: for all 145 the document differs in `source`
  alone; the 141 imported ones have notes equal to what stood before the block (66 keep notes
  of their own, 75 are now empty), no stored original, and one new revision; the other four
  are untouched; "Crouton" appears in no notes field and no source. Sources are now 101 `web`,
  42 `original`, 2 `person`.
- **Tools.** A fresh client: `recipes_pins` answers, `recipes_guide` is version 6 with the
  pins paragraph and the rule against import history.
- **Browser tests** (WebKit at iPhone size through the SSH proxy):
  - `library.spec.js`: 8 passed, 1 skipped (the list set elsewhere needs the mock): masonry
    of 145 cards at 390px and 820px, scroll restore, pin from both menus, drag reorder, menu
    reorder, the amount sheet's steps in every unit family the library has, the yield stepper.
  - `cook-chart.spec.js`: 8 passed (on Cauliflower Piccata and World's Best Lasagna).
  - The existing suite (`fond`, `chart-borders`, `shell`, `view-preference`): 35 passed, 4
    skipped (finishing a cook, the inbox upload, on-demand screenshots). An earlier run of it
    was lost to the SSH tunnel dropping, and showed that the chart-lines test needed a larger
    time budget now that it also marks a step through the sheet in each of 30 recipes; both
    are fixed (keep-alives on the tunnel, the budget in the test).
  - Not run against production: `requests.spec.js` (unchanged since the last batch, and it
    needs a throwaway recipe and writes requests) and the mock-only group.
- **Test data.** Nothing is pinned, no cook session is open, the Requests page and the inbox
  are empty. Two cook sessions the interrupted run left open were discarded (nothing logged).
  The amount tests leave revisions on Dad's Mustard Vinaigrette, each change followed by its
  undo, as before.
- Not verifiable here: a real iPhone (the list below).

## To check on the iPhone

Steppers

1. A recipe in servings: plus and minus move by one serving. A recipe whose yield is in ml or
   g (a dressing, a syrup): they move by sensible amounts.
2. Pencil on an ingredient: minus and plus either side of the amount; grams move by 5 or 10,
   spoons by quarters or halves; changing the unit in the sheet changes the step.

Pinned recipes

3. Pin from a recipe's three-dot menu and from a card's three dots; "Pinned" appears at the
   top; Unpin from either.
4. Press and hold a pinned card until it lifts, drag it onto another pinned card (it gets an
   outline), let go: they swap places. The page must not scroll during the drag, and no link
   preview or text selection should appear.
5. A short press and a swipe on a pinned card still open the recipe and scroll.
6. Card menu of a pinned card: Move up, Move down.
7. Pin on the phone, look on another device: same order.

Library

8. The grid: two ragged columns, no gaps under short cards, covers appear as you scroll, and
   Back from a recipe returns to the same place.
9. The three dots on a card do not get in the way of opening it.

Crouton history

10. A few imported recipes: notes hold only your own words, there is no "Original" section
    with Crouton text, and "From …" names the real source or nothing.

Cook-mode chart

11. Tap a step cell: the sheet, with Mark done at the bottom above the home indicator.
12. Press and hold a step cell: it flashes and is marked done, the sheet does not open; hold
    again to un-mark. Scrolling the chart with a finger on a step cell must not mark it.
13. Start a timer from the sheet: the countdown shows in the cell and in the strip above.
14. Tap an ingredient cell: crossed off, as before.
