# Omni

Edit `AGENTS.md` and `.agents/skills/` directly. These are native Codex files;
no generated copies or compatibility links are needed.

Personal automation service for livestream monitoring, email processing,
recommendations, and PressPods. It is a Rust workspace: one `omni` binary serves the API, MCP endpoint and a Leptos
frontend, and runs every scheduled task. See `docs/architecture.md`.

Update this file when work establishes or changes a durable project convention.

## Work and delivery

Use `cargo` from the repository root. The toolchain comes from
`rust-toolchain.toml` (rustup installs it on first use, plus
`rustup target add wasm32-unknown-unknown` for the frontend). After code
changes, run:

```bash
cargo xtask gate                         # hygiene, fmt --check, clippy -D warnings
                                         # (native and wasm32), deps-check, tests
                                         # (cargo-nextest when installed)
cargo deny --locked check                # advisories, licenses, bans, sources
(cd crates/omni-web && trunk build --release)   # when the frontend changed
```

`cargo xtask gate` is the local equivalent of CI's lint and test jobs, except
`cargo deny` (install with `cargo install --locked cargo-deny@0.20.2`, the CI
pin). Tests run under cargo-nextest (`.config/nextest.toml`; CI pins
`cargo-nextest@0.9.143` and uses `--profile ci`) and fall back to `cargo test`
when it is missing. trunk, wasm-bindgen and wasm-opt versions are pinned in
`deploy/web-tools/install.sh` and `crates/omni-web/Trunk.toml`; wasm-bindgen
must equal the workspace `wasm-bindgen` pin. `cargo fmt --all` fixes formatting.

Build directories grow without bound. The gate starts with
`cargo xtask hygiene`, which measures every Cargo profile directory under
`target/` and, above `OMNI_TARGET_LIMIT_GIB` (default 40), sweeps the oldest
artifacts with `cargo sweep` (`brew install cargo-sweep`) down to half the
limit. Run one heavy build session at a time. Concurrent agents in the same
checkout each set their own `CARGO_TARGET_DIR=target/agents/<name>` (and a
separate one for wasm or trunk work) so they do not block on Cargo's lock or
invalidate each other's artifacts; hygiene covers those directories too.

Completed, reviewed work is authorized to ship directly to `main`: preserve
unrelated changes, commit the scoped result, pull with rebase if the remote moved,
and push. A push deploys production automatically. Do not ask for another
confirmation already supplied by this rule or the current request.

Production runs as the `omni` container on `boris` (`10.10.1.100`) from
`/home/michael/compose`. Persistent data is under
`/home/michael/compose/volumes/omni`. The LAN UI is
`http://omni.boris/`. Inspect failures with `docker logs omni` over SSH.

Prefer a committed boot migration for reproducible data changes. For one-off
database surgery, stop the container and make a WAL-safe backup first. Direct
destructive production data changes require authorization specific to that
change; reuse authorization already given in the conversation.

## Rust conventions

- **Dependencies:** every external dependency is declared once in the root
  `[workspace.dependencies]` with an exact `=x.y.z` pin; crates use
  `workspace = true`. `Cargo.lock` is committed and every build and test uses
  `--locked`. `deny.toml` allows crates.io sources only and a fixed license list.
  The toolchain is pinned in `rust-toolchain.toml` and matches the Dockerfile
  `rust:` images and `rust-version`.
- **Lints:** every crate has `[lints] workspace = true`. `unsafe_code` is
  forbidden. `unwrap`, `expect`, `dbg!` and `print!`/`eprint!` fail the gate
  outside tests (`clippy.toml` allows unwrap and expect in tests; `xtask` opts out
  of the print lints). Dropped futures are denied.
- **Errors:** expected failures are `Result` values with a `thiserror` enum per
  domain module (`ImapError`, `CaldavError`, `ParcelError`, ...). Wrap foreign
  errors at the leaf; give retry-relevant errors an `is_transient()`-style
  method. `anyhow` is for binaries, `xtask` and tests only. Panics are bugs; they
  are caught and logged only at task and request boundaries. HTTP handlers return
  `ApiError`, which renders `{"error": "..."}`.
