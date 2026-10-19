# Boris deployment

This is a separate Compose project under `/home/michael/compose/executor-events`.
It joins the existing `compose_agent-integrations` network and publishes no host
ports. Executor core, its data/authentication, login/UI, and every other service
keep their current configuration.

## Deployment

Boris's `omni-deploy.timer` keeps the adapter current in the same pass
that updates Omni. Every minute `bin/deploy_omni.sh` pulls
`executor-events-adapter:latest`; when the image or the project's Compose config
hash changed, it recreates only the adapter, waits for its health check and sends
the same Pushover success or failure notices as Omni. The `Executor Events
Adapter` workflow builds the Rust `omni-events-adapter` crate with
`Dockerfile` in this directory (repository root as context) and publishes the
image only when that crate, the Dockerfile, the workspace manifest, lock file,
toolchain or the workflow changes.

The Rust image has no `node`. Its own `HEALTHCHECK` runs
`executor-events-adapter healthcheck`, so `compose.yml` here no longer overrides
it. Re-run `install.py` on Boris with this `compose.yml` before the first Rust
image is published; an installed copy that still overrides the health check
with `node` marks the new container unhealthy and fails the deploy.

The adapter reads `OMNI_MCP_TOKEN` from Boris's main Compose `.env` by
interpolation, the same value Omni receives, so a rotated token reaches it on the
next deploy check without copying credentials.

`install.py` sets up the project once, or again after `compose.yml` changes here.
Copy this directory to Boris (`executor-events/source/` holds the current copy)
and run:

```sh
python3 install.py --owner EXISTING_EXECUTOR_USER_ID
```

It writes the nonsecret owner to mode-0600 `deployment.env`, copies
`compose.yml`, then pulls and starts the adapter. It does not print credentials
or edit the main Compose files, Reminders config, or the main `.env`.

Omni defaults its delegated authorization check to
`http://executor:4788/api/auth/mcp/get-session` when Dockerized. The optional
`OMNI_EVENTS_EXECUTOR_AUTH_URL` overrides that fixed internal endpoint. The
adapter's owner ID is an existing Executor user, not a new account or API key.

## Other servers' events

To offer another MCP server's events (artifacts.boo workspaces), add
`EVENT_UPSTREAMS='[{"key":"...","url":"https://.../mcp","token":"..."}]'` to
`deployment.env` (mode 0600) and recreate the adapter with the Compose command
below. See `docs/mcp-events.md`.

## Exact MCP route

`route.mjs` is the one JavaScript file left in the repository on purpose. It
runs inside the Nginx Proxy Manager (NPM) container with NPM's own Node runtime
and imports NPM's models and access checks, so it needs no Node tooling here.

It is deliberately specific to the inspected NPM proxy host 44:
`mcp.syas.ca`, upstream `executor:4788`, no NPM access list. It refuses drift.
It adds one `location = /mcp` in that host's advanced configuration. OAuth,
well-known endpoints, UI paths, TLS and access lists remain unchanged.

From the directory containing this file on Boris:

```sh
docker exec -i -w /app -e OMNI_ROUTE_MODE=inspect npm node --input-type=module < route.mjs
docker exec -i -w /app -e OMNI_ROUTE_MODE=prepare npm node --input-type=module < route.mjs
# After compatibility, rollback and adapter health checks pass:
docker exec -i -w /app -e OMNI_ROUTE_MODE=apply npm node --input-type=module < route.mjs
```

Prepare validates an independent nginx configuration and saves a private copy
of the exact prior configuration. Apply uses NPM's own permission checks,
audit log and update/configuration API, with a short-lived locally signed token
for the host's existing owner. That token is kept in process memory and is not
persisted or printed. No user, permission, OAuth client, or signing key changes.
The script rejects concurrent edits and verifies unchanged host fields.

## Rollback

```sh
docker exec -i -w /app -e OMNI_ROUTE_MODE=rollback npm node --input-type=module < route.mjs
```

This restores the exact original advanced configuration, including unrelated
settings. MCP immediately reaches the unchanged Executor host again. Existing
legacy clients retain their endpoint, credentials and OAuth metadata. Event
subscriptions and outbox remain in Omni; rolling back the route does not erase
or unsubscribe them. Stop subscriptions first if delivery should stop.

After routing rollback, the unused adapter can be stopped from
`/home/michael/compose/executor-events`; stop the deploy timer's adapter check
first or it restarts the container:

```sh
docker compose --env-file ../.env --env-file deployment.env -f compose.yml stop
```

Do not stop or recreate Executor or NPM to update the adapter. Archive receipts
and event data are additive and do not require a destructive migration.

## Verify

Check adapter health and image revision, then exercise existing legacy
initialize/tools/list, native elicitation, modern server/discover/events/list,
OAuth 401 challenge and login/UI paths. Confirm other hosts retain their prior
status and routing. Use only read-only tools and synthetic mail fixtures.
Connected ChatGPT rescanning, subscription/challenge, matching callback delivery,
and idle-dot wake still need the parent-run lifecycle in `docs/mcp-events.md`.
