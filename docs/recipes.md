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
| | the link to the stored source, cook history, open adjustments, `derivedFrom`, `mergedInto` |

- **Content** is immutable per revision. `recipes_edit`, `recipes_replace` and
  `recipes_revert` each write one new revision after checking
  `expectedRevision`. Revisions are kept as long as the recipe exists.
- **Metadata** changes are recorded in a change log with a `before` and an
  `after` snapshot of exactly the facets they wrote (`state`, `tags`, `notes`,
  `images`, `cooks`, the tag registry, and which recipe owns which cooks and
  adjustments after a merge). `recipes_undo` applies `before`; undoing that
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
- **Merging** moves images, cook history, adjustments and tags to the kept
  recipe (and the stored source, when the kept recipe has none), appends
  notes, and trashes each absorbed recipe with `mergedInto`. A moved cook
  session keeps showing the document it pinned (`recipes_cook.doc_recipe_id`
  names the recipe whose revision that is; `mergedFrom` in tool output) and
  finishes against the kept recipe; a `rebase` moves it to the kept recipe's
  revisions. A moved adjustment whose step or ingredient is not in the kept
  recipe stays listed with its own text, flagged `targetMissing`. A cook
  started on a merged-away id uses the kept recipe's newest revision.
  Reading an absorbed id returns the kept recipe with `redirectedFrom`; a write
  to it is refused with `[merged]`. The pointer outlives the purge
  (`recipes_redirect`).

## Tools

All 21 are recommended `allow`: each changes only Omni's own recipe data and is
reversible. `recipes_import_url` and `recipes_images_edit` (with `url`) fetch a
public URL the caller supplies.

| Tool | Purpose |
|---|---|
| `recipes_guide` | Authoring guide, format, units, tag vocabulary with counts, library counts |
| `recipes_search` | Text, tags, ingredients, state, time, images, cook history; cards with paging |
| `recipes_get` | One recipe as `document`, `text` or `chart`, at any scale or revision |
| `recipes_history` | A recipe's revisions and diffs, or the library change log |
| `recipes_image_get` | One recipe image as an MCP image block |
| `recipes_create` | Create from a document; links and compares the source |
| `recipes_edit` | Id-addressed operations as one new revision |
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
| `recipes_cook_start` | Start a cook session at a scale |
| `recipes_cook_get` | Active sessions, one session's state, or a recipe's cook log |
| `recipes_cook_update` | Append events: check, adjust, note, timer, rebase, finish, dismiss |

Behaviour every tool keeps:

- Unknown input fields are refused; optional output fields are omitted, never
  `null`.
- Every tool that changes a recipe or the tag registry takes `dryRun`. A dry
  run executes the real write in a transaction and rolls it back, so it
  reports exactly what the write would.
- A document that breaks the format is refused with **every** problem at once
  (`[invalid_recipe]`, the text of `Report::refusal_text`); with `dryRun` the
  same list is a normal result (`saved: false`, `problems`).
- Every content write returns the rendered recipe (`text`), so the caller can
  compare it with the source without a second call.
- Bulk work takes explicit ids. There is no "everything matching".
- Every write that is logged returns its `changeId` (create, edit, replace and
  revert included), which `recipes_undo` takes.
- `recipes_search` refuses a tag filter that cannot be a tag name and returns
  filters that name no registered tag in `unknownTags`, with nearest names.
- Titles, notes, cook notes, adjustment text, inbox titles and page text are
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
| `not_found` | Unknown recipe, revision, image, inbox item, session or adjustment |
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
  shape) and the longest readable text (HTML is converted when no source
  extracted text). `render: "browser"` prefers the `obscura` copy. The page's
  lead image is fetched through `ctx.public_http` with a 15 MB cap; its failure
  never fails the import.
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
log: steps done and ready, gathered ingredients, running timers, adjustments
and notes.

- Cooking never edits the recipe. `adjust` overrides a step's or ingredient's
  text for the session.