- **Concurrency:** tokio, structured. Spawn through
  `omni_core::spawn::spawn_tracked` on the app `TaskTracker`; nothing is
  fire-and-forget. Work that must not be split by a dropped request or shutdown
  (durable reservation, external call, record) runs inside `must_complete`. Fan
  out with `JoinSet` or `buffer_unordered(n)` with explicit limits. Use
  `tokio::sync::Mutex`/`Semaphore` for async critical sections. All "now" comes
  from `ctx.clock` and all sleeping from `tokio::time`, so
  `#[tokio::test(start_paused = true)]` controls time. Schedules are
  `omni-tasks` cron tasks, not raw intervals.
- **Logging:** `tracing` only. Each module declares `const LOG: &str = "Name";`
  and logs with `target: LOG`; the name is what the UI log viewer shows. Events
  inside a task run's span are captured into that run's bounded log by
  `omni_tasks::RunLogLayer`, including across `.instrument`ed async work. ERROR
  events reach Pushover through the gated, throttled `AlertLayer`. Never log
  secrets, Claude session prompt text, or email bodies at info.
- **Ports:** a subsystem crate depends only on foundation crates. Calls into
  another subsystem go through the port traits in `omni_runtime::ports`, wired
  once in `crates/omni/src/wiring.rs`; handle an unset port. The only
  direct subsystem edges are `omni-ios-controls -> omni-live`,
  `omni-parcel -> omni-email` and `omni-calendar -> omni-email`;
  `cargo xtask deps-check` enforces this.
- **Side effects:** every mutating external adapter honors `SideEffectMode`. In
  `Record` mode (tests, shadow runs, `--preview`) it records instead of sending.
  New adapters must do the same.
- **Persisted data:** the docstore holds rows written by earlier versions of
  the service; the `omni-store` CBOR decoder must keep reading every existing format
  (node-cbor rows, tag 1 dates, explicit `undefined`), while the encoder only
  has to write CBOR it reads back as the same value. Entity structs keep
  unknown fields in `#[serde(flatten)] extra: Extra`; use `Option` with
  `skip_serializing_if` for absent fields and a plain `Option` for stored
  nulls. Never change how keys, hashes, fingerprints, ids or idempotency keys
  are derived (calendar UIDs and Message-IDs keep the `@omni-notify` domain from
  before the rename to Omni): values derived with JS semantics and then persisted, hashed or
  compared with stored rows (UTF-16 lengths, `JSON.stringify`,
  `Number#toString`, `localeCompare`) go through `omni_core::js` only. Each
  subsystem lists its `EntityDescriptor`s; boot runs `migrate_all` over them,
  and `omni compat-audit` decodes a production copy against them.
- **Validation:** decode untrusted external, persisted, environment, AI and
  protocol values into typed serde structs before use. MCP tool inputs and model
  structured outputs are also checked against their JSON schemas (`jsonschema`).
- **Idempotency:** make notifications, queue operations, external writes and
  retries idempotent before reporting success. Reserve durably before the side
  effect, record after, and never retry an uncertain outcome automatically.

## Crate map

- `omni-core`: clock, ids, `js` semantics for stored derivations, digests, email model and handler
  trait, `spawn_tracked`/`must_complete`.
- `omni-config`: typed environment config, validation, redaction, defaults.
- `omni-store`: SQLite docstore actor, CBOR codec, entities, `migrate_all`,
  relational tables.
- `omni-tasks`: cron scheduler, task registry, durable run history, run log
  capture, catch-up, event buses.
- `omni-http`: reqwest client with the project user agent, bounded bodies,
  SSRF-guarded public client, `SideEffectMode`.
- `omni-alerts`: Pushover client, alert throttle, ERROR-log alert layer and gates.
- `omni-mailer`: outgoing SMTP and MIME with the fixed sender identity.
- `omni-ai`: model registry, provider clients, tool loop, cost accounting.
- `omni-server-kit`: axum helpers (body caps, errors, origin guard, SSE, SPA).
- `omni-mcp-kit`: MCP tool interface, schema derivation and generated snapshots.
- `omni-runtime`: `AppContext`, `Subsystem`, boot steps, services, ports.
- `omni-api`: wire DTOs shared by backend and frontend (serde only).
- `omni-testkit`: temp stores, test clock, recorders, fake models, no-network
  HTTP, golden helpers (dev-dependency only).
- `omni-imap`: iCloud IMAP transport, cursors, archive actions, drafts, Sent
  copies, compose, email MCP reads.
- `omni-email`: email dispatch, retry queue, watchdog, activity, triage, sender
  rules, feedback.
