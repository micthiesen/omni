# Fond build progress

Fond is the agent-first recipe app served on `https://recipes.syas.ca` (LAN and WireGuard only).
Spec: `~/Code/oracle/notes/2026-11-17-recipes-proposal.md` (oracle commit e3d18cd), prototype beside it.
Durable references: `docs/recipes.md` (server), `docs/recipes-app.md` (PWA).

Michael's changes to the spec:

1. No Crouton import (no `.crumb`, no zip upload). The photo inbox stays.
2. Every `recipes_*` tool is exposed to every client like other Omni tools.
3. Recipe pages are fetched with the PressPods retrievers, through the `PageFetcher` port.
4. Server-side AI is allowed where it helps quality.

A push to `main` deploys and restarts Omni, which ends the building session. Resume by reading this
file.

## Status

Deployed and verified in production at build 8f521e6 (first deploy). A second push (this commit)
carries the fixes from the production acceptance run.

Done against production (first deploy):

- `https://recipes.syas.ca` serves Fond; other Omni paths there are 404; Fond's paths are 404 on
  `omni.syas.ca` and `gateway.syas.ca`; a foreign-Origin POST is 403.
- Outside probe `verify-recipes-boundary.yml` run 44309507641: every request for the recipes host
  from a GitHub runner was refused 403 (plain, forged LAN headers, and on the gateway's
  connection); Fond's paths on the gateway host are 404.
- Seeded through the MCP tools by a fresh agent: Chicken Piccata `rcp_3ps0548z`, Chocolate Chip
  Cookies `rcp_8ayqek0q`, Lasagne alla Bolognese `rcp_3e2bvg6e`, Chicken Enchiladas `rcp_xr3r1m5h`
  (the four spec recipes, text and chart equal to the goldens), Simple Scones `rcp_r8kc2s9k` and
  Classic Homemade Chili `rcp_tw452req` (URL imports), Dad's Mustard Vinaigrette `rcp_vwz7357v`
  (pasted text). Six have cover photos. 22 tags. Throwaways in the trash: ZZ Test Flatbread
  `rcp_nnk6nt18` (and its merged duplicate `rcp_47v2svf8`).
- Agent flows on production: dry-run create with all problems at once, edit with ops, dry-run
  edit, stale revision conflict, replace, revert, history diff, undo and undo conflict, organize,
  unknown-tag refusal, archive, trash, restore, search filters and sorts, scaled get, merge with
  redirect and `[merged]`, cook start/update/finish, `resolves`, inbox resolve.
- Playwright WebKit, iPhone viewport, against production: 13 passed (shell/manifest/service
  worker, CSP, library with images, scroll restore, search, tag filter, steps and merged-cell
  chart, wide chart and landscape, scaling, notes, original, archive/delete with undo, offline).
  Cook mode ran through cross-off, adjust and timer but stopped before Finish; inbox upload not
  run. Playwright's processes cannot reach the LAN from the Mac, so the run goes through
  `ssh -f -N -D 127.0.0.1:18080 boris` with `FOND_PROXY=socks5://127.0.0.1:18080`.

Second push contents: exact amounts at scale 1 (326 g no longer shows as 325 g), bot-challenge
pages refused on import, `makeAhead` shown for single-component recipes, dry runs report unknown
tags and duplicate sources, Wayback URLs unwrapped and `imageReason`, better import text and tag
suggestions, richer undo/edit/history results, a discard action for cook sessions (`abandon`),
chart label and landscape fixes, guide version 2. Local: gate ok (3,779 tests), deny ok, e2e 16/16
against `omni --preview`, 23/23 against the mock.

## Next (after the second deploy restarts the session)

1. Confirm: `gh run list --limit 2` green, `/api/sync` on `https://recipes.syas.ca` reports the new
   build, logs clean.
2. Check the fixes on production with the tools: `recipes_get rcp_r8kc2s9k view text` shows 326 g
   and 113 g; `recipes_guide` reports version 2.
3. Discard the test cook session left open on Chicken Piccata (`recipes_cook_get` with no
   arguments lists active sessions; `recipes_cook_update` with `{type: "abandon"}` on each).
4. Full browser suite on production, including cook finish, discard and inbox upload, on the
   throwaway: restore it (`recipes_organize ids [rcp_nnk6nt18] state active`), open the tunnel, then
   `cd crates/omni-recipes-web/e2e && FOND_PROXY=socks5://127.0.0.1:18080
   FOND_BASE_URL=https://recipes.syas.ca FOND_RECIPE_CHART="Lasagne alla Bolognese"
   FOND_RECIPE_COOK="ZZ Test Flatbread" FOND_RECIPE_SOURCE="Dad's Mustard Vinaigrette"
   FOND_SHOTS=<dir> npx playwright test fond.spec.js`. Afterwards: discard the uploaded inbox
   item (`recipes_inbox_list`, `recipes_inbox_resolve`), abandon any open session, trash the
   throwaway again, and confirm the seven seeded recipes are active with cook count 0.
5. Write the final report here and reply to Michael (what shipped, URL, Home Screen steps, tests
   run against production, deviations, what only he can check on the phone).

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
- Unrelated upkeep needed for CI: `lettre` 0.11.23 to 0.11.24 (six new SMTP advisories) and a
  documented `deny.toml` ignore for the `chrono` unmaintained advisory.

## Blockers

None.
