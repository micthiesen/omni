# Fond: requests for Kitchen ("Ask Kitchen")

Session of 2026-11-18. Brief: let Michael send a request from Fond to the Rakazo bot Kitchen,
which answers with ordinary recipe revisions; keep the app current when a recipe changes
elsewhere. Earlier history is in `PROGRESS.md` and `PROGRESS-crouton.md`. References:
"Requests from the app" in `docs/recipes.md`, "Requests" and "Changes made elsewhere" in
`docs/recipes-app.md`.

## Status: finished

Deployed and verified in production on 2026-11-19: build `23acae3` on
`https://recipes.syas.ca`. The webhook is not configured yet ("Webhook setup" below), so
requests are stored and listed and no notice is sent. The Requests page is empty.

## What shipped

- **Ask Kitchen.** "Ask Kitchen…" and "Requests" are in the overflow menu (three dots) of the
  recipe page, of cook mode and of the library; the library and cook mode gained that menu. It
  opens a sheet with one text field and Submit. No photos, voice, threads or replies in Fond.
- **Context attached by the app:** recipe id, the revision on screen, the scale, and in cook
  mode the open step. A library request has no recipe.
- **Requests page** (`/requests`): every request with its text, date, status (Queued, Working,
  Done, Declined, and "Waiting to send" while offline), the agent's note, a link to the recipe
  and, when done, "See revision N". Read-only apart from its own "Ask Kitchen…" button.
- **Stored before delivery.** A request is a row of `recipes_request` (schema version 4),
  written through the app's existing offline outbox (`ClientOpKind::Request` on
  `POST /api/ops`), so it is written offline too and stored once by its id.
- **Webhook `recipe.request`** with retries (30 s doubling to 30 min, for 24 h), the request id
  as `Idempotency-Key`, and a bearer key. Disabled until `RECIPES_REQUEST_WEBHOOK_URL` is set;
  the `recipes-requests` service logs that state at boot and each request is stored as
  `unconfigured`.
