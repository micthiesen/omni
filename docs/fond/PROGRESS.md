# Fond build progress

Fond is the agent-first recipe app served on `https://recipes.syas.ca` (LAN and WireGuard only).
Spec: `~/Code/oracle/notes/2026-11-17-recipes-proposal.md` (oracle commit e3d18cd), prototype beside it.
Durable references: `docs/recipes.md` (server), `docs/recipes-app.md` (PWA).
Later sessions: `PROGRESS-crouton.md` (diet, measuring, the Crouton import, the shell) and
`PROGRESS-requests.md` (requests for Kitchen, live refresh).

Michael's changes to the spec:

1. No Crouton import (no `.crumb`, no zip upload). The photo inbox stays.
2. Every `recipes_*` tool is exposed to every client like other Omni tools.
3. Recipe pages are fetched with the PressPods retrievers, through the `PageFetcher` port.
4. Server-side AI is allowed where it helps quality.

A push to `main` deploys and restarts Omni, which ends the building session. Resume by reading this
file.

## Status: finished

Fond is built, deployed and tested end to end. Production runs build 9d26992 at
`https://recipes.syas.ca`. Nothing is in progress. A later docs-only push (this file) redeploys the
same code.

## Final report (2026-11-18)

**What shipped**

- `omni-recipe-format`: the recipe document model, validator, list and chart renderers, scaling,
  edit operations, timeline, search tokeniser (native and wasm).
- `omni-recipes`: revisions, tags, change log and undo, 30-day trash, images, inbox (pages, text,
  photos), cook sessions and timers, 21 `recipes_*` tools, the app's HTTP API as a host app,
  `RecipesBackup` and `RecipesPurge`, `omni recipes-verify` / `recipes-restore`, two MCP events.
- `omni-recipes-web`: the Fond PWA (library, recipe with Steps and Chart, cook mode, inbox,
  offline with outbox, service worker).
- Foundations: host apps in `omni-server-kit`, `omni-image`, the `PageFetcher` port over the
  PressPods retrievers.
- Boris: NPM host 51 behind access list 3 with the real-IP override, DNS-only A record,
  `RECIPES_APP_URL` in compose, `deploy/recipes/route.mjs`, `verify-recipes-boundary.yml`.
- Michael's feedback after first use: horizontal full-width charts, cancel a cook from the X,
  change an ingredient's amount from the app (a real revision, tool `app`).

**Tests run against production**

- Playwright WebKit, iPhone 14 viewport, build 9d26992: 22 of 22 passed (shell, manifest, icons,
  service worker, first visit, CSP, library with images, scroll restore, search, tag filter, steps
  and merged-cell chart, wide chart and landscape, every recipe's chart in the recipe view and in
  cook mode, scaling, notes, original, archive and delete with undo, cook mode cross-off in all
  three tabs with adjust, timer, reload and finish, discard a cook, amount change in the recipe
  view, on a scaled page, from cook mode and offline, inbox upload, offline load/read/cook/sync).
