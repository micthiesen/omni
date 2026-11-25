# Architecture

Omni is one Rust binary (`omni`) that runs scheduled tasks, an axum
HTTP server with the JSON API, MCP gateway and Leptos frontend, and long-lived
background services over one SQLite document store.

## Crate layout

The workspace (`Cargo.toml`, `members = ["crates/*"]`) has four layers. Crate
names are `omni` and `omni-*`; library names are `omni` and `omni_*`.

- **Foundation:** `omni-core`, `omni-config`, `omni-store`, `omni-tasks`,
  `omni-http`, `omni-alerts`, `omni-mailer`, `omni-ai`, `omni-server-kit`,
  `omni-mcp-kit`, `omni-runtime`, `omni-api`, `omni-image`,
  `omni-recipe-format`. They know nothing
  about features. `omni-image` depends on no other workspace crate: it decodes
  untrusted images (JPEG, PNG, GIF, WebP, HEIC by magic bytes) in memory under
  pixel, allocation and time limits, one at a time, and produces oriented JPEG
  variants and an average colour. `omni-recipe-format` is pure too (no I/O,
  no clock, builds for wasm): the recipe document, its validator and its
  renderers, shared by `omni-recipes` and the recipe app
  ([recipes.md](recipes.md)).
- **Subsystems:** `omni-imap`, `omni-email`, `omni-parcel`, `omni-calendar`,
  `omni-live`, `omni-ios-controls`, `omni-live-intel`, `omni-presspods`,
  `omni-podcasts`, `omni-media`, `omni-arr`, `omni-reminders`, `omni-mcp`,
  `omni-device-link`, `omni-personal`, `omni-stylist`, `omni-youtube`,
  `omni-contacts`, `omni-recipes`. Each exposes
  a constructor that returns an `omni_runtime::Subsystem`. `omni-mcp` hosts
  the MCP gateway (`/gateway/mcp`, [gateway.md](gateway.md)), Omni's only MCP
  endpoint, which serves every tool together with the configured upstream MCP
  integrations'.
- **App:** `omni` builds the `AppContext`, constructs every subsystem,
  wires ports, and owns boot, the ops routes, dashboard SSE, the data manager and
  the CLI.
- **Outside the backend layering:** the frontend crates `omni-web`,
  `omni-web-kit`, `omni-web-pages` and `omni-recipes-web` (Fond's phone app,
  which also uses `omni-recipe-format`; `docs/recipes-app.md`); the dev-only
  `omni-testkit`; and `xtask`.

`cargo xtask deps-check` (part of the gate and CI) enforces the direction:

```
omni-core <- {omni-config, omni-store} <- omni-tasks <- omni-runtime <- subsystems <- omni
omni-core <- omni-http <- {omni-alerts, omni-ai, omni-mailer} <- omni-runtime
omni-api (serde only) <- omni-server-kit, subsystems, frontend crates
```

Subsystem crates must not depend on each other, except for three allowlisted
library edges: `omni-ios-controls -> omni-live`, `omni-parcel -> omni-email` and
`omni-calendar -> omni-email`. Frontend crates may use only `omni-api` and each
other. Nothing depends on `omni`.

## Composition and ports

`omni_runtime::AppContext` carries the shared handles: config, store, clock,
HTTP clients (`HttpClient`, SSRF-guarded `PublicHttpClient`), Pushover, the
optional mailer, model client and cost recorder, task registry, event bus,
shutdown token, `TaskTracker`, ports, `SideEffectMode` and resolved paths.

A `Subsystem` contributes its axum router (routes at absolute paths), tasks, MCP
tools, entity descriptors, data-manager rows, boot steps, background services,
email handlers, alert gates and host apps. `omni` merges the routers, registers
the tasks and tools, and starts the services.

A host app (`omni_server_kit::host_app::HostApp`) is a second web app on a host
of its own, such as the recipe app on `RECIPES_APP_URL`. Its router is never
merged into Omni's. `boot::app_router` applies `host_app::dispatch` outermost,
outside the public-host allowlists: a request whose `Host` (lowercased, without
port or trailing dot) is the app's goes to the app's router and nowhere else, so
on that host there is no Omni UI, `/api/*` or `/gateway`, and on every other
host the app's routes do not exist. An unmatched path there is a 404 (JSON under
`/api`), never a fallback page, and panics are caught as on Omni's router.
Config refuses an app host equal to the gateway's or PressPods' public host. A
host app has no login, so its mutations pass `host_app::MutationGuard`: the
exact app `Origin`, a same-origin `Sec-Fetch-Site` when present, and
`application/json` or, on the app's upload paths, its custom header.

