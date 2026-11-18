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

Everything is built, reviewed and passing locally. The first push (this commit) deploys it.
Production has not been verified yet.

| Piece | State |
|---|---|
| `omni-recipe-format` | done; goldens byte-identical to the prototype, property tests |
| Host dispatch, `RECIPES_APP_URL`, `omni-image`, `PageFetcher` port | done |
| `omni-recipes`: 14 tables + FTS5, 21 tools, cook sessions, HTTP API, events, tasks, CLI | done |
| `omni-recipes-web` PWA, second trunk bundle in the Dockerfile and CI | done; the Docker web stage is first built by CI on this push (no Docker on the Mac) |
| Review (data integrity, security, spec conformance) and fixes | done |
| Local verification | `cargo xtask gate` ok (3,768 tests), `cargo deny --locked check` ok, Playwright WebKit iPhone suite 15/15 against `omni --preview` (real server), 22/22 against the mock |
| Boris: NPM host 51 `recipes.syas.ca` (access list 3, real-IP override), DNS-only A record, `RECIPES_APP_URL` in compose (applies at this deploy) | done |
| Production: deploy check, seed recipes, browser tests, outside boundary probe | NEXT |

## Next (after the deploy restarts the session)

1. Confirm the deploy: `gh run list --limit 3` (CI green; if the Docker web stage failed, fix and push
   again), build sha at `http://omni.boris/api/snapshot`, `ssh boris docker logs --tail 100 omni`
   (no recipes errors; `/data/recipes` created), `tasks_list` shows `RecipesBackup` and
   `RecipesPurge`.
2. Check the host: `https://recipes.syas.ca/` serves Fond (not the Omni UI), `/api/sync` answers,
   `/gateway/mcp` there is 404; `https://omni.syas.ca/api/sync` is 404; gateway host paths 404.
3. Seed through the MCP tools from a fresh client (`claude -p`, since this session's tool list is
   stale after a deploy): read `recipes_guide`, create the tags, then the four spec recipes
   (`crates/omni-recipe-format/tests/golden/*.json`: chicken piccata, chocolate chip cookies,
   lasagne alla bolognese, chicken enchiladas) with `recipes_create`, add a cover image to at least
   two with `recipes_images_edit`, and import one real recipe from a URL with `recipes_import_url`
   + `recipes_create({fromInbox})`. Also exercise against production: an edit with ops, a dry run, a
   stale-revision conflict, undo, trash and restore (on a throwaway recipe, left trashed or
   restored and then trashed again). Leave the seeded recipes in place.
4. Browser tests against production (WebKit, iPhone viewport):
   `cd crates/omni-recipes-web/e2e && npm ci && FOND_BASE_URL=https://recipes.syas.ca
   FOND_E2E_SKIP_FINISH=1 FOND_E2E_SKIP_INBOX=1 FOND_RECIPE_CHART="Lasagne alla Bolognese"
   FOND_RECIPE_COOK="Chicken Piccata" npx playwright test fond.spec.js`. Decide per test whether
   finish and inbox can run against production with cleanup (a finished cook stays in history; an
   uploaded photo can be discarded with `recipes_inbox_resolve`).
5. Outside probe: `gh workflow run verify-recipes-boundary.yml -f origin_ipv4=<Boris public IPv4>`
   and confirm every probe is refused or 404.
6. Write the final report here and reply to Michael (what shipped, URL, Home Screen steps, tests
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
- Unrelated upkeep needed for CI: `lettre` 0.11.23 to 0.11.24 (six new SMTP advisories) and a
  documented `deny.toml` ignore for the `chrono` unmaintained advisory.

## Blockers

None.
