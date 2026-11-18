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

Production runs build 8f521e6 (first deploy). The second push (bb9560e) never deployed: CI's
image job failed because the upstream sherpa-onnx parakeet archive was replaced and no longer
matches its pinned SHA-256. b496a41 makes the image copy the model production already runs from
the published image (digest-pinned, per-file SHA-256). The third push (this commit) carries
bb9560e's fixes, that Dockerfile change, and Michael's three feedback items, all built and
tested locally: gate-equivalent checks ok (3,786 tests), deny ok, e2e 28/28 on the mock and 21/21
on `omni --preview`.

Already verified on production (first deploy): host isolation and the outside probe (run
44309507641), seeding through the tools (seven recipes: Chicken Piccata `rcp_3ps0548z`,
Chocolate Chip Cookies `rcp_8ayqek0q`, Lasagne alla Bolognese `rcp_3e2bvg6e`, Chicken Enchiladas
`rcp_xr3r1m5h`, Simple Scones `rcp_r8kc2s9k`, Classic Homemade Chili `rcp_tw452req`, Dad's Mustard
Vinaigrette `rcp_vwz7357v`; throwaway ZZ Test Flatbread `rcp_nnk6nt18` in the trash), every agent
flow, and 13 browser tests. Michael has since used the app himself: anything in the library that
is not listed here is his; do not touch it.

## Next (after this deploy restarts the session)

1. Confirm: `gh run list --limit 2` green (the image job is the first real build of the parakeet
   change; if it fails, fix and push again), `/api/sync` on `https://recipes.syas.ca` reports the
   new build, `ssh boris docker logs --since 20m omni` clean, and livestream intelligence still
   loads its models (`grep -i "intelligence" in the boot log; no "disabled"`).
2. Spot-check with the tools: `recipes_get rcp_r8kc2s9k view text` shows 326 g and 113 g;
   `recipes_guide` reports version 3.
3. Abandon only the test cook sessions (`recipes_cook_get` lists active ones; the e2e sessions
   carry an adjustment text starting "Adjusted by e2e"; leave any session Michael started).
4. Full browser suite on production through the tunnel, on the throwaway recipe: restore it
   (`recipes_organize ids [rcp_nnk6nt18] state active`), `ssh -f -N -D 127.0.0.1:18080 boris`, then
   `cd crates/omni-recipes-web/e2e && FOND_PROXY=socks5://127.0.0.1:18080
   FOND_BASE_URL=https://recipes.syas.ca FOND_RECIPE_CHART="Lasagne alla Bolognese"
   FOND_RECIPE_COOK="ZZ Test Flatbread" FOND_RECIPE_SOURCE="Dad's Mustard Vinaigrette"
   FOND_RECIPE_AMOUNT="ZZ Test Flatbread" FOND_AMOUNT_INGREDIENT=<an ingredient of it with an
   amount> FOND_SHOTS=<dir> npx playwright test fond.spec.js` (read the switches at the top of
   fond.spec.js first). The chart test loops over every recipe at iPhone width in the recipe view
   and cook mode and writes a screenshot of each: look at the vinaigrette, piccata, cookies and
   lasagne ones. Afterwards: discard the uploaded inbox item, abandon any test session, trash the
   throwaway again, confirm the seeded recipes kept revision and cook count (the vinaigrette is at
   revision 2 by design).
5. Final report here and to Michael. Mention: the parakeet upstream change (new export not
   adopted), what only he can check on the phone, and the SOCKS tunnel for browser tests.

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
