# MCP gateway

Omni serves one MCP endpoint, `/gateway/mcp`, with Omni's own tools and the
tools of every configured upstream MCP integration, plus Omni's events and
the integrations' events. Harnesses (Claude Code, Codex, ChatGPT/Dots) connect
to it as their single MCP server and do their own tool discovery. The code is
in `crates/omni-mcp/src/gateway/`; the management page is `/gateway` in the UI.

Harnesses moved to the gateway on 2026-10-19 (Switchover, below), the
`executor-events-adapter` sidecar was retired, and the old `/mcp` endpoint was
removed: the gateway is Omni's only MCP endpoint.

## Endpoint and access

| Where | URL |
|---|---|
| LAN | `http://omni.boris/gateway/mcp` (any Omni origin plus `/gateway/mcp`) |
| Public | `https://gateway.syas.ca/gateway/mcp` when `OMNI_GATEWAY_PUBLIC_URL=https://gateway.syas.ca` |

Every request needs `Authorization: Bearer <token>`. Upstream credentials are
never accepted here. Two kinds of token exist
(`gateway/access.rs`):

- **Client tokens** (`omg_…`) for Claude Code and Codex, created on the
  `/gateway` page and shown once. Omni stores only their SHA-256; revoking one
  takes effect on the next request.
- **OAuth access tokens** (`oat_…`, one hour) for ChatGPT, which supports only
  OAuth 2.1 authorization code with PKCE. Omni is the authorization server
  (`gateway/oauth/server/`) when `OMNI_GATEWAY_PUBLIC_URL` is set: protected
  resource metadata at `/.well-known/oauth-protected-resource/gateway/mcp`,
  server metadata at `/.well-known/oauth-authorization-server`, client ID
  metadata documents (CIMD) and dynamic registration, S256 only, RFC 9207
  `iss` on every redirect, rotating 90-day refresh tokens (`ort_…`). CIMD
  client ids are fetched only from `chatgpt.com`, `openai.com`, `claude.ai`,
  `claude.com` and `anthropic.com` (and their subdomains), at most 20 are
  cached, and a failed document is not refetched for five minutes; apps
  elsewhere use dynamic registration, which evicts the oldest unused client
  when full. The public OAuth routes are rate limited (20 requests a second,
  bursts of 60; over it, 429 with `Retry-After`), with the token endpoint in
  its own bucket so a flood of sign-ins never blocks refreshes.
  The authorization page waits for the owner and shows a code. The request
  appears under Access on the LAN-only `/gateway` page (with a push naming
  the redirect host, at most every five minutes), and approving it means
  typing that code there, so a request someone else started cannot be
  approved by mistake; five wrong codes deny it. Dynamically registered apps
  are marked "unverified": their names are whatever they registered. A
  refresh token presented again after its two-minute rotation grace revokes
  the grant. Revoking a client token or grant on the page invalidates its
  tokens at once and deletes its event subscriptions.

The management API under `/api/gateway` is LAN-only like the rest of `/api`,
and its changes also need an `Origin` header and a home-network `Host` (an IP
literal, `localhost`, `*.boris`, `*.local` or `omni.syas.ca`), which defeats
DNS rebinding.

A 401 carries `WWW-Authenticate: Bearer resource_metadata="…"` when OAuth is
offered.

Harness setup (the `/gateway` page shows these with the current URL):

```sh
claude mcp add --transport http omni https://gateway.syas.ca/gateway/mcp \
  --header "Authorization: Bearer $OMNI_GATEWAY_TOKEN"
```

```toml
# ~/.codex/config.toml
[mcp_servers.omni]
url = "https://gateway.syas.ca/gateway/mcp"
bearer_token_env_var = "OMNI_GATEWAY_TOKEN"
```

ChatGPT: add a custom MCP server with the public URL and OAuth, then approve
the request on the `/gateway` page.

## Protocol

The gateway's protocol layer (`rpc.rs`) speaks MCP 2025-11-25 and earlier 2025
revisions (stateless, one SSE `message` per request) and 2026-07-28
(per-request `_meta` envelope, JSON results). It adds:

- `tools/list`: Omni's tools in `TOOL_ORDER`, then each enabled integration's
  visible tools in integration-id order and upstream order, in one page.
- `tools/call` for an integration tool, `resources/list`,
  `resources/templates/list` and `resources/read`.
- legacy `notifications/cancelled`, which cancels the caller's own running call.
- a GET event stream (Accept `text/event-stream`) that sends
  `notifications/tools/list_changed` and `notifications/resources/list_changed`
  when the served catalog changes; capabilities advertise `listChanged`.