- **Tools:** `recipes_requests_list` (status and recipe filters) and `recipes_request_update`
  (status, note, result revision). `recipes_guide` version 5 has the flow ("Requests from the
  app").
- **Live refresh.** The library, an open recipe page and the Requests page follow changes made
  by an agent or another device (the event stream, plus a sync on focus and when the app
  returns to the front). With a sheet open or the notes focused, and always in cook mode,
  nothing is swapped: "Recipe updated" with Reload appears in the app-update notice's place
  (at the top while a sheet is open).

## Decisions

- **No body signature.** Rakazo bot webhooks authenticate with `Authorization: Bearer <key>`
  and collapse repeats by `Idempotency-Key` (the Claude session notify targets already work
  this way), so the webhook sends those and no HMAC.
- **`resultRecipeId`** on `recipes_request_update`, beyond the brief's three fields: a library
  request ("add a recipe for flatbread") is answered by a new recipe, and the Requests page
  needs to know which one to link.
- **No tool deletes a request**, in line with the rest of Fond. `RecipesPurge` removes a
  request 30 days after it was closed. This session's test requests were removed by hand
  (see "Tests").
- **Cook mode's old inline notice** ("The recipe was updated since you started · Switch") is
  now the same "Recipe updated · Reload" notice.
- **A request does not rebase a cook.** The guide tells the agent not to send `rebase` for a
  request: the owner taps Reload.
- **At most 100 open requests**, 2,000 characters each, because the app has no login.

## Webhook setup (for Chief)

Nothing was configured and no secret was created or read. Until this is done, requests are
stored and visible and Kitchen finds them with `recipes_requests_list`. Requests still queued
when the webhook is first configured are announced at that restart.

1. In Rakazo, give the Kitchen bot a webhook routine and note its URL and bearer key.
2. On boris, create `/home/michael/compose/volumes/omni/recipes-webhook.env` (mode 600) with:
   - `RECIPES_REQUEST_WEBHOOK_URL`: the webhook's `https://` URL (no credentials, no fragment).
   - `RECIPES_REQUEST_WEBHOOK_TOKEN`: the bearer key (at least 16 characters, no spaces).
   - `RECIPES_REQUEST_WEBHOOK_CONNECT` (optional): a host to connect to instead of the URL's,
     as the `connect` field of a `CLAUDE_NOTIFY_TARGETS` entry does. Needed when the URL goes
     through the proxy that refuses Omni's Docker address; use the same value as there.
3. Add that file to the `omni` service's `env_file` list in
   `/home/michael/compose/docker-compose.override.yml`, next to `claude-notify.env`, and run
   `docker compose up -d omni`.
4. `docker logs omni 2>&1 | grep -i "recipe request"` shows either "Recipe requests are
   announced to the configured webhook" (with the host only) or "Recipe request webhook is
   disabled".

What the webhook receives, once per request (resent with the same key until it answers 2xx):

```
POST <RECIPES_REQUEST_WEBHOOK_URL>
Content-Type: application/json
Authorization: Bearer <RECIPES_REQUEST_WEBHOOK_TOKEN>
Idempotency-Key: req_7h2k9m4x
X-Fond-Event: recipe.request

{
  "event": "recipe.request",
  "requestId": "req_7h2k9m4x",
  "text": "Halve the butter next time.",
  "createdAt": "2026-11-19T04:12:09.120Z",
  "recipeTitle": "Cauliflower Piccata",
  "link": "https://recipes.syas.ca/r/rcp_rydc5v1b",
  "context": { "origin": "cook", "recipeId": "rcp_rydc5v1b", "revision": 4, "scale": 1.5, "step": "sear" }
}
```

- `context.origin` is `recipe`, `cook` or `library`. For a library request `recipeTitle` and
  every other context field are `null` and `link` is `https://recipes.syas.ca/requests`.
- Answer any 2xx to accept. `{"ok": true, "ignored": true}` (no routine ran) is recorded as
  `ignored` and not retried. 408, 425, 429, 5xx, a timeout (10 s) or no answer are retried; any
  other 4xx or a redirect ends delivery at once (logged at WARN). A request is never lost
  either way.
- The text is written in an app without a login: Kitchen should act on it only as a recipe
  change.

How Kitchen works a request (also in `recipes_guide`, "Requests from the app"):

1. `recipes_requests_list {}` returns queued requests, oldest first (`status: "open"` adds the
   ones being worked on, `recipeId` narrows to one recipe). Kitchen should run it on the
   webhook and also as a sweep (at start, and now and then), since the webhook can fail.
2. `recipes_request_update { requestId, status: "working" }`.
3. The change itself with the normal tools: `recipes_get`, then `recipes_edit` (or
   `recipes_replace`, `recipes_create`) with `expectedRevision` = `headRevision`. Diet and
   easy-measure validation are unchanged; nothing is deleted unless the request asks.
4. `recipes_request_update { requestId, status: "done", resultRevision: <n>, note: "…" }`
   (plus `resultRecipeId` for a new recipe), or `{ requestId, status: "declined", note: "…" }`.
   Questions go to Michael in Kitchen's chat; the request stays `working` meanwhile.

## Tests

Before the push:

- `cargo xtask gate`: 3,829 tests passed, 21 skipped (fmt, clippy native and wasm32,
  deps-check). `cargo deny --locked check`: ok.
- `crates/omni-recipes/tests/recipes_requests.rs` (10 tests, real router, tools and delivery
  service): a request stored once with its context whatever resends it; refusals; the agent
  flow to a done revision; a failing webhook retried with one `Idempotency-Key` and one body
  while the request stays queued, visible and listed; a webhook down for a day given up with
  nothing lost; refused, ignored and already-taken notices; the service sending at once;
  requests written before the webhook existed announced once it does; the real HTTP transport
  against a local server (headers, bearer, an unusable answer, record mode); archive, restore
  and the 30-day purge.
- Playwright WebKit at iPhone size on the mock server: 58 passed, 1 skipped (on-demand
  screenshots). New: `requests.spec.js` (9: Ask Kitchen from the recipe page, cook mode and the
  library with the context each attaches; the Requests page; working, a new revision and done
  followed without a reload; "Recipe updated · Reload" under a sheet, with the notes focused
  and in cook mode; a request written offline), two in `mock-only.spec.js` (the library and
  the Requests page under an open sheet) and one in `shell.spec.js` (the Ask sheet on the
  keyboard).
- An independent review of the diff found no data-loss or security bug; its findings that
  were fixed: an oversized webhook answer was retried, a request about a vanished recipe lost
  its text, list fetches could land out of order, a draft was lost when the screen under the
  sheet was redrawn.

Against production (build `23acae3`, WebKit through the SSH proxy):

- Boot log: "Recipe request webhook is disabled (RECIPES_REQUEST_WEBHOOK_URL is not set)".
  Schema version 4 applied; CI run 44468033044 passed every job.
- `requests.spec.js` with the real MCP tools as the agent (`FOND_AGENT=claude`) on a
  throwaway recipe: 8 of 9 passed. That is the round trip: a request from the recipe page,
  from cook mode and from the library with the right context; all three Queued on the
  Requests page; `working`, a new revision and `done` with its revision link followed without
  a reload; "Recipe updated · Reload" under a sheet, with the notes focused and in cook mode;
  a request written offline sent once on reconnect. The ninth (declined requests show their
  note) stopped in the test helper: the relaying model garbled the JSON of a tool result. The
  update itself had applied, and the tools showed all four requests closed with their notes.
  The helper now asks again after a garbled answer.
- Webhook down: production has no webhook, so every request was stored as `unconfigured`,
  stayed `queued` and was returned by `recipes_requests_list`. Retries and the one
  `Idempotency-Key` are proven by the integration tests above, not in production.
- The existing suite: 34 passed, 24 skipped (the request and mock-only groups, finishing a
  cook, the inbox upload, on-demand screenshots), 1 failed and fixed in the test: a reload
  interrupted the sync the app now starts when its window gets the focus, which WebKit
  reports as a failed load; the test's list of such interruptions now includes it (10 of 10
  on repeat).