- `omni-parcel`: Parcel email handler and the budgeted delivery-status cache.
- `omni-calendar`: calendar email handler, CalDAV writes, and the primary
  calendar mirror, change feed and MCP tools.
- `omni-live`: database streamer configuration (UI and MCP), aggregate streamer state, notifications, viewer
  metrics and records, DGG discovery, `LiveCheckTask`, cached streamer avatars (`StreamerAvatars`).
- `omni-ios-controls`: the iOS app backend: signed routes, device registry,
  APNs alert route, live control slots, On Air and Deliveries widgets.
- `omni-live-intel`: livestream audio capture, local speech and speaker models,
  viewer anomalies, Destiny presence, summaries, alerts.
- `omni-presspods`: article retrieval, narration, speech, audio chain, storage,
  RSS.
- `omni-podcasts`: podcast recommendations, taste reflection, Castro bridge and
  Inbox cleanup.
- `omni-media`: Plex, TMDB, Radarr/Sonarr, media recommendations, taste
  reflection.
- `omni-arr`: `ArrRecovery` and `ObserverRepair`.
- `omni-reminders`: server iCloud Reminders and the shared protected-access
  (PCS) workflow; callers own authenticated requests, encrypted cookie
  persistence and per-account serialization.
- `omni-mcp`: the MCP gateway, Omni's only MCP endpoint (`/gateway/mcp`:
  Omni's tools, upstream integrations, downstream tokens and OAuth server,
  integration event feeds; `docs/gateway.md`), the MCP Events outbox, MCP
  activity, system/events/Claude session tools and the policy inventory.
- `omni-device-link`: the Claude Code host's `omni-link` long-poll relay.
- `omni-personal`: pets, LAN printer, Hister, Codex/Claude reset alerts
  (`reset_alerts`), and agent push notifications to the owner
  (`owner_notify`, the `notify_owner_*` tools).
- `omni-stylist`: the fit-first stylist ledger: typed records and events,
  private evidence, frozen predictions, narrow rules, the versioned runbook,
  archives and the `StylistBackup` task.
- `omni`: the binary: boot, wiring, ops routes, dashboard SSE, data
  manager, `doctor`, `healthcheck`, `compat-audit`, `stylist-*`, `--preview`.
- `omni-web`, `omni-web-kit`, `omni-web-pages`: the Leptos CSR frontend (app
  shell and ops pages, shared kit, domain pages), built by trunk.
- `xtask`: gate, hygiene, deps-check, golden capture and synthesis, MCP snapshots.
- `ios/OmniLive` (not a crate): the iOS/iPadOS app, its WidgetKit extension
  (Live Slot control, On Air and Deliveries widgets) and notification service
  extension, built from XcodeGen `project.yml`. See `docs/ios-app.md`; verify
  with `scripts/ios-live/typecheck.sh` and `scripts/ios-live/test.sh`.
- `slate/` (not a crate): Omni Slate, the battery e-paper display hardware project
  (ESP32-H2 on Thread, its own firmware workspaces, tscircuit PCB, BOM and test matrix).
  It has its own `slate/AGENTS.md` and gate (`sh slate/scripts/check.sh`), its own
  workflow, and never enters the image; the main workflow ignores `slate/**`. Its
  server side will be an `omni-slate` crate depending on `slate/firmware/protocol`; that
  crate must then be added to the `.dockerignore` allowlist and stop being ignored by
  `ci.yml`.

The frontend is Leptos 0.8 client-side rendering built by trunk into
`crates/omni-web/dist` and served from `OMNI_WEB_DIST`. It talks only to the
REST API in `omni-api`. Live data arrives over one `/api/events` SSE stream per
tab and falls back to polling `/api/snapshot` every 10 s while the stream is down.
The UI is dark only and follows `docs/design-system.md` ("Instrument"): tokens
and layered CSS in `crates/omni-web/style/`, self-hosted Geist fonts in
`crates/omni-web/assets/fonts/`, kit components in
`crates/omni-web-kit/src/components/`, and the rail, top bar, phone tab bar and
command palette in `crates/omni-web/src/shell/`. Change the doc with the UI.

## Failure-sensitive invariants

### Livestreams

YouTube discoveries resolve video ownership through YouTube metadata and durable
identity links. Never infer YouTube ownership from display names. A video linked
to its configured YouTube account enriches DGG presence without adding a second
viewer-count source. Failed identity revalidation preserves the last verified link.