Cross-subsystem calls go through the port traits in
`crates/omni-runtime/src/ports.rs`: `EmailReader`, `ArchiveEcho`,
`EmailRetryHandlers`, `CalendarConnection`, `LiveDirectory`, `LiveIntelligence`,
`SpeechTranscriber` (the local speech model, for YouTube transcripts),
`PageFetcher` (the PressPods retrievers' candidates for a public URL;
`docs/presspods-retrieval.md`),
`OnDeckSource`, `ClaudeSessionNotifier`, `ClaudeHost` and
`EventPublisher` (MCP Events publishing; `docs/mcp-events.md`). Each
port is set once during wiring (`crates/omni/src/wiring.rs`); a consumer
must handle an unset port, because the providing subsystem may be disabled by
configuration. Port payloads that belong to another subsystem's `omni-api` DTO
cross as `serde_json::Value` and are decoded into the same DTO on the other side.
The Pushover client also takes one `AppPushRoute`, set by wiring from
`omni-ios-controls`, so every notification tries the iOS app first
(`docs/ios-app.md`).

`SideEffectMode::Record` makes every mutating adapter (Pushover, SMTP, IMAP
writes, CalDAV, Arr/Castro/Plex mutations, APNs, webhooks, printer, Parcel)
record instead of sending, while reads stay live. Tests and shadow runs use it;
select it with `omni --side-effects=record` or `OMNI_SIDE_EFFECTS=record`.

## Boot order

`crates/omni/src/boot.rs`:

1. Load config and log it redacted.
2. Open the store.
3. Construct subsystems (they do not read the docstore in constructors).
4. `migrate_all` over every subsystem's entity descriptors.
5. Import historical costs.
6. `Migrate` and `Services` boot steps.
7. `registry.initialize` marks interrupted runs.
8. `Reconcile` boot steps (interrupted MCP calls, calendar hash reconcile).
9. Start the HTTP server.
10. `AfterServer` boot steps.
11. Register tasks, start background services (email features restart with
    30 s to 300 s backoff, MCP delivery worker, Reminders health check), start the
    scheduler, then run catch-up recovery as tracked work.

`--server-only` stops after step 10. Shutdown cancels the token, stops accepting
requests, closes MCP and SSE streams, and waits up to 30 s for tracked work.

Livestream intelligence is optional at boot: if its speech or speaker model
files are missing, empty or corrupt (`omni-live-intel/src/model_check.rs`), boot
logs `Livestream intelligence disabled: ...`, installs no `LiveIntelligence`
or `SpeechTranscriber` port and continues.

## Data compatibility

The production database is the SQLite file at `DB_NAME`
(`/data/docstore.db` in the container). It holds rows written since the service
first ran, so its format is a contract:

- One connection on a dedicated thread, WAL, `synchronous=NORMAL`,
  `busy_timeout=5000`, writes in `BEGIN IMMEDIATE`.
- The `blobs` table keys are `$<entity>#` plus parts joined by `#`
  (`s<utf16 len>:<value>`, `n<number>`, `b1`/`b0`). Reads hide expired rows.
- Payloads are CBOR. The decoder reads every format already stored, including
  node-cbor 10 rows written by earlier versions (tag 0/1 dates, tag 258 sets,
  bignums, explicit `undefined`); the committed node vectors in
  `crates/omni-store/tests/golden/cbor.json` freeze that. The encoder writes
  definite-length CBOR with integer heads for integral numbers, 64-bit floats
  otherwise, tag 1 dates, and objects without their `undefined` properties;
  `cbor::same_value` defines the equality every write must preserve.
- Every entity struct carries `#[serde(flatten)] extra: Extra`, so a
  read-modify-write never drops fields it does not know.
- Values derived with JS semantics and then persisted or hashed (UTF-16 lengths,
  `JSON.stringify`, `Number#toString`, `localeCompare`, date parsing) go through
  `omni_core::js` only.
- Entity migrations run at boot through `migrate_all` from each subsystem's
  `EntityDescriptor` list. Reproducible data changes are boot migrations, not
  manual edits.

