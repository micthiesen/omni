# Fond build progress

Fond is the agent-first recipe app served on `https://recipes.syas.ca` (LAN and WireGuard only).
Spec: `~/Code/oracle/notes/2026-11-17-recipes-proposal.md` (oracle commit e3d18cd), prototype beside it.

Michael's changes to the spec:

1. No Crouton import (no `.crumb`, no zip upload). The photo inbox stays.
2. Every `recipes_*` tool is exposed to every client like other Omni tools.
3. Recipe pages are fetched with the PressPods retrievers, through the `PageFetcher` port.
4. Server-side AI is allowed where it helps quality.

A push to `main` deploys and restarts Omni, which ends the building session. Resume by reading this
file.

## Status

Nothing is pushed or deployed yet. Local commit 0fed18e holds the foundations.

| Piece | State |
|---|---|
| `omni-recipe-format` (types, validator, renderers, ops, timeline, search tokeniser) | done, 84 tests, goldens byte-identical to the prototype |
| Host dispatch (`omni_server_kit::host_app`), `RECIPES_APP_URL`, `AppPaths::recipes_dir` | done |
| `omni-image` foundation crate (extracted from `omni-imap`) | done |
| `PageFetcher` port over the PressPods retrievers | done |
| App and server HTTP contract (`crates/omni-api/src/fond.rs`) | written |
| Boris: NPM host 51 `recipes.syas.ca` (access list 3, real-IP override), DNS-only A record, `RECIPES_APP_URL` in compose (applies at the next deploy), `deploy/recipes/route.mjs`, `verify-recipes-boundary.yml` | done; the outside workflow has not run (needs a push) |
| `omni-recipes`: tables, search, 21 tools, cook sessions, HTTP API, tasks, CLI | in progress (subagent) |
| `omni-recipes-web` PWA, Dockerfile and CI for the second bundle | in progress (subagent) |
| Integration of server and app, review, gate | not started |
| Deploy, seed the four spec recipes through the tools, browser tests against production | not started |

## Decisions and deviations

- Tools are named `recipes_*` (single underscore), as the spec corrects.
- `omni-recipes` cannot depend on `omni-presspods`, so URL import goes through a new port,
  `omni_runtime::ports::PageFetcher`, implemented by `omni_presspods::page_fetch` (all retrievers,
  unrated, with raw HTML from the fetch, Wayback and Obscura sources). JSON-LD recipe parsing is in
  `omni-recipes`.
- One server-side model call: an extraction review that compares the stored source (text or
  photos) with the rendered recipe on create and replace. It never blocks a save.
- NPM: the SSE settings are host-level instead of a `location /api/events` block, because a custom
  location would bypass the access list. Upload cap is 64 MB (no Crouton zip).
- Format crate: see its report in the commit; notable changes from the prototype are stricter edit
  ops (anchors must be siblings), `rescale` skipping uses of `scales: false` ingredients, extra
  validator codes for the spec's limits, and `fmt_time_prose` fixing "1 hours".
- Inbox kinds are `photo`, `url`, `text`. `source.kind: "crouton"` stays in the format for the
  manual import Michael will run later.

## Next

1. Wait for the server and app subagents, then run both together locally
   (`omni --preview` with `RECIPES_APP_URL=http://fond.localhost:<port>`), run the e2e suite against
   the real server, review ($check), run `cargo xtask gate`, `cargo deny --locked check` and both
   trunk builds.
2. Update this file, commit, push (deploys). After the restart: confirm the build sha at
   `/api/snapshot`, check `https://recipes.syas.ca`, seed the four recipes through the MCP tools from
   a fresh client, run the Playwright WebKit suite against production, run
   `verify-recipes-boundary.yml`.
3. Final report here and to Michael.

## Blockers

None.
