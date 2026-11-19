# Fond: diet, measuring, chart and cook-mode changes, and the Crouton import

Session of 2026-11-18. Brief: five Fond code changes (Part A) and the import of Michael's Crouton
export (Part B). A push to `main` deploys and restarts Omni, which ends the working session, so
the push came last. Earlier build history is in `PROGRESS.md`.

## Status: finished, pending the deploy of the commit that carries this file

Everything below is built and tested locally and committed together. The import ran against
production with the tools that were already deployed; the new diet and measuring checks were run
over every imported document locally before the push.

After the deploy, check: `https://recipes.syas.ca` loads, `recipes_guide` reports version 4 with
a `household` field, and schema version 3 applied at boot (`docker logs omni`).

## Part A

### 1. Household diet (bivalve vegetarian)

- The rule is code: `crates/omni-recipes/src/household.rs`, word lists and `diet_flag` in
  `crates/omni-recipe-format/src/diet.rs`.
- Every agent sees it: `recipes_guide` returns `household.diet` and the guide has a new section
  "The household" (guide version 4); `recipes_import_url` and `recipes_inbox_get` return `diet`
  with the rule and the source's ingredient lines that conflict; the `recipes_create`,
  `recipes_guide` and `recipes_import_url` descriptions say so.
- A create, edit or replace that introduces meat, fish, other seafood or a hidden animal product
  (stock, fish sauce, anchovies, Worcestershire, Caesar dressing, gelatin, marshmallows, lard,
  bacon fat) is refused with `diet_conflict`, one problem per ingredient with a fitting hint.
  `keepDietConflicts: true` keeps them, for an ingredient Michael explicitly asked for. A conflict
  the recipe already had is a warning, so later edits are not refused. Bivalves, clam juice,
  oyster sauce, eggs and dairy pass; names with "vegetarian", "vegan", "plant-based" and the like
  pass.
- The guide's worked example is now Cauliflower Piccata.
- Seeded recipes: Chicken Piccata `rcp_3ps0548z` trashed (with its stray test cook) and replaced
  by the new Cauliflower Piccata `rcp_rydc5v1b`; Chicken Enchiladas `rcp_xr3r1m5h` became Black
  Bean Enchiladas; the beef lasagne `rcp_3e2bvg6e` and beef chili `rcp_tw452req` were trashed as
  duplicates of his own recipes. The `beef` and `chicken` tags are deleted.
- Not built: a settings screen. The diet is a constant in code, like the guide.

### 2. Amounts that are easy to measure

- The guideline is in the guide ("Amounts that are easy to measure") and in `household.measuring`.
- The validator warns with `awkward_amount` and says what to write instead
  (`crates/omni-recipe-format/src/measure.rs`): grams off a multiple of 5 from 20 g, tsp not in
  quarters, tbsp not in halves, more than 4 tbsp in one go (judged per use when steps share an
  ingredient), cups that are neither quarters nor thirds, ml off 5 up to 60 and off 25 above
  (cup measures and can sizes pass), any ounces or pounds. Warnings never block.
- Scaling keeps friendly amounts: a scaled amount is shown as the nearest measurable form
  ("1½ tsp", "¼ cup + 2 tbsp", "175 g", "750 g" for 0.75 kg). Stored amounts at scale 1 read
  exactly as written.
- Also fixed: a share of a step's result reads "1/6 of the sauce", not "0.17 of".

### 3. View preference

- The last chosen view (Steps or Chart) is stored per device in localStorage (`fond.view`,
  `crates/omni-recipes-web/src/platform/prefs.rs`). Recipes open in it; with nothing stored the
  default is unchanged. Cook mode shares the preference: its Steps and Chart tabs store it,
  Gather does not. Blocked or throwing storage leaves the default and a working toggle.

### 4. Chart borders and the pinned column

Design note: `docs/fond/chart-borders.md`. Rule set:

- Strong line: the frame of a block, the left side of every step cell, and under the last input
  of a step.
- Light line: between two inputs of the same step.
- No line: inside an input that waits for a later step, and across a step cell.
- Each line is drawn by exactly one cell, so none doubles.
- The ingredient column is pinned. Only while scrolled sideways it gains a full-height line and
  a short shadow, so a stretching ingredient no longer looks like a missing border. Block titles
  do not scroll. Nothing is pinned to the top (the chart has no header row).

Root causes fixed: `border-collapse: collapse` loses the lines of sticky cells in WebKit (now
`separate` with one owner per line); fractional column widths and 32.5px rows gave uneven line
thickness (whole-pixel columns, 18px line height); the pinned label of a stretching ingredient
had no right edge while scrolled; a ready set-up step had two rings.

Before and after screenshots (WebKit, iPhone 14, 71 each: vinaigrette, cookies, piccata,
enchiladas, lasagne; recipe view and cook mode; scroll start, early, middle, end; dark):
`~/.claude/projects/-Users-michael-Code-omni/fond-crouton-2026-11-18/chart-borders/{before,after}/`.

### 5. No cook-only Adjust

