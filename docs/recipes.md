# Recipes (Fond)

Fond is the owner's recipe library. Agents manage it through the `recipes_*`
MCP tools; the owner cooks from a dedicated web app that Omni serves only on
its own host. The server side is `crates/omni-recipes`; the document format,
its validator and its renderers are the pure crate `crates/omni-recipe-format`,
shared with the app (`crates/omni-recipes-web`) so an agent's preview and the
phone render from the same code. The contract between server and app is
`crates/omni-api/src/fond.rs`.

## Model

A recipe is a JSON document (`RecipeDoc`): components, each with ingredients
and steps. Every step lists its inputs (ingredients, earlier steps, by-products
and earlier components), which makes the document a graph. Amounts live on the
inputs, and step text refers to inputs with `{placeholders}`. The list view,
the tabular chart, scaled amounts, times and cook-mode readiness are all
computed from the one document. `recipes_guide` returns the full authoring
reference (`crates/omni-recipes/src/guide.md`); its text is frozen by a digest
test, so changing what agents are told means bumping `guide::VERSION`.

| In the versioned document | Record metadata (not versioned) |
|---|---|
| title, summary, yield, time override, source | tags, state (active, archived, trashed) |
| components, ingredients, steps, inputs | the notes field, images and the cover |
| | the link to the stored source, cook history, `derivedFrom`, `mergedInto` |

- **Content** is immutable per revision. `recipes_edit`, `recipes_replace` and
  `recipes_revert` each write one new revision after checking
  `expectedRevision`. Revisions are kept as long as the recipe exists.
- **Metadata** changes are recorded in a change log with a `before` and an
  `after` snapshot of exactly the facets they wrote (`state`, `tags`, `notes`,
  `images`, `cooks`, the tag registry, and which recipe owns which cooks
  after a merge). `recipes_undo` applies `before`; undoing that
  undo applies it the other way. An undo is refused with `[undo_conflict]`
  when a newer change that still stands wrote the same facet of the same
  recipe. An undo of a tag-registry change puts back only the registry rows
  that change wrote, and is refused when a later change created or used one
  of those names. Content changes and creates are in the log too: undoing an edit
  writes the earlier document as a new revision, and undoing a create sends
  the recipe to the trash. A finished cook is logged but is history, not a
  change: `recipes_undo` refuses it with `[not_undoable]`.
- **Delete** is a state: `trashed` recipes stay restorable for 30 days, then
  `RecipesPurge` removes them. No tool removes data.
- **Tags** come from a registry that agents maintain. Each tag is in one of
  seven groups (`course`, `cuisine`, `main`, `method`, `diet`, `occasion`,
  `other`). Nothing is seeded. A write with an unregistered tag is refused with
  the nearest names unless it passes `createTags` (which registers it in
  `other`).
- **Merging** moves images, cook history and tags to the kept
  recipe (and the stored source, when the kept recipe has none), appends
  notes, and trashes each absorbed recipe with `mergedInto`. A moved cook
  session keeps showing the document it pinned (`recipes_cook.doc_recipe_id`
  names the recipe whose revision that is; `mergedFrom` in tool output) and
  finishes against the kept recipe; a `rebase` moves it to the kept recipe's
  revisions. A cook
  started on a merged-away id uses the kept recipe's newest revision.
  Reading an absorbed id returns the kept recipe with `redirectedFrom`; a write
  to it is refused with `[merged]`. The pointer outlives the purge
  (`recipes_redirect`).

## The household

Two standing rules apply to every recipe and override a source. They are code
(`crates/omni-recipes/src/household.rs`, checks in `omni-recipe-format`'s `diet` and `measure`
modules) and reach every agent through `recipes_guide` (the `household` field and the guide's
"The household" section), the `diet` field of `recipes_import_url` and `recipes_inbox_get`, and
the tool descriptions.

**Diet: bivalve vegetarian.** No meat, poultry, fish or other seafood; bivalves, eggs and dairy
are fine; hidden animal products count (stock, fish sauce, anchovies and Worcestershire, gelatin,
lard, bacon fat). `diet_flag` reads ingredient names against a word list, with a list of words
that mark a vegetarian product ("vegan fish sauce") and of harmless phrases ("beefsteak tomato").
A create, edit or replace that introduces a conflicting ingredient is refused with
`diet_conflict`, one problem per ingredient with a fitting hint. `keepDietConflicts: true` keeps
them, for an ingredient the owner explicitly asked for. A conflict the recipe already had before
the write is a warning, so a recipe with an accepted exception can still be edited; a revert and
an app amount change never refuse. `recipes_get` lists kept conflicts among its warnings. The
check errs towards flagging: a name that trips it by mistake is renamed or kept.

**Amounts that are easy to measure.** Metric with tsp, tbsp and cups, and a kitchen scale: grams
for what is weighed into the bowl, spoons for small amounts, counts, cups or ml only where a
standard measure is natural, rounded to friendly numbers. The guide carries the guideline. The
validator reports an amount off the grid as an `awkward_amount` warning with what to write
instead (`measure::awkward`): grams not on 5 from 20 g up, tsp not in quarters, tbsp not in
halves, more than 4 tbsp measured in one go (an ingredient shared by several steps is judged per
use), cups that are neither quarters nor thirds (a third is stored as 0.3333), ml off 5 up to 60
and off 25 above (cup measures and can sizes pass), and any ounces or pounds. Counts, cans and
the source's `alt` measure are not judged.

A scaled amount is shown in the nearest measurable form (`measure::friendly`, used by
`fmt_amount_at` for every scaled view): spoons in quarters with tsp turning into tbsp, cups as a
standard measure plus spoons ("¼ cup + 2 tbsp"), grams to 5 from 50 g and to 10 from 1 kg, small
millilitres as spoons, and part of a kilogram or litre in grams or millilitres. A stored amount
at scale 1 always reads exactly as written.