A streamer is an aggregate identity over platform bindings. Notify only on the
aggregate offline-to-live and live-to-offline edges. The first live binding is
the sticky primary for the session; a primary switch is silent. Viewer counts
sum currently live bindings. Streamer configuration lives in the database
(`omni-live/src/config.rs`) and changes only through `StreamerConfigService`,
which validates every write (unique names and sources, no background override)
and reloads the roster. Display order lists every primary streamer before every
background one; a tier change moves the streamer to the boundary (last primary
or first background). A streamer removed at runtime is retired on the next
tick without an offline notification. Pushover tokens are write-only and never
cross MCP.

`tier: "background"` mutes live, offline, and title notifications, records only
all-time viewer highs, and polls every third tick. It cannot be combined with an
explicit `liveNotifications` value. Title changes use an eager debounce: the
first change sends immediately, later changes within ten minutes collapse to the
last title, and offline or primary-switch transitions clear pending state. A
viewer peak becomes a record only after count falls 5 percent below it; flush a
pending peak when the stream goes offline. Primary-tier streamers notify 7, 30
and 90-day window records as well as all-time records; the window maximum is
taken from history before the current observation, and a record already
confirmed today does not repeat (`omni-live/src/metrics/`). Streams outside the
configured roster (transient DGG discoveries) keep viewer metrics but never
push a record.

Viewer surges compare against a 5-20-minute-old baseline that must be flat, not
still climbing after go-live, and a platform surge must also reach the median
peak of recent full sessions. Sparse baselines after a restart or primary switch
suppress rather than alert.

Missing, empty or corrupt livestream speech, speaker or diarization model files
disable livestream intelligence at boot (`Livestream intelligence disabled: ...`)
and leave live checks running; they never crash boot.

Rolling summaries listen continuously. Each summary target gets one listener
(at most six) that captures back-to-back `LIVESTREAM_SUMMARY_INTERVAL_SECONDS`
windows (the first is two minutes) outside the voice capture queue and
summarizes each window while the next records. A window is diarized (pyannote
segmentation 3.0 with the CAM++ extractor), each speaker turn is transcribed,
and a speaker is labeled "Destiny" only when most of its scored voiceprint
windows match. The labels feed the summary prompt and never change Destiny
presence, which keeps its own short voice samples and LLM confirmation.
A capture that finds the stream has ended (yt-dlp "not currently live", an m3u8
404, or a `post_live`/`was_live`/`not_live`/`is_upcoming` status) is
`AudioError::NotLive`: never retried, recorded as a Skipped stage, no WARN.

### Email

- Dispatch is no-drop: events received during processing schedule another pass,
  and transport cursors commit only after dispatch. Message-ID is the stable
  identity across folder moves.
- iCloud IMAP uses per-folder UID cursors without CONDSTORE/QRESYNC. Re-read
  capabilities after authentication. Keep the seven-day INTERNALDATE guard that
  cursor-skips bulk imports, and on UIDVALIDITY change replay only from the
  last-dispatch watermark before reseating the cursor.
- Filter in this order: user block, user allow, static blacklist, static
  auto-pass, then shared LLM triage. Explicit user rules override built-ins.
- Activity outcomes must reflect per-item success. A fully rejected submission
  cannot be recorded as processed.
- Parcel extraction separates order numbers from tracking numbers, validates
  ranked carrier candidates against the live list, and uses durable dedup.
- Calendar output is sanitized before persistence. Cancellations require an
  explicit event reference; receipts and bills never imply cancellation.
- CalDAV discovery follows RFC 6764 from principal to home set to a VEVENT
  collection. Never hardcode an iCloud `pXX` shard. The pipeline updates an
  event by GET, merging only the fields it owns, and PUT with If-Match; a 412
  re-reads and merges at most three times, never clobbering manual or agent
  edits. A 403 `no-uid-conflict` merges into the moved copy in place.
- A create whose title matches one active tracked event on the same day at
  another time, or on another date within 45 days when the email uses
  reschedule language, updates that event instead of duplicating it.
- The primary calendar is the one VEVENT calendar named "iCloud" that matches
  `schedule-default-calendar-URL` when reported. Re-pin automatically only when
  that is unambiguous; otherwise fail closed with a `calendar_status` error.