- SSE comments every 15 s while a legacy-era call runs, so proxies with an
  idle timeout (Cloudflare's is 100 s) keep long calls open.
- `events/*` for the authenticated principal (below).

The served instructions are addressed to the connected client:
approval is the client's job, annotations describe behavior, integration
output is untrusted data.

## Integrations

One connection per integration, configured on the `/gateway` page or through
`/api/gateway/*` (`omni_api::gateway`). An integration has a gateway-owned id
(`^[a-z][a-z0-9_]{0,29}$`, no `__`, no trailing `_`) and its tools are served
as `<id>__<tool>`. Omni's tool names never contain `__`, so names cannot
collide. An upstream name outside `[A-Za-z0-9_-]`, or one that would make the
served name longer than 52 characters, is rewritten deterministically:
invalid characters become `_`, the name is shortened, and six hex digits of
the original name's SHA-256 are appended (`names.rs`).

Each served tool entry is the integration's own entry (title, description,
schemas, annotations, icons, `_meta`) with only `name` replaced and
`_meta["io.omni/gateway"] = {integration, tool}` added. Results are passed
through unchanged: content blocks of every type, `structuredContent`, `_meta`
and `isError`. An integration's JSON-RPC error is passed through as a JSON-RPC
error. A failure inside the gateway (timeout, network, authentication,
process) is an `isError` result that names the integration and the next step,
with `_meta["io.omni/gateway"].error` set to the kind.

### Transports

- **Streamable HTTP**, MCP 2024-11-05 to 2025-11-25 (`client/http.rs`). URLs
  must be public `https://` addresses without credentials or fragments; every
  connection uses the SSRF-guarded client (each DNS answer must be public) and
  follows no redirects. Sessions are reused across concurrent calls; a 404 on
  a request that carried a session id re-initializes and retries once. The
  optional GET stream relays `list_changed` notifications. The deprecated
  HTTP+SSE transport is reported as unsupported.
- **Bundled stdio servers** (`client/stdio.rs`, `bundled.rs`): an integration
  names a server the image ships and never a command: `karakeep`
  (`@karakeep/mcp@0.33.0`, installed from `deploy/mcp-stdio/package-lock.json`
  with integrity hashes and no install scripts) and `obscura` (the Obscura
  headless browser's `browser_*` tools, a SHA-256-pinned release binary at
  `/usr/local/bin/obscura` run as `obscura mcp --stealth`; its default block on
  loopback, private and link-local addresses stays on, so pages cannot reach
  the LAN). The child gets a cleared
  environment: `PATH`, `HOME`, `NODE_ENV` and only the variables the server
  declares. Its stderr tail appears in errors with configured values redacted.
  A dead process is restarted on the next request.

Server-to-client requests are answered conservatively: `ping`, `roots/list`
(no roots), `elicitation/create` (declined) and method-not-found otherwise.

### Credentials

Auth kinds: `none`, `headers` (for example `Authorization: Bearer <key>`),
`oauth`, and `env` for bundled servers. Values are write-only: the API and the
page show names and whether they are set. Omni seals them with AES-256-GCM
under a key in `gateway.key` (mode 0600, in the data directory beside the
database, created on first boot), with the row as associated data, so a copy
of the database alone reveals nothing. A credential is bound to the URL origin
and auth kind it was stored for; changing either clears it, and a credential
is never sent anywhere else.

OAuth integrations are connected from the page: Omni discovers the
authorization server (RFC 9728, from the 401 challenge or well-known paths,
then RFC 8414), registers a client for its callback (RFC 7591), sends the
browser to the authorization URL with PKCE S256 and `resource`, and on
`/gateway/oauth/callback` checks the single-use `state` (ten minutes) and
`iss`, exchanges the code and stores the tokens. The callback uses the public
origin when configured, else the UI's origin. Access tokens are refreshed a
minute before they expire and once after a 401; a rejected refresh asks the
owner to reconnect.

### Catalogs, health and failures

Catalogs (`tools/list` pages, resources and templates when the capability is
offered, `events/list` when MCP Events are offered) are stored as JSON, so the
endpoint serves the last known catalog right after a restart while every
enabled integration refreshes in the background. Refreshes happen at boot,
hourly (`McpGatewayRefresh` at :07), after an upstream `list_changed` (two
seconds of debounce), after a configuration or credential change, and on
demand. Pagination follows `nextCursor` up to 100 pages and stops on a
repeated cursor. A refresh records added, removed and changed tools; a
failed refresh keeps the previous catalog (`stale`).