- The Adjust sheet, its copy, "Adjusted", "Use again", "Last time" and the finish-sheet block are
  gone. In cook mode the pencil on an ingredient opens the amount sheet; saving writes the normal
  `app` revision and the open session moves to it at once (a `rebase` queued behind the amount
  change, offline too). The toast Undo restores it. A step offers "Amounts" and "Recipe notes"
  (the recipe's own notes, saved permanently).
- Scale stays a view setting. Cook mode has no scale control; nothing turns a scale into a recipe
  change.
- Removed end to end: `adjust`, `unadjust` and `dismissAdjustment` events, adjustment records,
  open adjustments in every tool output, `resolves` on `recipes_edit` and `recipes_replace`,
  `openAdjustments` in `recipes.cook_finished`. Agents help mid-cook with `recipes_edit` and a
  `rebase` event.
- Stored data: schema version 3 drops `recipes_adjustment` (production had no open ones). Old
  cook events of the retired kinds stay in the log and every reader skips them. Archives written
  before the change still restore.
- An app installed before the change keeps working: the server still sends an empty
  `adjustments` list in each synced recipe and accepts and drops retired events.

### Tests

- `cargo xtask gate`: 3,815 tests passed, 21 skipped (fmt, clippy native and wasm32, deps-check).
- `cargo deny --locked check`: ok.
- Playwright WebKit, iPhone size, on the mock server: 37 passed, 1 skipped (the screenshot
  capture, which runs only when asked). Includes Michael's tests: view switched to Chart survives
  a reload and another recipe, and back; an amount changed in cook mode creates a revision, shows
  at once in cook mode and in the recipe, and Undo restores it; the built app has no wording
  about changes for one cook only; chart lines checked in every recipe at several scroll
  positions.
- Not run: the browser suite against production (the deploy ends the session), a real iPhone.

## Part B: Crouton import

- Source: `Crouton Recipes - Nov 18, 2026.zip` (141 recipes), unzipped to scratch; the original
  is untouched.
- **141 imported, 0 skipped.** Each was handled on its own (twelve batches of about twelve, one
  agent per batch following a written brief with the diet and measuring rules), created with its
  Crouton text kept as the Original, its image attached, tags from an extended vocabulary, his
  notes verbatim followed by an "Imported from Crouton" block listing the changes.
- Per-recipe results (amount changes, fixes, swaps):
  `~/.claude/projects/-Users-michael-Code-omni/fond-crouton-2026-11-18/import/results.jsonl`.
- Library now: 145 active (141 imported, Cauliflower Piccata, Black Bean Enchiladas, Simple
  Scones, Dad's Mustard Vinaigrette).
- Seeded duplicates trashed first: Chocolate Chip Cookies, Lasagne alla Bolognese, Classic
  Homemade Chili.
- Diet: one meat conversion, World's Best Lasagna (plant-based Italian sausage and ground, 2 tbsp
  oil added for browning). Hidden products swapped in seven more: Baechu Kimchi (vegan fish
  sauce, miso for the salted shrimp), Mushroom Soup, Instant Pot Mac and Cheese, The Easiest
  Vegetable Stir Fry and Cauliflower Soup (vegetable stock or bouillon), Caesar Cocktail and
  Sloppy Joes (vegetarian Worcestershire; the Sloppy Joes steps still said ground beef).
- Amount rewrites, examples: 2 cups flour to 240 g; 1 lb cream cheese to 450 g; 4 tbsp butter to
  55 g; 6 tbsp flour to 45 g; 1/2 cup sugar to 100 g; 3/4 cup milk to 185 g; 19 oz can chickpeas
  to 1 can (540 ml); 28 oz can tomatoes to 1 can (796 ml); 1.5 lb potatoes to 675 g; 107 g flour
  to 105 g; 60 ml pasta water to 4 tbsp.
- Notable fixes: methods written or restored where Crouton had none (both Caesars, Greek Salad,
  Gypsy Soup, Pancakes, Dried Summer Borscht, Sweet Yams, the Oz Family lentil soup, Chickpea
  Shakshuka); ingredients Crouton had lost restored from the source page (Roti, Ten-Minute Chana
  Masala, Baechu Kimchi); dry mixes, doughs, fillings and sauces split into components; the
  ravioli sauces list the ravioli they dress.
- Things to look at:
  - Dried Cream of Broccoli Soup and Dried Potato Soup: his notes use more water than the
    bouillon in the mix is for. Left as written and flagged in the notes.
  - Freezer Ricotta Ravioli: the list said 5 eggs, his note 200 g (about 4); the note was
    followed. Freezer Ricotta Gnocchi: his note expects 1.0 to 1.2 kg, the ingredients total
    about 750 g.
  - Amounts and methods that are the importer's own: Greek Salad, Greek-ish Salad, Apple Crisp
    filling, Sweet Yams, Dahl.
  - Yields guessed where Crouton had none.
  - Four import notes are slightly stale (World's Best Lasagna cheese weights, Veggie Powder,
    Air Fryer Chickpeas, Ramekin Bread Pudding): notes cannot be edited through the tools.
  - Many export images are 280 px thumbnails.
  - Two awkward-amount warnings left on purpose: 320 ml vodka for 8 Caesars, 8 tbsp ice water in
    the pie crust (added by the spoon).
- Checked after the import: every document passes the new diet check, renders as a list and a
  chart at scale 1, 0.5 and 2 without error, and has an image; the inbox is empty.