- Calendar MCP writes reserve an idempotency key durably, use If-Match or
  If-None-Match, and verify by GET. Uncertain writes settle only by reading,
  never by resending. Invitations are read-only; organizer events with
  attendees need `attendeeNotifications: "send"`. The change feed is durable
  and cursor-paged. `calendar.event_changed` publishes feed rows after they
  commit, skips Omni tool writes by default and never delivers history;
  `calendar.event_starting` scans only while subscribed and dedups by
  occurrence start, so a reschedule fires again. See `docs/calendar.md`.
- Network and 5xx failures enter the durable retry queue. Re-fetch by email id;
  handler dedup makes replay safe.
- Systemic extraction failures (non-retryable 4xx, invalid output schema,
  missing key) park the retry row for the running build
  (`omni-email/src/systemic.rs`): they log at WARN and alert once per pipeline
  and signature per day. A boot under a different build releases at most 20
  parked rows (plus recent systemic `error` activity without a row), once per
  build and at most three builds per email. Only dedup-safe pipelines
  (`CalendarEvents`, `ParcelTracker`) replay.

### Parcel delivery status

Parcel's API limits are very low (about 20 reads per hour and 20 adds per day,
failed requests included). Only the `ParcelDeliveries` task reads deliveries:
every 30 minutes while anything is active or Omni submitted a number since the
last read, every 3 hours otherwise. A durable budget (at most 4 attempts per
rolling hour) is reserved before each request, a 429 pauses reads for at least
an hour, and `SideEffectMode::Record` never reads. `GET /api/parcels`,
`parcels_list` and `parcels_get` serve only the cache; never add a manual or
on-demand refresh. The add path (`parcel_api.rs`) is separate and unchanged.
See `crates/omni-parcel/src/deliveries/`.

### Notifications and runs

Every notification, including ERROR-log alerts, goes to the iOS app over APNs
first (`omni_alerts::AppPushRoute`, implemented in `omni-ios-controls`).
Pushover sends it only when the app is not set up: APNs is unconfigured, no
device has alerts enabled with a token, or every alert device definitively
rejected the push. A push with no APNs answer is uncertain: it never falls back
or resends. The route never logs at ERROR. See `docs/ios-app.md`.

Throttle each notification path at exactly one layer. Preserve distinct incident
keys for distinct users, URLs, subjects, or tracking numbers. Logs emitted during
a tracked task run remain attributable through async work and are bounded before
persistence.

A run that completes but skips its real work because an upstream failed calls
`omni_tasks::report_degraded` inside the run and is stored as `degraded` with the
reason in `error`; "no work due" and missing configuration stay `success`.
`TaskHealth` judges durable run history with the shared persistent-failure rule
(three consecutive failed or degraded runs spanning twelve hours, less five
minutes of jitter) and sends one Pushover note per incident whose streak includes a
degraded run, plus one recovery note after the next success. Plain error streaks
stay on the ERROR-log alert path and its gates, such as Castro's. Frequent
pollers whose single failures are network blips (`CodexResets`,
`ClaudeResets`, `CalendarPrimarySync`, non-gap `PetTracker` failures) alert
only after 30 minutes of consecutive failed runs through
`omni_alerts::TaskFailureGate` and `SUSTAINED_FAILURE`; the first gate that
applies to a title decides it.

`CodexResets` polls Reset Beacon's public alert feed and history every minute.
Keep predictions, announcements, observed rollouts and reported landings distinct.
Never infer a landing from an elapsed deadline or a banked grant from an ordinary
reset. Completed history can precede the alert feed; deduplicate both by source
post as well as event. Keep pushes concise with a source button and preserve
durable delivery reservations. See `docs/codex-resets.md`.

`ClaudeResets` polls Reset Radar's structured catalog for confirmed historical
Claude Code counter-reset reports. Tracker confirmation is not account
verification. Preserve banked-reset wording without inferring redemption or
expiry. The providers share bounded reads, scheduling, and delivery code in
`omni-personal`'s `reset_alerts` module, with separate durable namespaces and
unchanged Codex keys. See `docs/claude-resets.md`.

### PressPods

Retrievers run independently and the metadata model selects the best usable
article. Speech is chunked and verified before the finished episode is exposed.
Every sample-rate conversion in
`crates/omni-presspods/src/speech/audio_chain.rs` must retain `RESAMPLE_HQ`; the
Higgs denoise path retains `FIZZ_SHELF`. See `docs/presspods-audio.md` before
changing the audio chain.