## Step data beyond prose

Prose does not follow the scale, so what changes with it is data on the step, rendered
through placeholders. Every field is optional; a document without them reads exactly as before
(the four sample recipes' list, chart and timeline goldens are unchanged). A placeholder name
that is also the id of one of the step's inputs keeps its input meaning, so a document written
before a name became a placeholder reads as it did.

- **Counts** (`text.rs`, `unscaled.rs`). `{yield}` renders the scaled yield with its unit;
  `portions: { qty, unit }` with `{portions}` renders a count the step divides the food into.
  Scaled counts are whole (`format::fmt_count`), with "about" when rounding moves them more
  than a tenth, and the `rescale` operation multiplies `portions`. The validator warns
  `unscaled_number` for a number in step `text`, `until` or `tip`, or in an ingredient's or
  input's `prep` or `note`, that is followed by a counted word: times, temperatures, sizes
  (`mm`, `cm`, `in`, `x`), ratios, repeats (`times`, `sides`), parts of an item (`halves`),
  ordinals, labels (`step 3`, `gas mark 4`) and measured amounts (left to
  `literal_quantity`) are exempt, as are number words. The hint names `{yield}` when the number
  equals the yield, `repeat` for batches, and `portions` otherwise.
- **Heat in stages** (`format::fmt_heat`). `heat: [{ temp? | level?, label? }]`, levels
  `low` to `high` in five steps, at most four stages, exactly one of `temp` and `level` each,
  labels unique, 1 to 20 characters without braces, `|` or `:`. A step has `temp` or `heat`,
  not both. `{heat}` renders the stages joined by ", then ", `{heat:label}` one stage, and the
  chart label shows them short ("high→med", "200→150°C").
- **Batches** (`Step::batches`). `repeat: { count | capacity }`: a count scales and rounds up;
  a capacity divides the scaled yield and rounds up; at least one batch. `timeline_at` and
  `derived_time_at` multiply the step's time by its batches at the scale (`timeline` and
  `derived_time` are scale 1, which is what a recipe's stored totals use, and the list header
  uses the shown scale). The chart label adds "×N" from two batches, `{batches}` renders the
  count, `rescale` multiplies a count, and cook mode labels the step timer "(one of N
  batches)".
- **Variants** (`variant.rs`). `variants: [{ id, name, note?, ops }]`, at most six, each a list
  of at most 50 `VariantOp`s: `set`, `setInput` (never `remove`), `replaceText` and
  `setRecipe` limited to `yield` and `time`. Its own type keeps the schema free of recursion
  (an `Op::SetVariant` holds a `Variant`) and keeps the ids of a variant the recipe's, so a
  cook switches without losing what is crossed off. `variant_doc` applies one to the recipe as
  written; every view of a variant is that plain document. The validator applies each and
  reports the problems its document has that the recipe as written does not, under
  `variants.<id>.`; the household diet is checked over every variant too. `setVariant` and
  `removeVariant` edit them, `rename` rewrites their targets, refs and placeholders, and
  `rescale` multiplies the amounts they set. The text view ends with a Variants section (name,
  note, then only the changed yield and time, ingredient lines and steps, with the chart label
  when it changed); the chart view ends with a "Variants:" line naming what each changes;
  `recipes_get` `variant` renders text or chart as that variant. A cook session records the
  variant as a `variant` event (`CookEventKind::Variant`); `recipes_cook_start` `variant`
  appends one after checking the id, and the session view shows the variant and computes
  readiness on its document. A revision without the session's variant shows as written.
  A variant's amounts are its own: a later change to the amount as written (an edit or the
  app's amount sheet) leaves an amount the variant sets as it was, and the app refuses its
  amount sheet while a variant is shown (`VARIANT_AMOUNT`).
- **Sizes** (`format::fmt_size`). `size: [{ kind, value, max?, by?, unit }]`, kind
  `thickness`, `diameter`, `length` or `pan`, unit `mm`, `cm` or `in`, at most four. `{size}`
  renders all of them, `{size:kind}` the first of that kind, each in both systems (inches to
  the nearest ⅛ below an inch and ½ above, centimetres whole, millimetres below 1 cm). Sizes
  never scale; at a scale other than 1 a pan adds "for the unscaled recipe". Scaling the yield
  by pan area is not done: depth and shape make the ratio unreliable.

## Tools

All 24 are recommended `allow`: each changes only Omni's own recipe data and is
reversible. `recipes_import_url` and `recipes_images_edit` (with `url`) fetch a
public URL the caller supplies.

| Tool | Purpose |
|---|---|
| `recipes_guide` | Authoring guide, format, units, tag vocabulary with counts, library counts, household rules |
| `recipes_search` | Text, tags, ingredients, state, time, images (`placeholderImage`), cook history; cards with paging |
| `recipes_get` | One recipe as `document`, `text` or `chart`, at any scale or revision; `ids` marks text and chart with the ids edits address |
| `recipes_history` | A recipe's revisions and diffs, or the library change log |
| `recipes_image_get` | One recipe image as an MCP image block |
| `recipes_create` | Create from a document; links and compares the source |
| `recipes_edit` | Id-addressed operations as one new revision; its text carries the ids |
| `recipes_replace` | Whole document as one new revision |
| `recipes_revert` | An earlier revision as the newest |
| `recipes_organize` | Tags and state on up to 200 listed recipes |
| `recipes_tags_edit` | Create, rename, merge, regroup and delete tags |
| `recipes_merge` | Fold duplicates into one recipe |
| `recipes_undo` | Reverse one logged change from the last 30 days |
| `recipes_images_edit` | Add (URL, inbox photo, base64), remove, reorder, choose the cover |
| `recipes_import_url` | Fetch a page into the inbox; returns what was found |
| `recipes_inbox_list` | Captured material waiting for an agent |
| `recipes_inbox_get` | One inbox item: text, structured data, photos as image blocks |
| `recipes_inbox_resolve` | Mark items discarded or duplicate |
| `recipes_cook_start` | Start a cook session at a scale, optionally as a variant |
| `recipes_cook_get` | Active sessions, one session's state, or a recipe's cook log |
| `recipes_cook_update` | Append events: check, uncheck, note, timer, cancelTimer, rebase, variant, finish, abandon |
| `recipes_requests_list` | Requests the owner wrote in the app, by status and recipe, oldest first |
| `recipes_request_update` | A request's status, note and the revision that answers it |
| `recipes_pins` | The pinned recipes in order: read, or set the whole list |