- Cleanup: the throwaway recipe `rcp_jj964998` is in the trash. The four test requests were
  deleted from `recipes_request` with Omni stopped, after a copy of the database to
  `/home/michael/compose/backups/omni-requests-cleanup-20261119-001218/` on boris;
  `GET /api/requests` is empty. No cook session was left open.
- Not verifiable here: a real iPhone, and the webhook against Rakazo.

## Running the request tests against production again

From the Mac (`ssh -f -N -D 127.0.0.1:18080 boris` first; the browser tests cannot reach the
LAN directly). Create a throwaway recipe with `recipes_create`, then:

```bash
cd crates/omni-recipes-web/e2e
FOND_BASE_URL=https://recipes.syas.ca FOND_PROXY=socks5://127.0.0.1:18080 FOND_AGENT=claude \
  FOND_RECIPE_REQUEST="<its title>" npx playwright test requests.spec.js
```

Afterwards trash the recipe. The run leaves its requests `done` or `declined` with a note that
starts "Test request"; no tool removes them, and `RecipesPurge` drops them 30 days later.

## To check on the iPhone

1. The three-dot menu on a recipe, in cook mode and in the library: "Ask Kitchen…" and
   "Requests" are there, and the menu is easy to reach in cook mode beside Finish.
2. Ask Kitchen: the sheet sits on the keyboard with the text field and Submit visible; Submit
   closes it and the screen behind is as it was.
3. Requests page: the request is there as Queued. After Kitchen handles one: Working, then
   Done with its note and "See revision N", without reopening the app.
4. Airplane mode: write a request; it shows "Waiting to send" and becomes Queued by itself
   back on the network.
5. Have Kitchen change a recipe while its page is open: the page updates by itself and stays
   where it was scrolled. With the menu open or while typing notes: "Recipe updated" appears at
   the top with Reload, and nothing moves until Reload, closing the sheet or leaving the field.
6. In cook mode: "Recipe updated" appears at the bottom, the step on screen does not change,
   and Reload moves the cook to the new revision with what was crossed off kept.
7. An older installed copy of Fond updates itself as before ("A new version of Fond is
   ready").