`omni compat-audit --db <copy>` is a read-only decode and health audit
of a copy of production: per-entity row counts, NULL payloads, CBOR and typed
decode failures, primary-key recomputation, unknown or legacy rows, and the
typed round trip `decode -> typed -> encode -> decode`. The source file is
copied to a temporary directory first and never opened in place. Take the copy
with `sqlite3 docstore.db ".backup /tmp/copy.db"` on boris, keep it mode 600
outside the repository, and point `OMNI_PROD_COPY` at it to run the ignored
`prod_copy` tests.

## Database snapshots

`DbSnapshot` (`crates/omni/src/snapshots.rs`, hourly at minute 47) writes a
consistent copy of the docstore to `/data/snapshots/docstore-<UTC
stamp>.db`. It runs `VACUUM INTO` from a read-only connection in a child
process (`omni db-snapshot`) under `nice -n 19 ionice -c 3`: one read
transaction, so the copy is consistent, and in WAL mode the server's writes
never wait for it. The copy is written as `*.db.tmp` (which Kopia's policy
skips), checked, fsynced and renamed.

- Restore check: every snapshot passes `PRAGMA quick_check` and has rows;
  the first one after 03:00 local each day passes `PRAGMA integrity_check`
  and the compat audit (every row decodes with this build's entity types).
  A file that fails is deleted and the run fails (ERROR log, alert).
- Retention: the newest 24 snapshots plus the newest of each of the last 7
  days. Kopia keeps older history.
- Backup: Kopia backs up `snapshots/` and skips the live `docstore.db` and
  its `-wal`/`-shm` (`~/compose/.kopiaignore`), so every backed-up database
  is a checked, consistent file.
- `/api/health/deep` reports `db_snapshot` (not critical) when the newest
  snapshot is older than 2 h 15 min.
- Restore: stop the container, copy a snapshot over
  `volumes/omni/docstore.db`, delete `docstore.db-wal` and `docstore.db-shm`,
  start it.

## Retention

`Retention` (`crates/omni/src/retention.rs`, daily at 04:37) deletes Omni's
own operational records that nothing else bounds: MCP call traces after 30
days, email archive and compose receipts after 180, parcel submission dedup
rows after 365, rows of retired features (`shell-run`), per-run Markdown logs
under `LOGS_PATH/<feature>/` after 90 days, daily `omni-*.log` files after 14
(previously swept only at process start), and orphaned `pp_*`/`omni-*` scratch
files in the temp directory after a day. It never touches recipes, reminders,
mail, media, contacts or the stylist ledger. Bounded elsewhere: task runs
(newest 50 per task), the event journal (7 days, failed 30), email activity,
YouTube transcripts, archives and DB snapshots. Kept on purpose: AI cost
events (all-time totals), recommendation attempts and taste evidence (dedup),
and the calendar's tracked events (their tombstones keep deleted events
deleted). Deleting rows does not shrink `docstore.db`; snapshots are compact
(`VACUUM INTO`).

## Golden fixtures

External contracts are frozen by committed fixtures: HTTP JSON responses
(`crates/omni-api/tests/golden/http/`), MCP `tools/list`, handshake and policy
(`crates/omni-mcp-kit/golden/`), the PressPods RSS feed and ffmpeg arguments,
CBOR vectors, MIME parsing, config parsing, cron reference times and JS
semantics cases.

Committed fixtures are synthetic. `cargo xtask capture-golden --base URL`
captures read-only GETs and the MCP handshake from a running server into the
gitignored `.local/golden-capture/`; `cargo xtask golden-synthesize` derives the
committed HTTP fixtures from that capture, replacing emails, IDs, URLs, IPs and
free text consistently and failing if any replaced value leaks.
`crates/omni-api/tests/golden_privacy.rs` scans every committed fixture for
personal markers. `cargo xtask golden-check` verifies the MCP policy and that
committed fixtures are unchanged.

Each MCP tool's public contract is a `ToolDef` with `schemars`-derived schema
types in its package's `defs` module; `crates/omni-mcp-kit/golden/tools-list.json`,
the policy inventory and the `tools/list` copies in the protocol fixture are
generated from those definitions. After changing one, run
`cargo xtask mcp-golden` and review the diff (see `docs/mcp.md`).

## Frontend