Behaviour every tool keeps:

- Unknown input fields are refused; optional output fields are omitted, never
  `null`.
- Every tool that changes a recipe or the tag registry takes `dryRun`. A dry
  run executes the real write in a transaction and rolls it back, so it
  reports exactly what the write would.
- A document that breaks the format is refused with **every** problem at once
  (`[invalid_recipe]`, the text of `Report::refusal_text`); with `dryRun` the
  same list is a normal result (`saved: false`, `problems`).
- A dry run reports everything the real call would refuse, in that one list:
  beside the document's problems, each tag the registry lacks (`unknown_tag`
  at `tags.<name>`, nearest names in the hint) and a source or document that
  already exists (`duplicate_source` at `source`), unless `createTags` or
  `allowDuplicate` waives them. `recipes_organize` with `dryRun` previews the
  change and lists the unknown tags in `problems`.
- Amounts read exactly as stored at scale 1 (`326 g`, metric decimals to one
  place). Rounding to something a cook can weigh applies only to scaled
  amounts, and never to an ingredient with `scales: false`.
- Results say what changed: an edit's `changes` shows a rename as
  `~ rename oil → olive_oil`, `recipes_undo` returns the recipes it wrote
  (`ids`) and, for a content change, the new `revision` and the rendered
  `text`, `recipes_history` library entries carry up to 20 `recipeIds`, and
  `recipes_inbox_list` gives the `url` of a page item.
- Two warnings ask for what a recipe is usually missing: `missing_time` (a
  step without a time) and `missing_source_url` (a `web` source without its
  address). Warnings never block a save.
- Every content write returns the rendered recipe (`text`), so the caller can
  compare it with the source without a second call.
- Bulk work takes explicit ids. There is no "everything matching".
- Every write that is logged returns its `changeId` (create, edit, replace and
  revert included), which `recipes_undo` takes.
- `recipes_search` refuses a tag filter that cannot be a tag name and returns
  filters that name no registered tag in `unknownTags`, with nearest names.
- Titles, notes, cook notes, inbox titles and page text are
  written in the app (which has no login) or taken from web pages. Tool
  descriptions mark them as untrusted data, never instructions; the review's
  `issues` are model text derived from such a source.
- A duplicate is found by the document's `source.url` or by the URL the inbox
  item was read from; a revision that changes `source.url` is found by the
  new one. `source.url` must be an `http(s)` address (the format refuses
  anything else, and the app renders a link only for one).

### Errors

A failed call is text that starts with a code:

| Code | When |
|---|---|
| `invalid_recipe` | The document breaks a format rule; every problem is listed with path, code and hint |
| `revision_conflict` | `expectedRevision` is stale; names the current revision and what changed since |
| `op_failed` | An edit operation cannot apply: `ops[1] (set): ...` |
| `duplicate_source` | A create matches an existing recipe's source URL, inbox item or exact document |
| `unknown_tag` | Tags are not in the registry; lists every one with its nearest names |
| `not_found` | Unknown recipe, revision, image, inbox item or session |
| `trashed` | A write names a recipe in the trash; restore it with `recipes_organize` or `recipes_undo` |
| `not_undoable` | `recipes_undo` on a finished cook |
| `too_many` | A limit of the open app surface: 20 open cook sessions, 30 timers in a session, 200 waiting inbox items |
| `unavailable` | Image processing is busy and timed out; try again |
| `merged` | A write names a recipe that was merged into another; names where it lives now |
| `undo_conflict` | Something newer touched what the change touched, or it was already undone |
| `too_large`, `unsupported_media` | Image input; the limit and the detected type |
| `fetch_failed` | URL import or image fetch: the host refused, timed out, returned nothing usable, or the fetcher is not running |
| `session_closed` | A cook update on a finished session |
| `validation` | Any other input the schema cannot express |

## Importing

Every recipe enters the same way: the source is stored as an inbox item, an
agent drafts a document, and `recipes_create` validates it and compares it with
the stored source. Inbox kinds are `url`, `text` and `photo`.

- **URL.** `recipes_import_url` gets the page from the `PageFetcher` port
  (`omni_presspods::page_fetch`, the PressPods retrievers through the
  SSRF-guarded client); `omni-recipes` has no fetcher of its own, and an unset
  port is `[fetch_failed]`. From the candidates it takes the first schema.org
  `Recipe` JSON-LD object any HTML copy holds (`jsonld.rs`: `@graph`, arrays,
  `HowToStep` and `HowToSection` flattening, ISO 8601 durations, every image
  shape), looking at the page itself and its browser render before the
  Wayback copy, and for `text` the longest main-content extraction
  (readability, postlight) before whole-page text, with leading navigation
  lines removed (HTML is converted when no source extracted text).
  `render: "browser"` prefers the `obscura` copy. A candidate that is a bot
  challenge (titles such as "Just a moment...", "Attention Required" or
  "Access denied", or challenge and captcha markers in the page) is skipped;
  when every candidate is a challenge, or none has Recipe data or 500
  characters of text, the import fails with `[fetch_failed]` and stores no
  inbox item. Addresses taken from a Wayback copy are unwrapped
  (`https://web.archive.org/web/<stamp>/<original>` becomes the original) in
  `structured.images`, the source URL and the lead-image fetch, which falls
  back to the archived address. The lead image is fetched through
  `ctx.public_http` with a 15 MB cap; its failure never fails the import, and
  the result says `imageStored: false` with an `imageReason`.
