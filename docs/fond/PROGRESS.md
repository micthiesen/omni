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

Nothing is deployed yet.

| Piece | State |
|---|---|
| `omni-recipe-format` (types, validator, renderers, ops, timeline) | in progress |
| Host dispatch, `RECIPES_APP_URL`, `omni-image`, `PageFetcher` port | in progress |
| Boris: NPM host, DNS record, compose env, outside probe workflow | in progress |
| `omni-recipes` library, tables, search, 21 tools | not started |
| Fond HTTP API (sync, ops, cook sessions, inbox upload, SSE) | not started |
| `omni-recipes-web` PWA | not started |
| Deploy, seed recipes, browser tests against production | not started |

## Decisions and deviations

(recorded as they are made)

## Next

Finish the format crate, then build `omni-recipes` against it.

## Blockers

None.