Higgs denoise uses the `arnndn` filter with
`assets/press-pods/denoise.rnnn` (`DENOISE_MODEL_ASSET`); the model must remain
in the Docker image and runtime FFmpeg must include that filter.
`omni doctor --image` checks both at image build.

### Pet health watch

`PetTracker` evaluates the health rules after every sync
(`omni-personal/src/pets/health.rs`). Whisker timestamps are UTC without an
offset. Weights are robust 7-day medians that drop readings over 1 lb from the
window median (visit misattribution). The `pet-health-alert` row per pet and rule
is the only throttle: one push per episode, at most one per pet and rule per
week, reserved as `sending` before the push and never resent when uncertain.
No reading from any pet for 48 h fails the run; `PetGapAlertGate` keeps that
failure off the ERROR-alert path while the watch tracks the gap. A UI dismissal
(`pet-health-dismissal`) hides a per-pet finding until the episode clears or a
newer episode or push is recorded; it never touches the alert rows or pushes.
Pushes report numbers and never diagnose. Recalibrate against a production copy
with the `prod_copy` replay test; see `docs/pet-health.md`.

### Stylist

The stylist ledger (`omni-stylist`, `docs/stylist.md`) is the only source of
truth for fit and wardrobe facts; never seed or invent body data, purchases or
outcomes. Every tool takes a namespace: `real` is the owner's and `fixture-*`
is test data that must never be written to `real`; only fixtures can be purged
or restored through MCP. Predictions, measurements and profile snapshots are
immutable and events are append-only; corrections supersede and never rewrite.
Writes are keyed and run in one transaction with optimistic concurrency;
SQLite triggers keep rows append-only and `real` undeletable. Stored bodies
must keep decoding, so new fields are optional. Evidence stays in the 0700
private directory, re-hashed on read, deleted only by name (never by
sweeping), never served over HTTP, and MCP activity keeps only identifiers and
error codes for `stylist_*` calls. Archives never cross between `real` and
fixtures. The LAN `/stylist` page is read-only and its `GET /api/stylist`
overview never carries evidence bytes, photos or captions.
Published runbook versions are frozen by a digest test: change the playbook
by adding a version. Nothing in the stylist sends messages, shops or scans
mail; proactive help is limited to items listed in the context.

### Castro

Inbox preview cleanup (`CastroInboxCleanup`) runs every six hours
(`0 */6 * * *`). Keep failed runs visible, but gate Pushover on at least three
consecutive failures spanning twelve hours (allowing five minutes of schedule
jitter), using durable run history (`omni-podcasts/src/castro/alert_gate.rs`).
Success resets the incident; isolated HTTP 500 and socket failures must not
notify.

### Arr recovery

`ArrRecovery` requires an unchanged explicit import failure across observations
spanning 15 minutes; never bypass that gate for manual runs. Luna may interpret
names but cannot override exact target mappings or unsafe import rejections.
Reserve mutations durably and verify imports/deletions before reporting success.
Search only missing monitored targets, with durable retry limits. See
`docs/arr-recovery.md` for NZBGet health checks and shared temporary-path mounts.

### Sonarr seasons

The `media_series_*` MCP tools keep monitoring and searching separate: adding
or monitoring a season never searches unless `search: true` is passed. A new
series is added with only the selected seasons monitored and searched only by
Sonarr's own post-add search. Every season search, including the post-add
search of any series add, is reserved durably per season before the request;
within 24 hours, or while a post-add or Sonarr search is pending, a season is
reported and never searched again, and an uncertain request is never retried.
See "Sonarr seasons" in `docs/mcp.md`.

### Observer issue repair

`ObserverRepair` uses Luna to interpret reports and deterministic Arr tools to
replace scoped media or search missing files. Preserve exact TMDB/TVDB identity,
file-to-import-to-grab provenance, and the report scope ceiling. Reserve mutations
durably; never repeat uncertain deletions/searches automatically. Resolve only
after search acceptance and a verified explanatory comment, then Pushover notify.
Unsupported issues stay open and notify once. See `docs/observer-repair.md`.

### Reminders and MCP