- **Text.** The agent passes the original as `sourceText`; it is stored as a
  `text` item already linked to the new recipe.
- **Photos.** The app uploads one or more photos as one item (`POST
  /api/inbox`). The item becomes visible to agents, and `recipes.inbox_added`
  fires, when its last photo has arrived. `recipes_inbox_get` returns the
  photos as JPEG image blocks, rotated upright and bounded by `maxDimension`,
  four per call.

Page text, structured data and photos are untrusted source material. They are
bounded, never executed and never treated as instructions; the tool
descriptions and the guide say so.

### Fidelity

`fidelity` on a create or replace result has two parts. Neither ever blocks a
save.

1. **The deterministic comparison** (`fidelity.rs`). For each ingredient line
   of the source (JSON-LD `recipeIngredient`, or the ingredient lines of pasted
   text) it finds a document ingredient that shares head words and checks that
   one of the line's amounts equals the ingredient's `qty` or `alt` after unit
   normalisation (g and kg, ml and l, tsp, tbsp and cup, oz and lb; never
   volume against mass). It reports source lines with no ingredient, document
   ingredients the source never names, and amounts that differ. Every
   temperature and duration in the source instructions must appear in some
   step's `temp`, `time` or text. Photo sources report `checked: false, reason:
   "photo source"`.
2. **The extraction review** (`review.rs`), the only model call in the crate.
   When a stored source exists it sends the source (text, or up to six photos)
   and the rendered recipe to the `RecipeReview` model role (code default, no
   env override) with a strict output schema, one attempt, 45 seconds. The
   result is `fidelity.review: { ran, issues: [{ severity, text }], model }`.
   A failure, a timeout or missing configuration is `ran: false` with a reason.
   A dry run reviews only with `review: true`; `review: false` skips it. Cost
   is recorded through the normal cost recorder (feature `recipes`). Source
   text is never logged.

## Cook sessions

A session pins one revision and one scale and is an append-only event log
(`recipes_cook_event`) that the app and agents both write. Each event carries a
client-chosen `clientEventId` and is applied once. Views are projections of the
log: steps done and ready, gathered ingredients, running timers and notes.

- A session holds no recipe text of its own. A change made while cooking is
  a change to the recipe: the app writes an amount as a revision (below) and
  an agent uses `recipes_edit`; a `rebase` event then moves the session to
  the new revision. The session's scale is how it is shown and never becomes
  a recipe change.
- `finish` writes the cook log (cook count, last cooked).
  `recipes.cook_finished` fires.
- A session idle for 18 hours closes itself. It counts as a cook
  (`finished`) when at least half its steps were done, otherwise it is
  `abandoned` with no cook log.
- `abandon` discards a session: it closes as `abandoned` at once, with no
  cook counted, no `recipes.cook_finished` and its running
  timers dropped without an alert. Later events are answered as on any closed
  session.
- `recipes_cook_get` with a `recipeId` lists each cook with the notes taken
  during it (`notes`) as well as the finish note.
- `rebase` moves a session to a newer revision; state carries over by id.
- Sessions used to take text overrides (`adjust`, `unadjust`) that became
  adjustment records on the recipe when the cook finished
  (`dismissAdjustment` closed one). That is gone. The cook log is
  append-only, so those events stay in `recipes_cook_event` and every reader
  skips them (`CookEventKind::Unknown`); the app route accepts and drops them
  from an app that has not updated yet, so the rest of its batch still
  lands. The tools no longer have these event types, `resolves` or any
  adjustment output. Schema version 3 drops the `recipes_adjustment` table;
  no adjustment was open in production when it ran. An archive written
  before it still restores, without that table's rows, and an older change
  that mentions adjustments can still be undone.

### Amounts changed in the app

The app makes one content edit: the amount of one ingredient
(`ClientOpKind::Amount` through `POST /api/ops`, applied once by op id). It
carries a quantity (finite, above 0, at most 100000) and a unit (one of the
known units, the ingredient's current unit, or none for a plain count), and
nothing else: no names, no steps, no ingredients added or removed, and an
ingredient without a quantity (to taste) cannot be given one.

- The change is built as ordinary edit operations
  (`omni_recipe_format::amount_ops`) on the head revision and written by the
  function `recipes_edit` uses: validated, a new revision with tool `app` and
  a summary such as "App: kosher salt 2 tsp → 1 tsp", the search index, the
  library sequence and a change-log entry, so `recipes_history` shows it and
  `recipes_undo` takes it back.
- With the unit unchanged, the amounts steps take explicitly (`qty` on an
  input) and the `alt` measure are multiplied by the same ratio, so a split
  still adds up; `fraction` and `rest` uses need nothing. A new unit drops
  `alt`, and is refused when any step takes the ingredient by amount.
- `baseRevision` is the revision the cook was looking at. When the recipe has
  moved on, the change still applies if that ingredient's quantity and unit
  are what they were in the base revision; otherwise it is refused with "the
  recipe changed; reload and try again".
- A refusal is `ok: false` with one line to show the cook (for a document the
  validator refuses, its first problem) and the recipe as it is, so the app
  can put back what it showed. Nothing is written.
- The app moves its own open sessions along: one that shows the revision
  the change replaced, and the session the change was made from, get a
  `rebase` event queued behind the amount. A session left on an older
  revision stays there, as does any session after an agent's edit, until it
  is rebased.
- The guide tells agents that an `app` revision is the owner's own decision:
  build on it, and do not revert it unless asked.
- A finished session refuses an agent's events with `[session_closed]`. The
  app's late events are dropped instead, because its outbox cannot send them
  again differently.
- **Timers** are stored as end times. The timer service
  (`BackgroundService` `recipes-timers`, every five seconds) reserves a row in
  `recipes_timer_alert` before sending one notification through
  `ctx.pushover` (the iOS app first), and never sends it again, whatever the
  outcome. With no app client on the event stream the alert goes out when the
  timer ends. With a client connected the app rings by itself, and the alert
  goes out only if the timer is still unanswered (not cancelled, its step not
  crossed off) 30 seconds later: a phone that slept with the stream open must
  not swallow the alert. A timer that ended more than ten minutes ago (the
  server was down) is recorded as missed and not announced.

## Pinned recipes

The owner pins recipes to the top of the app's library and orders them by hand. The list is one
ordered thing, stored in `recipes_pin` (`pins.rs`), so every device and every agent sees the
same one.

- The app sends the whole list (`ClientOpKind::Pins { ids }` through `POST /api/ops`): pinning,
  unpinning and reordering are all "the list is now this". A recipe that was merged away is
  pinned as the recipe it became. The app's list is taken as far as it goes (a recipe the
  library no longer has is left out, the 61st is dropped), because the app sends it from what
  it holds, possibly offline, and cannot send it again differently; an agent's list with such
  an id is refused. At most 60.
- Each recipe the app syncs carries `pinned`, its stored position (0 first). Only the recipes
  whose stored position changed are stamped with the new sequence number, so another device
  syncs just those. Positions are counted afresh only when the list is set, never behind a
  recipe's back: a pinned recipe that goes to the trash leaves a gap, the ones behind it keep
  the position every device already has, and the app orders by position.
- Brought back from the trash before the list changes again, a recipe is where it was.
  Archived recipes keep their pin; the app shows only active ones.
- `recipes_pins` reads the list, or with `ids` sets it (dry run, `changeId`). `recipes_get` and
  `recipes_search` show `pinned`, the place among the pinned recipes still in the library (1
  first). Every change is one change-log entry on the key
  `*pins`, so `recipes_undo` puts the earlier list back and refuses when a newer pin change
  stands.

## Requests from the app

The owner writes a request to the kitchen agent in the app ("Ask Kitchen…" in the overflow menu
of a recipe, of cook mode and of the library): a text field and Submit. Fond holds no
conversation. It stores the request, announces it, and shows where it stands; every question
and answer happens in the agent's own chat with the owner.

- **Stored first.** A request is a row of `recipes_request`, written by `ClientOpKind::Request`
  through `POST /api/ops` (so it rides the app's offline outbox) before anything is announced.
  The library is the source of truth: a webhook that is down, misconfigured or never set up
  loses nothing. The id is chosen by the app (`req_` and eight characters) and stored once,
  whichever op id carries it.
- **Context.** `origin` (`recipe`, `cook`, `library`), the recipe, the revision on screen, the
  scale it was shown at, and in cook mode the step that was open. A library request has none.
  Context that does not check out is dropped, never a reason to lose the text: an unknown
  revision or a scale out of range goes, and a request about a recipe that no longer exists
  (it waited offline) is kept as a library request. An empty text, more than 2,000 characters
  or a malformed id refuses the op (`ok: false`, one line for the cook). At most 100 requests may be open at once, since the
  app has no login.
- **Status.** `queued`, `working`, `done`, `declined`, set only by an agent with
  `recipes_request_update`, with an optional note of at most 300 characters and the revision
  that answers it (`resultRevision`; `resultRecipeId` when a new recipe answers a library
  request, and then its newest revision when none is given). The tool changes only the request:
  the recipe change is an ordinary `recipes_edit`, `recipes_replace` or `recipes_create`, with
  the household diet and measuring checks unchanged. `recipes_guide` carries the flow
  ("Requests from the app").
- **The notice.** When `RECIPES_REQUEST_WEBHOOK_URL` is set, the `recipes-requests` service
  posts one JSON event per request to it (`requests.rs`):

  ```json
  {
    "event": "recipe.request",
    "requestId": "req_7h2k9m4x",
    "text": "Halve the butter next time.",
    "createdAt": "2026-11-19T04:12:09.120Z",
    "recipeTitle": "Cauliflower Piccata",
    "link": "https://recipes.example.com/r/rcp_rydc5v1b",
    "context": { "origin": "cook", "recipeId": "rcp_rydc5v1b", "revision": 4, "scale": 1.5, "step": "sear" }
  }
  ```

  `recipeTitle` and the context fields are `null` when the request has none; a library request
  links to `/requests`. Headers: `Content-Type: application/json`, `X-Fond-Event:
  recipe.request`, `Idempotency-Key: <requestId>` (the same on every attempt, so the receiver
  collapses a resend), and `Authorization: Bearer <RECIPES_REQUEST_WEBHOOK_TOKEN>` when that is
  set. There is no body signature: the bearer key is the authentication, as on the Claude
  session notify targets. `RECIPES_REQUEST_WEBHOOK_CONNECT` connects to another host than the
  URL's (a reverse proxy on a Docker network) while keeping the URL's host for TLS and `Host`.
  The client is the LAN-capable one (the address is configuration, never app input), follows
  no redirect, waits ten seconds, and in `SideEffectMode::Record` sends nothing.
- **Retries.** An attempt is reserved (`attempts`, `next_attempt_at`) before it is sent. No
  answer, a timeout, 408, 425, 429 and 5xx are retried after 30 seconds, doubling to 30
  minutes, for 24 hours from the request; then, or at once on any other 4xx, a redirect or an
  answer longer than 64 KiB (some other page, not a webhook's answer), the notice is `failed`
  and logged at WARN. A 2xx whose body says `"ignored": true` (the bot has no
  webhook routine) settles as `ignored`. A request an agent already took is `skipped`. Whatever
  the notice's fate (`delivery` in the tool output: `pending`, `delivered`, `ignored`,
  `failed`, `unconfigured`, `skipped`), the request stays `queued` until an agent takes it,
  and `recipes_requests_list` returns it. Without the URL the service logs at boot that the
  webhook is disabled and each request is stored as `unconfigured`; when the service later
  starts with a URL, the `unconfigured` requests still `queued` are announced then.
- **In the app.** `GET /api/requests` lists the newest 200; an `/api/events` frame named
  `requests` says the list changed. Request writes bump `cook_seq`, not the library sequence:
  no device syncs its library over them, and the hourly archive still sees them.
- **Kept.** `RecipesPurge` removes a request 30 days after it was closed (`done` or
  `declined`); an open one stays. Request text is written in an app without a login: the tool
  descriptions mark it as untrusted, to be acted on only as a recipe change.

## The Crouton import's history (removed)

The Crouton import of 2026-11-18 left three traces of itself on each recipe: `kind: "crouton"`
as the document's source (with the name "Crouton" where the crumb named no other source), a
block appended to the notes ("Imported from Crouton on …" and a "Changes: …" paragraph), and
the Crouton text as the stored original. A one-time step at boot removes them
(`Recipes::remove_import_history`, guarded by `recipes_meta.import_history_removed`):

- The source becomes `web` when it has a page address, `person` when only a name, author or
  detail is left, otherwise `original`; "Crouton" as a name goes. One ordinary revision per
  recipe, tool `system`, summary "Remove Crouton import provenance". Nothing else in the
  document changes.
- The notes lose the block and keep everything the owner wrote before (and after) it.
- The link to the stored Crouton text is dropped and the inbox item set to `discarded`, so the
  purge removes it 30 days later.
- The notes and the link are one change-log entry per recipe, whose snapshot also holds the
  inbox item's state: `recipes_undo` brings both back within 30 days, and undoing the revision
  entry brings the source back.

The guide now tells agents not to write such history into a recipe: no import date, change list
or swap explanation in notes, summary, source or ingredient notes (that is what the revision
summary and `recipes_history` are for), and `crouton` is a retired source kind that is read
and never written. Once the step has run in production its code can be deleted; the `crouton`
kind must stay readable for the old revisions.

## Storage

Relational tables in `docstore.db`, written through `store.write`
(`schema.rs`). Ordered, idempotent migrations keyed by
`recipes_meta.schema_version` run in a `BootPhase::Migrate` step; every tool
also ensures them on first use, and a database from a newer build is refused.

| Table | Holds |
|---|---|
| `recipes_recipe` | One row per recipe: head revision, state, notes, cover, source, counters, `seq` |
| `recipes_revision` | Every revision's document (JSON, with a `format` number); append-only |
| `recipes_tag`, `recipes_recipe_tag` | The registry and its use |
| `recipes_image` | Image rows: digest, size, role, caption, position |
| `recipes_inbox`, `recipes_inbox_file` | Source snapshots and their photos |
| `recipes_cook`, `recipes_cook_event` | Sessions and their append-only events |
| `recipes_change` | The change log: tool, summary, touched facets, before, after |
| `recipes_client_op` | Ids of the app's edits already applied |
| `recipes_redirect` | Where a purged, merged-away recipe lives |
| `recipes_request` | Requests written in the app, their status and outcome, and the state of each one's webhook notice |
| `recipes_pin` | The pinned recipes and their order |
| `recipes_timer_alert` | One row per ended timer: the alert reservation |
| `recipes_meta` | `schema_version`, `seq`, `cook_seq`, backup bookkeeping |
| `recipes_fts` | FTS5 index |

- Triggers abort any `UPDATE` of a revision and any `DELETE` unless the recipe
  is in the trash. Old formats are converted as they are read
  (`omni_recipe_format::upgrade`), never rewritten.
- `seq` is a library-wide counter bumped by every change. The app syncs with
  "everything after `seq` N". Cook events bump `cook_seq` instead, so crossing
  a step off does not make every device sync.
- Ids are a prefix (`rcp_`, `chg_`, `inb_`, `cook_`, `img_`, `adj_`, `req_`) and eight
  lowercase base32 characters. The app chooses a session's `cook_` id and a
  request's `req_` id.

### Search

`recipes_fts` is an FTS5 table (`unicode61 remove_diacritics 2`, prefix
indexes) with columns `title`, `ingredients`, `tags`, `body` (summary and step
text), `notes`, `source`, and `folded`, which holds every word as the shared
tokeniser of `omni-recipe-format` cuts it. A recipe's row is rewritten in the
transaction of any change to its head revision, tags or notes. The tool cuts
the query into folded words itself: each word is a prefix term, a quoted run is
a phrase, all are required, ranked by `bm25` weighted towards title,
ingredients and tags. No part of the caller's query reaches FTS as syntax. The
app does not call the server to search; it indexes its synced copy with the
same tokeniser.

### Images

- Type is decided from magic bytes: JPEG, PNG, WebP or HEIC. Decoding goes
  through `omni-image` (pixel and allocation caps, a deadline, one decode at a
  time).
- The original bytes are stored once, content-addressed, at
  `<recipes_dir>/images/sha256/<aa>/<digest>`, with `<digest>.thumb.jpg` (480
  px) and `<digest>.full.jpg` (1600 px) beside it and an average colour in the
  row. Files are written under a temporary name and renamed; no final name
  ends in `.tmp`, `.part` or `.log`, which the off-site backup skips.
- Base64 in a tool call is limited to 3,000,000 decoded bytes and must match
  its `mimeType`; an uploaded photo to 25 MB; an image fetched from a URL to 15
  MB.
- Removing an image removes the row. The file stays while any recipe, inbox
  item or change-log entry from the last 30 days refers to it, so an undo can
  put it back; after that `RecipesPurge` deletes it. Files modified in the last
  day are never purged.
- Every recipe should have a picture of the dish (role `dish`). `recipes_create`
  and `recipes_replace` return a `missing_image` warning without one (reported
  by the library, like `diet_conflict`; it never blocks). The guide's order of
  preference is the owner's photo, the source page's lead image, then a freely
  licensed image captioned `Placeholder: <author, source, licence>`.
- A placeholder is only its caption: an image whose caption starts
  `Placeholder:` (any case) reads `placeholder: true` in `recipes_get`, a card
  whose cover is one has `coverPlaceholder`, and `recipes_search`
  `placeholderImage` filters on the cover (`db::PLACEHOLDER_COVER_SQL`, the
  same rule as `db::is_placeholder`). Nothing is stored for it.
- URL fetches go through the public client, which follows up to 10 redirects
  and checks every hop, so a Commons `Special:FilePath` link reaches the file.

## The app's HTTP API

`http.rs` builds one router that is registered only as
`Subsystem::host_apps`; it is never merged into Omni's router. It exists when
`RECIPES_APP_URL` is set.

| Route | Purpose |
|---|---|
| `GET /api/sync?since=<seq>` | The library after `seq` (or all of it), `removed` ids, tags, open sessions |
| `GET /api/events` | SSE: `hello`, then `library`, `session` and `requests` frames, with pings |
| `POST /api/ops` | Notes, state and amount edits, the pinned list and requests, each applied once by id |
| `GET /api/requests` | The requests written in the app, newest first |
| `POST /api/cook`, `GET /api/cook/:id` | Start (idempotent by id) and read a session |
| `POST /api/cook/:id/events` | Append events |
| `POST /api/cook/:id/photo?id=` | The photo of the result, as an image of the recipe; stored once per upload `id` |
| `GET /api/inbox`, `POST /api/inbox?item=&index=&count=&title=` | List; upload one photo |
| `GET /api/original/:recipeId` | The stored source |
| `GET /img/:digest/:variant.jpg` | `thumb` or `full`, immutable; only digests a row refers to |
| `GET /src/:itemId/:n.jpg` | Photo `n` of an inbox item |
| `/`, `/r/:id`, `/cook/:id`, `/inbox`, `/requests` | `index.html` (`no-cache`) |
| `/assets/*`, `/icons/*`, `/sw.js`, `/manifest.webmanifest` | The app's files |

- The static files come from `AppPaths::recipes_web_dist`
  (`OMNI_RECIPES_WEB_DIST`; `/app/recipes-web` in the container,
  `crates/omni-recipes-web/dist` otherwise). `/assets/*` is immutable, the
  service worker and the manifest are `no-cache`, and a precompressed `.br` or
  `.gz` sibling is served when accepted. When the directory is missing the API
  still works and `/` answers 503 with a plain message.
- `/assets/snippets/*` (wasm-bindgen snippets, whose path does not change
  when the file does) is `no-cache`; everything else under `/assets` is
  immutable.
- There is no catch-all. Any other path is a 404, as `{"error": "Not found"}`
  under `/api`.
- The surface is bounded, because it has no login: 20 open cook sessions and
  200 waiting inbox items (429 beyond), 30 timers in a session, 16 open event
  streams (the 17th gets 503; none is dropped), two uploads and 32 other
  requests in flight at once (503 with `Retry-After` beyond), and at most ten
  timer alerts to the phone in any hour (the rest are recorded as suppressed
  and never sent later).
- Every response carries a Content-Security-Policy for a wasm app that loads
  only its own files (`script-src 'self' 'wasm-unsafe-eval'`; no inline
  script).
- Mutations pass `MutationGuard`: the exact `Origin`, `Sec-Fetch-Site:
  same-origin` when present, and `Content-Type: application/json`. The two
  upload routes take the `X-Fond-Upload` header instead, which a page on
  another origin cannot send without a CORS preflight that Fond never answers.
- A notes edit carries the revision it was made from. A stale one never
  overwrites: both texts are kept, the server's first (or the longer one alone
  when it contains the other). Nothing is cut: when the two do not fit in
  20,000 characters the server's text stays and the op answers `ok: false`.
  A state change on a merged-away recipe answers `ok: false` (`[merged]`); a
  note follows the recipe to where it lives. The app's notes and
  state changes are in the change log, so an agent can undo them.
- Every cook session the server returns carries `doc`, the pinned revision's
  document, so any device renders the recipe the cook started on.
- `build` in `hello` and in sync is the running image revision
  (`OMNI_IMAGE_REVISION`, else a fingerprint of the executable): the value
  Omni's `/api/snapshot` reports as `build.server`. The app checks for a new
  shell when it changes.
- The app retries no answer, 5xx, 408, 425 and 429, and drops an outbox entry
  on any other 4xx. So a refusal that can never succeed is a 4xx, a server
  fault is a 500, and events that arrive before the start of their session
  (the app starts sessions offline) are 425.
- An in-process hub (`hub.rs`) feeds the event stream. Tools and the app's own
  writes both notify it after their transaction commits; a dry run notifies
  nothing. A receiver that falls behind ends its stream, and the app starts
  again from `hello`.

### Host isolation

Omni has one listener and learns the host only from `Host`.
`omni_server_kit::host_app` dispatches every request whose host equals the
app's to the app's router and nowhere else, outside every other layer. So on
the recipes host only Fond exists (no Omni UI, no `/gateway`, no `/api/data`),
and on every other host Fond does not exist. The gateway host can never serve
it: Fond is reachable only through the host match; the gateway host answers 404
for every path but its own; the proxy forwards only those paths; and boot
refuses a `RECIPES_APP_URL` whose host equals the gateway's or PressPods'
public host. `crates/omni/tests/route_precedence.rs` holds the matrix against
the real router.

Two refusals come before the app, for any request that names its host. A
request that names two different hosts (two `Host` headers, or an
absolute-form URI that disagrees with `Host`) is a 400: no proxy and Omni can
disagree about where it goes. A request that carries `CF-Connecting-IP` or
`CF-Ray` is a 403: the app's DNS record is private, so a request that came
through Cloudflare means that record or the proxy's access list is gone, and
the app closes instead of answering the world. Boot also refuses a
`RECIPES_APP_URL` on the host of any origin Omni itself is served on
(`OMNI_GATEWAY_PUBLIC_URL`, `PRESSPODS_PUBLIC_URL`,
`ICLOUD_REMINDERS_PUBLIC_ORIGIN`, `RECS_PUBLIC_URL`, `YOUTUBE_REDIRECT_URI`).

`RECIPES_APP_URL` must be an `https://` origin. Outside the container,
`http://localhost[:port]` and `http://<name>.localhost[:port]` are accepted for
local runs.

There is no login. Anyone on the LAN or the VPN can read the recipes and do
what the app can do: change a recipe's state and notes, change the amount of
one ingredient, run cook sessions, upload to the inbox and write a request for
the kitchen agent. The recipe writes are logged and reversible; a request
changes nothing until an agent acts on it.

### Running it locally

```bash
(cd crates/omni-recipes-web && trunk build)          # the app; optional for the API
RECIPES_APP_URL=http://fond.localhost:8099 cargo run -p omni -- --preview --port 8099
```

Open `http://fond.localhost:8099/`. The preview uses a throwaway database and
recipes directory and seeds the four sample recipes, a small three-step one and ten tags. The first
(Chicken Piccata) also has notes, a stored source text, and one finished cook
with a drawn placeholder photo and a note, and one photo waits in
the inbox, so every screen of the app has something to show
(`crates/omni/src/preview_recipes.rs`). `http://localhost:8099/` is still the
Omni preview. Of the background services the preview starts only
`recipes-timers` and `recipes-requests`, so idle sessions close, ended timers
are recorded (`recipes_timer_alert`, state `unrouted`) and a request's notice
is settled; nothing is sent in a preview.
Fetching is offline in a preview, so URL import answers `[fetch_failed]`.
The app's browser tests run against it with
`FOND_BASE_URL=http://fond.localhost:8099` (`docs/recipes-app.md`).

## Events

| Event | Fires | Dedup key | Polling |
|---|---|---|---|
| `recipes.inbox_added` | An item captured in the app became complete | `inbox:<itemId>` | `recipes_inbox_list` |
| `recipes.cook_finished` | A session finished and counted as a cook | `cook:<sessionId>` | `recipes_cook_get` |

Both are published through the `EventPublisher` port after the transaction
commits. `recipes_import_url` does not publish `recipes.inbox_added`: the agent
that imported the page is already working on it. See
[MCP Events](mcp-events.md).

## Backup and restore

The off-site backup copies the live SQLite file without a consistent snapshot,
so the library keeps its own archive.

- **`RecipesBackup`**, hourly at minute 37, runs only when `seq` or `cook_seq`
  moved. It writes `<recipes_dir>/archives/fond-<seq>-<time>.json.gz`: every
  row of every recipe table as plain JSON with a SHA-256 per table and the
  list of image digests. It reads the file back, compares it with what it
  wrote, and keeps the newest 72 by the time in the file name. Applied
  app-edit ids (`recipes_client_op`) are archived too, so a restored library
  still applies each of the app's edits once.
- **Images** need no archive: they are immutable files written atomically, so
  a file-level snapshot of them is always consistent.
- Before **`RecipesPurge`** removes any recipe for good it writes a verified
  archive of the whole library to `<recipes_dir>/archives/purge/`, outside the
  hourly rotation, kept 180 days, and logs how many recipes it removed. A
  stored source shared by another recipe stays.
- **`RecipesPurge`**, daily at 04:12, removes recipes trashed 30 days ago
  (keeping the pointer of a merged-away recipe), change-log entries and applied
  app-edit ids older than 30 days, requests closed 30 days ago, inbox items
  discarded 30 days ago, uploads that never completed, and image files nothing
  refers to.
- **`omni recipes-verify <archive> [--dir DIR]`** checks every table's
  checksum and, when the recipes directory exists, that every image the
  archive refers to is stored there intact.
- **`omni recipes-restore <archive> [--db PATH] [--dir DIR]`** restores into
  an **empty** library only. It commits only when the restored tables read
  back exactly as the archive holds them, then rebuilds the search index.
  Image files are restored separately, from the file-level backup of
  `<recipes_dir>/images`.

Worst case is about 80 minutes of agent edits: up to an hour since the last
archive plus the off-site backup's interval. The archive is also the export.

## Invariants

- No tool removes data: content has revisions, metadata has 30 days of undo,
  delete is a 30-day trash, and only `RecipesPurge` empties it.
- A revision is never rewritten. Old formats are upgraded on read.
- A dry run changes nothing, announces nothing and reviews nothing unless
  asked.
- Fidelity and the review report; they never block a save.
- The review is the only model call; it never sees or logs more than the one
  source and its rendering, and source text is never logged.
- Page fetching goes only through the `PageFetcher` port; image fetches only
  through the SSRF-guarded public client.
- Fond's router is only a host app. Its mutations need the app's own origin.
- A timer alert is reserved before it is sent and is never sent twice.
- Events are published after commit, once per item or session.
- A request is stored before its notice is sent, and no failure of the notice
  changes or loses the request.
- A restore needs an empty library and an archive that verifies.