`omni-web` is a Leptos 0.8 client-side app built by trunk
(`crates/omni-web/Trunk.toml`) into `crates/omni-web/dist`. `omni-web-kit` holds
the API client, live data, hooks, components, SVG charts and markdown;
`omni-web-pages` holds the domain pages. The REST contracts in `omni-api` are the
only API; there is no SSR.

Live data uses one `EventSource("/api/events")` per tab. The server sends a ping
immediately, then the client's initial snapshot, then debounced broadcasts.
While the stream is down the client polls `/api/snapshot` every 10 s and shows a
reconnecting badge. Run logs stream from `/api/task-runs/:id/logs/stream`.

The server serves the built app from `OMNI_WEB_DIST` (`/app/web` in the image):
hashed `/assets/*` are immutable and Brotli-precompressed, `index.html` is
`no-cache`, and unknown `/api/*` GETs return JSON 404. trunk injects no inline
script, so the `/reminders` CSP needs only `'wasm-unsafe-eval'`.

Build identity (`omni_api::build::BuildIdentity`, computed once at boot in
`omni/src/build_identity.rs`) rides on every snapshot and on
`/api/health`. `server` is `OMNI_IMAGE_REVISION` (the Dockerfile sets it from the
`OMNI_REVISION` build arg, which CI fills with the commit SHA), else a
fingerprint of the executable's path, size and mtime. `frontend` is a hash of the
served `index.html`, which names every content-hashed asset. A page records the
first snapshot's build and shows the update state (design system, section 8.1)
when a later one differs.

## Deployment pipeline

`Dockerfile` stages, all pinned and SHA-256 verified:

- download stages for cargo-chef, the sherpa-onnx static library, trunk /
  wasm-bindgen / wasm-opt (`deploy/web-tools/install.sh`), yt-dlp and the three
  livestream models;
- `chef -> planner -> server`: cargo-chef cooks dependencies, then one
  `cargo build --release --locked` builds `omni`, `omni-voice-enroll` and
  `omni-intel-doctor`. The server stage excludes the web crates
  and `xtask`, so editing them keeps it cached;
- `web -> web-dist`: `trunk build --release`, then Brotli and gzip copies;
- `mcp-stdio`: `npm ci --ignore-scripts` of `deploy/mcp-stdio/package-lock.json`,
  the stdio MCP servers the gateway bundles (copied to `/app/mcp-stdio`);
- `runtime` on `node:24.22.1-slim` (Node stays for `yt-dlp --js-runtimes node`
  and the bundled stdio MCP servers),
  with ffmpeg, CUPS/brlaser, ghostscript, poppler and tini. Layers run from least
  to most frequently changed so a code change pushes only the last layers.
  `RUN omni doctor --image` fails the build when ffmpeg filters, the
  denoise model, printer files, sherpa models, yt-dlp, Node or a bundled MCP
  server are missing, or when the in-process HEIC to JPEG conversion fails.

The Rust stages rely on cargo-chef layers, not cache mounts, because the GitHub
Actions cache exports layers but not mounts.

CI (`.github/workflows/ci.yml`) runs three jobs in parallel, using
`Swatinem/rust-cache` for the Rust ones. `lint` runs fmt, native and wasm32
clippy with `-D warnings`, `cargo xtask deps-check` and `cargo deny --locked
check`. `test` runs `cargo nextest run --workspace --locked --profile ci`.
`image` builds the Dockerfile with `type=gha,mode=max` layer caching. `publish`
pushes `ghcr.io/micthiesen/omni:latest` and `:<sha>` after all three
pass, from the fully cached build.

On boris, `omni-deploy.timer` runs `deploy_omni.sh` every minute
(`deploy/boris/`, installed with `sh deploy/boris/install.sh`; the copy in
`~/compose/bin` is not edited by hand). It pulls `:latest`, tags the running
image `:previous`, recreates only the `omni` container (`up -d --no-deps`;
the service has no `depends_on`, so Plex and the Arr apps are never
recreated with it), waits for the health check (`omni healthcheck`) and runs
`omni_smoke.py`: `/api/health` names the new revision, `/api/health/deep`
passes its critical checks, and with `OMNI_SMOKE_TOKEN` (a gateway client
token in `~/compose/.env`, minted with `omni gateway-token`) an MCP
`tools/list` and the read-only `system_status` and `tasks_list` calls
succeed. A failure starts `:previous` again, records the failed image id in
`~/compose/.omni-deploy-bad` so later ticks skip it until a newer image is
published, and sends a priority-1 push; a success sends a silent notice.