Server iCloud Reminders is independent of Mac EventKit and IMAP/CalDAV. It stays
disabled without its complete `ICLOUD_REMINDERS_*` configuration. Its HTTPS code
page intentionally has no additional login; only bounded authentication controls
and public status are exposed there. Reminders data and CRUD remain behind MCP
bearer authentication. Preserve strict Origin checks, serialized challenges, private
encrypted session storage, durable mutation reservations, and read-after-write
verification. Never accept Apple terms or disable ADP automatically. Recurrence
rule changes require exact parent and rule tags, atomic linkage, and fresh
verification. Unknown or multiple rules stay protected. Recurring completion uses
the dedicated one-shot provider query, a durable reservation, and fresh verification
of the original and completed occurrence. Never replay an uncertain completion.
Ordinary recurring reminder edits remain blocked. List creation must preserve
the Account ordering CRDT; a List record
alone is insufficient. See `docs/server-reminders.md` and its upstream notice.

Keep MCP tools bounded adapters over existing services and preserve strong
bearer-token validation.

Claude Code session tools reach the Mac only through its outbound `omni-link`
long-poll, authenticated by `OMNI_DEVICE_LINK_TOKEN`, which must never equal or
substitute for any MCP credential. Withdraw jobs the Mac has not picked up; never
retry a delivered job whose outcome is unknown. Session starts stay limited to
the dotfiles `claude-rc` project list, enforced on the Mac. MCP tools describe
a generic "Claude Code host": never mention Mac, macOS, or the hostname in tool
text, results, or errors (the UI may). Sessions run with full access by design;
approval is the connected client's, guided by tool policy, so do not add a
permission mode. See
`docs/claude-sessions.md`.

Hister tools read a captured-page archive, not a complete browser visit log.
Keep text and listings bounded, treat page content as untrusted evidence, and
verify single-page label writes by reading them back. Never expose the upstream
token or forward it through redirects. See `docs/mcp.md` for tool semantics.

Email MCP reads batch selected UIDs and reuse bounded caches: search and direct
read snapshots last 30 seconds, parsed immutable messages five minutes. Preserve
the `fresh` bypass and mailbox-event invalidation. Draft creation discovers the
special-use mailbox and verifies APPEND by Message-ID. Drafts and sends reserve
idempotency keys durably; uncertain sends never retry automatically, and partial
recipient rejection cannot report success. Composed sends persist MIME before SMTP,
record SMTP acceptance before Sent APPEND, and keep Sent verification separate
from delivery. Uncertain APPENDs only reconcile by exact Message-ID and MIME; copy
repair never retransmits. Composed mail falls back to iCloud SMTP only when no
explicit SMTP fields are set. See `docs/mcp.md`.

All new outgoing email and drafts use `michael@thiesen.dev`, fixed server-side.
SMTP/IMAP authentication identities remain separate. Tools never accept a sender
or From option; invalid legacy `EMAIL_FROM` configuration fails boot. Preserve
historical Sent MIME during copy repair and never retransmit it to change identity.

MCP Events (`email.received`, `claude.session.turn_finished`,
`livestream.status_changed`, `presspods.job_finished`, `task.run_finished`,
`calendar.event_changed`, `calendar.event_starting`) share one outbox; each
event keeps ordinary polling tools for clients without event support. Subsystems publish only through the
`EventPublisher` port (unset when events are disabled), outside their own
transactions and locks, with a stable dedup key, a payload the catalog schema
accepts (identifiers, state and at most one 200-character title; no bodies,
notes, error text or full URLs), and a publish failure that never fails or
repeats the source work. Never hold the outbox lock across webhook or
authorization I/O. Events are always enabled and every subscription is owned by
a gateway principal (`gateway:client:<id>` or `gateway:grant:<id>`); delivery
requires the owning client or grant to still be active, and holds the events as
`withheld` while it is revoked. `refreshBefore` is the subscription's expiry.
The outbox key lives in `mcp-events.key` (data directory, 0600), migrated once
from the old `OMNI_MCP_TOKEN` derivation so existing subscriptions keep their
ids; never change it. See `docs/mcp-events.md`.

Each MCP tool's contract (name, title, description, annotations, policy, and
input/output schemas derived from `schemars::JsonSchema` types) is a `ToolDef`
in its package's `defs` module next to the handler; `omni-mcp`'s `TOOL_ORDER`
sets the serving order. `crates/omni-mcp-kit/golden/tools-list.json`, both
`mcp-policy.json` copies and the `tools/list` results in
`crates/omni-mcp/tests/golden/protocol.json` are generated snapshots: after
changing a definition run `cargo xtask mcp-golden` and review the JSON diff;
tests fail on drift. Served schemas must stay byte-identical unless the contract
change is intended.