- Agent flows through the MCP tools (first deploy): create from URL, from pasted text, from the
  four spec documents (text and chart equal to the prototype's goldens), edit with ops, dry runs,
  stale-revision conflict, replace, revert, history diff, undo and undo conflict, organize, tags,
  trash and restore, merge, search, scaled get, cook start/update/finish, `resolves`, inbox.
- Outside probe (run 44309507641): every request for the recipes host from a GitHub runner was
  refused 403, plain, with forged LAN headers and on the gateway's connection; Fond's paths on
  `gateway.syas.ca` are 404.
- Local: `cargo xtask gate` (3,786 tests), `cargo deny`, e2e 29/29 on the mock and the full suite
  on `omni --preview`.

**Library left in production**: Chicken Piccata `rcp_3ps0548z`, Chocolate Chip Cookies
`rcp_8ayqek0q`, Lasagne alla Bolognese `rcp_3e2bvg6e`, Chicken Enchiladas `rcp_xr3r1m5h`, Simple
Scones `rcp_r8kc2s9k`, Classic Homemade Chili `rcp_tw452req`, Dad's Mustard Vinaigrette
`rcp_vwz7357v`. Trash (purged after 30 days): ZZ Test Flatbread `rcp_nnk6nt18` and its merged
duplicate. No open cook sessions, empty inbox.

**Known leftovers and things to know**

- Chicken Piccata shows one cook on 2026-11-18 whose log carries "Adjusted by e2e ...": a test
  session that was finished from the app before discard existed. A finished cook cannot be removed
  through the tools.
- The upstream sherpa-onnx parakeet archive was replaced with a re-exported model. The image now
  copies the model production already runs from `ghcr.io/micthiesen/omni@sha256:f8e2e957...`
  (per-file SHA-256). Adopting the new export is a separate decision that needs a transcription
  test.
- Browser tests from the Mac must go through `ssh -D` (`FOND_PROXY`): Playwright's processes cannot
  reach the LAN directly.
- Not verifiable without the phone: Home Screen install and icon, Screen Wake Lock, camera
  capture in the Inbox, timer sound and the phone alert when a timer ends with the app closed,
  feel of the charts and sheets on the real device.
- Not built: Crouton import (by decision). `lettre` was bumped to 0.11.24 and a `deny.toml` ignore
  added for the `chrono` unmaintained advisory so CI passes.

## Michael's feedback after using it on the iPhone (2026-11-18 08:49 PT), decided, to build

1. **Chart is squashed.** Cook mode > Chart for "Dad's Mustard Vinaigrette" at ~390pt: the
   ingredient column takes ~40%, step columns are thin slivers with rotated text, the right half is
   empty. Make the chart use the full width and read horizontally: step cells wide enough for
   normal horizontal wrapped text, sized to content, horizontal scroll only when a big recipe
   genuinely does not fit. No rotated text. Check every seeded recipe (small and big, e.g. the
   lasagne) at iPhone width in the recipe view and in cook mode, with screenshots in the browser
   tests.
2. **Cancel a cook.** An obvious way to cancel an in-progress cook without finishing or logging
   it: next to Finish, or in the close X flow (keep for later / discard), one confirmation, no cook
   log entry. The existing discard (in the Finish sheet) is not discoverable.
3. **Adjust amounts.** Capture "half the salt next time": the user can edit an ingredient's amount
   (quantity and unit only; no adding or removing ingredients, no step edits) from the recipe view
   and from cook mode. Saving writes a new recipe revision through the same validated path the
   tools use (expectedRevision, revision history, undo): permanent, visible to agents, reversible.
   Step placeholders follow. Notes stay the free-text channel.

Built (not yet deployed at the time of this commit): the chart lays out horizontally at full
width with measured column widths and scrolls only when the columns cannot fit; the cook-mode X
asks Keep for later / Discard this cook; ingredient amounts can be changed from the recipe view
(pencil) and from cook mode (inside Adjust), through `POST /api/ops` kind `amount`, written as a
revision with tool `app`.

Then: redeploy, run the full production browser suite including these, clean up test data, final
report.

## Decisions and deviations

- Tools are named `recipes_*` (single underscore), as the spec corrects.
- `omni-recipes` cannot depend on `omni-presspods`, so URL import goes through a new port,
  `omni_runtime::ports::PageFetcher`, implemented by `omni_presspods::page_fetch` (all retrievers,
  unrated, with raw HTML from the fetch, Wayback and Obscura sources; Jina too when its key is set).
  JSON-LD recipe parsing is in `omni-recipes`.
- One server-side model call: an extraction review (`fidelity.review`) comparing the stored source
  (text or photos) with the rendered recipe on create and replace. It never blocks a save.
- Inbox kinds are `photo`, `url`, `text`. `source.kind: "crouton"` stays in the format for the
  manual import Michael will run later.
- NPM: SSE settings are host-level instead of a `location /api/events` block, because a custom
  location would bypass the access list. Upload cap is 64 MB at the proxy, 25 MB per photo in Omni.
- Format crate vs the prototype: stricter edit ops (anchors must be siblings), `rescale` skips uses
  of `scales: false` ingredients, extra validator codes for the spec's limits and for non-http
  source URLs, `fmt_time_prose` fixes "1 hours".
- Server: two extra tables (`recipes_redirect`, `recipes_timer_alert`); undo also covers creates
  and edits; a finished cook cannot be undone (`not_undoable`); a trashed recipe answers `trashed`;
  write results carry `changeId`; `recipes_inbox_get` pages photos four at a time;
  `recipes.inbox_added` fires only for app photo captures.
- Timer alerts: with an app client connected the phone alert waits 30 s and is skipped when the
  app answers the timer; alerts are capped at 10 per hour.
- Limits on the unauthenticated app surface: 20 active cook sessions, 30 timers per session, 200
  pending inbox items, bounded upload and SSE concurrency.
- Fail-closed gate: a request for the Fond host that carries Cloudflare headers is refused 403,
  so a lost private DNS record or access list does not expose the app through Cloudflare.
- `RecipesPurge` writes a verified archive under `archives/purge/` (kept 180 days) before it
  hard-deletes anything.
- App: the Archived switch adds archived recipes (badged) to the grid; bold in step text covers
  the whole ingredient reference; the "meanwhile" strip reads "While «label» runs:" with step
  chips; "Dismiss" on a "Last time" callout is an addition.
- Cook sessions can be discarded (`abandon` event, "Discard this cook" in the app): the spec had
  only finish and the 18-hour idle close.
- Unscaled amounts render exactly as stored; the prototype's metric rounding applies only to
  scaled amounts.
- The app can change an ingredient's amount (quantity and unit only), which the spec's
  minimal-editing rule did not allow; Michael asked for it. It writes a normal revision.
- No rotated text in the chart at any width (the spec had vertical text for tall cells).
- Unrelated upkeep needed for CI: `lettre` 0.11.23 to 0.11.24 (six new SMTP advisories) and a
  documented `deny.toml` ignore for the `chrono` unmaintained advisory.

## Blockers

None.