- `finish` writes the cook log (cook count, last cooked) and turns each
  override still in force into an open **adjustment** on the recipe, with the
  text the cook saw at the session's scale. `recipes.cook_finished` fires.
- An agent folds an adjustment in with `recipes_edit(..., resolves: [id])`, or
  dismisses it with a `dismissAdjustment` event (valid on a finished session).
  A `rename` operation re-points open adjustments.
- A session idle for 18 hours closes itself. It counts as a cook
  (`finished`) when at least half its steps were done, otherwise it is
  `abandoned` with no cook log and no adjustments.
- `rebase` moves a session to a newer revision; state carries over by id.
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
| `recipes_adjustment` | What cooks changed: open, resolved or dismissed |
| `recipes_change` | The change log: tool, summary, touched facets, before, after |
| `recipes_client_op` | Ids of the app's edits already applied |
| `recipes_redirect` | Where a purged, merged-away recipe lives |
| `recipes_timer_alert` | One row per ended timer: the alert reservation |
| `recipes_meta` | `schema_version`, `seq`, `cook_seq`, backup bookkeeping |
| `recipes_fts` | FTS5 index |

- Triggers abort any `UPDATE` of a revision and any `DELETE` unless the recipe
  is in the trash. Old formats are converted as they are read
  (`omni_recipe_format::upgrade`), never rewritten.
- `seq` is a library-wide counter bumped by every change. The app syncs with
  "everything after `seq` N". Cook events bump `cook_seq` instead, so crossing
  a step off does not make every device sync.
- Ids are a prefix (`rcp_`, `chg_`, `inb_`, `cook_`, `img_`, `adj_`) and eight
  lowercase base32 characters. The app chooses a session's `cook_` id.

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

## The app's HTTP API

`http.rs` builds one router that is registered only as
`Subsystem::host_apps`; it is never merged into Omni's router. It exists when
`RECIPES_APP_URL` is set.

| Route | Purpose |
|---|---|
| `GET /api/sync?since=<seq>` | The library after `seq` (or all of it), `removed` ids, tags, open sessions |
| `GET /api/events` | SSE: `hello`, then `library` and `session` frames, with pings |
| `POST /api/ops` | Notes and state edits, each applied once by id |
| `POST /api/cook`, `GET /api/cook/:id` | Start (idempotent by id) and read a session |
| `POST /api/cook/:id/events` | Append events |
| `POST /api/cook/:id/photo?id=` | The photo of the result, as an image of the recipe; stored once per upload `id` |
| `GET /api/inbox`, `POST /api/inbox?item=&index=&count=&title=` | List; upload one photo |
| `GET /api/original/:recipeId` | The stored source |
| `GET /img/:digest/:variant.jpg` | `thumb` or `full`, immutable; only digests a row refers to |
| `GET /src/:itemId/:n.jpg` | Photo `n` of an inbox item |
| `/`, `/r/:id`, `/cook/:id`, `/inbox` | `index.html` (`no-cache`) |
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
what the app can do: change a recipe's state and notes, run cook sessions and
upload to the inbox. Those writes are logged and reversible.

### Running it locally

```bash
(cd crates/omni-recipes-web && trunk build)          # the app; optional for the API
RECIPES_APP_URL=http://fond.localhost:8099 cargo run -p omni -- --preview --port 8099
```

Open `http://fond.localhost:8099/`. The preview uses a throwaway database and
recipes directory and seeds the four sample recipes and ten tags. The first
(Chicken Piccata) also has notes, a stored source text, and one finished cook
with a drawn placeholder photo and an open adjustment, and one photo waits in
the inbox, so every screen of the app has something to show
(`crates/omni/src/preview_recipes.rs`). `http://localhost:8099/` is still the
Omni preview. Of the background services the preview starts only
`recipes-timers`, so idle sessions close and ended timers are recorded
(`recipes_timer_alert`, state `unrouted`: nothing is sent in a preview).
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
  app-edit ids older than 30 days, inbox items discarded 30 days ago, uploads
  that never completed, and image files nothing refers to.
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
- A restore needs an empty library and an archive that verifies.