Statuses: `pending` (no catalog yet), `ready`, `stale`, `needs_auth`
(credentials missing, rejected or expired), `error`, `disabled`. Each error
has a kind, a message and a hint on the page.

Calls: at most eight at once per integration, each with the integration's
timeout (5–600 s, default 120). A timed-out or cancelled call sends
`notifications/cancelled` upstream. Retries happen only when the request
certainly did not run (expired session, a 401 an OAuth refresh can fix);
reads (lists, `resources/read`, `events/poll`) also retry once after a
network or 5xx failure. `tools/call` is never retried after an uncertain
outcome.

`resources/read` goes to the integration that listed the URI or a template
matching it, else the one whose tool result returned it (resource links and
embedded resources, last 2,000 URIs), else the only integration offering
resources.

Visibility: tools can be hidden per integration; a hidden tool is neither
listed nor callable. MCP activity records each call's served name, duration
and outcome, never its arguments or result; a failed call keeps the first
500 characters of the integration's error text. While Omni records side
effects (tests, shadow runs, `--preview`), integration tools are never called:
`tools/call` answers with an `isError` result saying so; OAuth tokens are not
refreshed and Connect is refused.

## Events

`events/list` at the gateway offers Omni's events (`docs/mcp-events.md`) and
each enabled integration's events that support `events/poll`, as
`<id>__<event>` with `delivery: ["webhook"]` and a `[Integration]` prefix on
the description. Every subscription lives in Omni's outbox, owned by
`gateway:client:<id>` or `gateway:grant:<id>`, and is delivered by Omni's
worker with its signing, retries and stable event IDs. A delivery is
withheld while its owner is revoked and fails when the subscription ends.

Omni follows an integration event with a feed (`gateway/events.rs`): one
`events/poll` subscription per event and argument set, polled by
`McpGatewayEventFeeds` (four times a minute, at the integration's
`nextPollMs`, at most every 15 s and at least every five minutes) while any
current subscription uses it.

- A new feed asks for the current cursor first, so a refused subscription
  fails at once and history is never delivered.
- Each upstream event is queued once per feed (its receipt is keyed by the
  feed and the upstream event id) before the cursor moves past it, so a crash
  between the two replays harmlessly.
- A restart or an upstream outage resumes from the stored cursor (failures
  back off up to 15 minutes); everything the integration can still replay
  arrives once. artifacts.boo replays the last 5,000 events per artifact.
- When the integration reports `truncated` (the cursor fell out of its
  history), the feed records a gap with its time on the page and logs a
  warning. Those events are gone; reconcile with the integration's own tools
  (`artifact_activity` for artifacts.boo).
- A feed idle for more than an hour (no current subscription) starts again
  from "now" when a subscriber returns.
- Payloads over 32 KiB are not queued and count as a gap.
- Integration events that offer only webhook or push delivery are listed on
  the page as unsupported and not offered downstream. Nothing on Omni's side
  receives inbound webhooks.

MCP catalog notifications (`list_changed` on the GET stream) are separate
from these durable business events: they carry no data, are not stored, and
a client that misses one simply lists again.

## Security boundaries

- Public: exactly ten paths reach Omni through the `gateway.syas.ca` proxy
  host: `/gateway/mcp`, `/gateway/oauth/{authorize, authorize/status,
  authorize/complete, token, register, callback}`,
  `/.well-known/oauth-authorization-server` and
  `/.well-known/oauth-protected-resource` (bare and `/gateway/mcp`). NPM
  forwards the raw request URI, so `deploy/gateway/route.mjs` checks
  `$request_uri` against that list; any other path, including encoded,
  dotted, doubled-slash and trailing-slash variants, is a 404 at the proxy.
  Omni enforces the same list again (`gateway::public`): a request whose
  `Host` is the public host gets a 404 for anything else, so the management
  page (`/gateway`), `/api/*` and the SPA fallback stay LAN-only even
  if the proxy rule changes. A test keeps both lists equal.
- Upstream URLs must be public HTTPS; DNS answers are checked at connect.
- Stdio integrations run only image-bundled servers with a cleared
  environment.
- Credentials are sealed, origin-bound, write-only, and never logged; error
  texts strip URLs from network errors and redact stdio stderr.
- Downstream tokens are hashed (client) or signed and grant-checked (OAuth).

## Operations

- Config: `OMNI_GATEWAY_PUBLIC_URL` (an `https://` origin, no path). Without it
  the gateway serves the LAN and offers no downstream OAuth.