### MCP gateway

`/gateway/mcp` (`omni-mcp/src/gateway/`, `docs/gateway.md`) is Omni's only MCP
endpoint and serves Omni's tools
plus each configured upstream integration's as `<id>__<tool>`; native tool and
event names never contain `__`. It accepts only gateway client tokens (stored as
SHA-256) and OAuth access tokens it issued, and downstream
OAuth approval happens only on the LAN-only `/gateway` page. Only `/gateway/*` and
`/.well-known/oauth-*` are public (`deploy/gateway/route.mjs`). Upstream URLs are
public HTTPS through the SSRF-guarded client with no redirects; stdio integrations
name an image-bundled server (`deploy/mcp-stdio` lockfile, no install scripts) and
run with a cleared environment. Credentials are sealed with `gateway.key`, bound
to their origin and auth kind, write-only and never logged. Results, errors and
annotations pass through unchanged; `tools/call` is retried only when it certainly
did not run. Integration events are followed by `events/poll` feeds whose cursor
is stored only after the events are queued (receipts keyed by feed and upstream
event id); a `truncated` replay is recorded as a gap, never papered over.

Private PDF attachment MCP reads bind exact Message-ID to actual MIME part IDs,
revalidate identity, and cap source and decoded bytes. Preserve read-only/PEEK
semantics, declared MIME validation, safe download names, and private binary
responses without public storage or server-selected filesystem destinations.
Outgoing send and draft attachments are only `attachmentReference` values
(`{messageId, attachmentId, sha256}`) from `email_attachment_get`, re-read through
that path, never caller bytes. The required SHA-256 must match before any APPEND
or SMTP; keep the PDF-only scope tied to that reviewed-read path. Resolve them before reserving the
idempotency key, settle known keys from their receipt first, and keep attachment
bytes in the persisted MIME so Sent copy repair never re-reads a source. Receipts
drop wire MIME once SMTP completes and the private copy once it is verified.

## Code and tests

- rustfmt with `max_width = 100` (`rustfmt.toml`); Clippy for correctness.
- Prefer strong types, enums for discriminated states, small modules, and
  explicit returns.
- Do not leave debug logs, commented-out code, or unnecessary abstractions.
- Tests are spec-style acceptance tests: integration tests in `crates/*/tests/`
  named after the behavior they cover, plus `#[cfg(test)]` unit tests for pure
  decisions. Test pure decisions separately from network adapters. Async tests
  use `#[tokio::test(start_paused = true)]` and the test clock; assert typed
  failures with `matches!`.
- Use `omni-testkit` fakes (`TestStore`, `TestApp`, `FakeModels`, recorded
  Pushover and mail, `no_network`, `mock_http` over wiremock). Tests never reach
  the network.
- Tests against a production copy are `#[ignore]` and read `OMNI_PROD_COPY`; run
  them with `cargo test -p <crate> --test prod_copy -- --ignored`. Live
  integration smoke tests are also `#[ignore]` and run manually with dotenvx.
- Committed golden fixtures are synthetic only. Capture raw responses with
  `cargo xtask capture-golden` into the gitignored `.local/golden-capture/`, then
  derive committed fixtures with `cargo xtask golden-synthesize`;
  `crates/omni-api/tests/golden_privacy.rs` rejects personal data. Never commit a
  production database or capture. The repository is public.

For ad-hoc scripts that need configured credentials, write a scratch example and
delete it afterward:

```bash
# crates/<crate>/examples/omni-<subject>.rs, removed when done
dotenvx run -- cargo run -p <crate> --example omni-<subject>
```

## Secret files

`.env` contains unrecoverable secrets.

- Never print or read secret values into the conversation. List key names with
  `cut -d= -f1 .env`; test presence with an opaque `grep -qE` check.
- Never rewrite, delete, or edit existing `.env` lines. If an existing value must
  change, ask Michael to edit it.
- New placeholders may be appended. `.env.example` is version controlled and can
  be edited normally.
- Consume secrets through `dotenvx`; do not export or interpolate them into shell
  commands.

For web requests made with curl or an equivalent raw client, set the user agent
to `OpenAI File Downloader, XaiImageApiFetch/1.0`.
Exception approved by Michael: the server Reminders Apple client retains ioBroker's
endpoint-specific Apple-compatible User-Agent and Referer headers. This exception
does not apply to other HTTP clients.