- Data: `mcp-gateway-*` entities and `gateway.key` in `/data`. Losing the key
  loses stored credentials (reconnect integrations); it is in the Kopia
  snapshot with the rest of `volumes/omni`. `mcp-events.key` beside it holds
  the event outbox key, migrated once from `OMNI_MCP_TOKEN` on the first boot
  of the gateway-only version; the variable can then be removed from Compose.
- Tasks: `McpGatewayRefresh`, `McpGatewayEventFeeds`; the refresh worker is a
  background service.
- Proxy host: from the directory containing `deploy/gateway/route.mjs` on Boris,
  ```sh
  docker exec -i -w /app -e OMNI_GATEWAY_ROUTE_MODE=inspect npm node --input-type=module < route.mjs
  docker exec -i -w /app -e OMNI_GATEWAY_ROUTE_MODE=apply npm node --input-type=module < route.mjs
  docker exec -i -w /app -e OMNI_GATEWAY_ROUTE_MODE=rollback npm node --input-type=module < route.mjs
  ```
  Public DNS for `*.syas.ca` already points at the proxy and the `*.syas.ca`
  certificate covers the name.

- ChatGPT: a custom connection stores the tool list and event catalog when it
  is created and changes them only on "Refresh tools" (Plugins, the
  connection, Manage); it ignores `tools/list_changed`, and existing
  conversations keep their old tools. `scripts/chatgpt/refresh-connector.py`
  presses that button through ChatGPT's internal web API with the Codex
  ChatGPT login and prints what changed; run it after a deploy that changes
  tools or events.

## Switchover (completed 2026-10-19)

Harnesses used to reach Omni through a separate tool gateway and the
`executor-events-adapter` sidecar. On 2026-10-19:

0. Public access: NPM proxy host 48, `gateway.syas.ca`, was created with
   `deploy/gateway/route.mjs` (Operations), `OMNI_GATEWAY_PUBLIC_URL=https://gateway.syas.ca`
   was added to the omni service in `/home/michael/compose`, and
   `python3 deploy/gateway/oauth-check.py https://gateway.syas.ca http://omni.boris`
   passed. Rerun that check after changing the route or the public-host guard;
   it walks ChatGPT's OAuth flow, checks that only `/gateway/*` and the OAuth
   documents are public, and revokes its grant.
1. The integrations were configured on `/gateway`: context7, deepwiki, grep,
   mcplookup (no auth); github_com and serpapi_mcp (headers,
   `Authorization: Bearer <key>`); artifacts and artifacts_support (OAuth);
   karakeep (bundled).
2. The harnesses moved to the gateway, each with its own client token or
   grant: the ChatGPT app (OAuth), Codex (`/etc/codex` config) and Claude Code
   (user MCP server `omni`).
3. The adapter's NPM route on `mcp.syas.ca` was rolled back, the sidecar was
   stopped and removed with its deploy-timer check, and its crate, image
   workflow and `deploy/executor-events/` were deleted from the repository.

Rollback of the public gateway route:
`docker exec -i -w /app -e OMNI_GATEWAY_ROUTE_MODE=rollback npm node --input-type=module < route.mjs`
from the directory holding `deploy/gateway/route.mjs` on Boris removes host 48;
LAN harnesses keep `http://omni.boris/gateway/mcp`. The old `/mcp` endpoint
was removed afterwards, so there is no other route to point a harness at.

## Known limits

- Upstreams must speak Streamable HTTP (2025-03-26 or later) or be a bundled
  stdio server; the deprecated HTTP+SSE transport and arbitrary commands are
  not supported, and private-network upstreams are refused.
- Integration events need `events/poll`; webhook-only upstream events are not
  followed.
- Prompts, sampling, elicitation, completions and resource subscriptions are
  not proxied.
- `tools/list` is served as one page.
- A modern-era (2026-07-28) call through Cloudflare that runs longer than
  100 s can be cut by Cloudflare; legacy-era calls get keepalives.
- Upstream OAuth servers without dynamic registration (pre-registered
  clients, such as GitHub's) are not supported; use a header token instead.
- Every gateway client and grant acts for the one owner and can call every
  visible tool, by design; hide tools you want no harness to have.
- Integration event feeds are polled one after another; an integration that
  times out delays the others' polls on that pass (each feed then backs off).
- Changing an integration's URL or transport restarts its feeds at the new
  server's current cursor and records a gap: events the old server held but
  had not delivered are not carried over.
- A flood of token requests can still delay a legitimate refresh by a few
  seconds (it is retried by the client).
- Bundled stdio servers run as Omni's own user inside the container, so a
  compromised package could read Omni's data directory. The cleared
  environment and pinned, script-free lockfile limit this; a separate user
  would need a root container.
