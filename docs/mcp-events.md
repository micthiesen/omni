# MCP Events through Executor

Omni implements MCP Events at its authenticated `/mcp` endpoint. The separate
`executor-events-adapter` sidecar (the Rust crate
[`omni-events-adapter`](../crates/omni-events-adapter/src/lib.rs), deployed as described in
[`deploy/executor-events/`](../deploy/executor-events/README.md)) adds event discovery to the existing Executor connection while retaining its
OAuth authority and tool catalog. It does not modify third-party Executor core.

## Verified protocol and deployment baseline

The deployment inspected on 2026-10-01 runs Executor 1.6.10, revision
`3890d6f5e5efd1530f0dba0fe23ada95a39caf86`, image digest
`sha256:b9e001775d3eb7d662d347c8f7054333c78c1a4fd97cb5a86dc1d1d1406093b9`.
It uses MCP 1.x and rejects `server/discover`. A freshly authenticated legacy
session at `/mcp?elicitation_mode=native` listed Executor's six tools and no
`resume` tool. Previously cached connection metadata is not evidence of that
session's mode.

On 2026-10-05 an unauthenticated modern `server/discover` to
`https://mcp.syas.ca/mcp` received the adapter's own 401, while a legacy
`initialize` received Executor's, so the exact `/mcp` route is live. Executor
also serves `/<org>/mcp` and `/mcp/toolkits/<slug>`; the route does not cover
those paths, so a ChatGPT connection must use exactly `/mcp` with Executor OAuth
(the adapter does not accept Executor API keys) to discover events.

Omni serves both the legacy `2025-11-25` era and the modern `2026-07-28` era
(`crates/omni-mcp/src/rpc.rs`). The adapter translates modern MCP requests to the deployed
legacy Executor host and forwards authenticated event RPC to Omni. Neither
Executor search/invoke passthrough nor ordinary SSE notifications implement
MCP Events. The required modern protocol is `2026-07-28`.

References:

- [OpenAI MCP Events](https://developers.openai.com/plugins/build/mcp-events)
- [Executor host envelope reference](https://github.com/UsefulSoftwareCo/executor/blob/98d606bd2b47b9dcc2c03a129a14b5134d9852c8/packages/hosts/mcp/src/envelope.ts)
- [Executor API reference](https://github.com/UsefulSoftwareCo/executor/blob/98d606bd2b47b9dcc2c03a129a14b5134d9852c8/packages/core/api/src/server/executor-app.ts)
- [Executor v2 contracts](https://github.com/UsefulSoftwareCo/executor/tree/2eb2871ecaaf168928eeb77fee89bfdc8e25c614)

## Events

`events/list` serves the catalog in `crates/omni-mcp/src/events/catalog.rs`. Each event
declares its arguments, payload schema and the rule that matches a payload to a
subscription. All events share one durable outbox, signing, authorization and
retry path. No event has a protocol replay cursor; subscription responses use
`cursor: null`. Every event also has ordinary tools for clients without MCP
Events, so subscriptions only remove polling.

Arguments are string-valued. Omitted enum arguments are stored as their
defaults, so `{}` and the explicit defaults are the same subscription identity.
Payloads carry identifiers, state and at most one title of 200 characters or
fewer; bodies, notes, error text and full URLs stay behind the polling tools.

### Publishing from a subsystem

Subsystems cannot depend on `omni-mcp`, so they publish through the
`EventPublisher` port in `crates/omni-runtime/src/ports.rs`, implemented by
`crates/omni-mcp/src/events/publisher.rs` over the outbox and set in
`crates/omni/src/wiring.rs`. The port is unset when MCP Events are
disabled (`Ports::publish_event` then returns `Ok(false)`). An
`EventPublication` names a catalog event, a dedup key, the source time and the
payload:

- The implementation rejects unknown events, empty dedup keys, payloads over
  4 KiB and payloads that fail the catalog's payload schema, before anything is
  stored. Payload DTOs live in `omni_api::events`, and a test checks each one
  against its schema.
- The receipt key is `sha256("<name>:<dedupKey>")` and the event ID derives from
  `"<name>:<dedupKey>"`, so a replay of the same source observation is dropped
  and keeps its event ID.
- Publishing is `McpEventService::publish`: receipt and outbox rows commit in
  one transaction under the short state lock; webhooks and Executor checks run
  outside it, delegated tokens are validated at delivery, and `withheld` and
  `refreshBefore` behave as for every other event.
- `active_arguments(name)` returns the canonical arguments of every current
  subscription, so a source can poll only while subscribed or fan out per
  argument set.
- Callers publish outside their own store transactions and locks. A publish
  failure is logged at warn and never fails or repeats the source work, so a
  crash between the source's durable write and the publish can lose that one
  event (documented per event below).

To add an event: add the payload DTO and name to `omni_api::events`, a catalog
entry (`EventKind`, `parse_arguments`, `matches`, `input_schema`,
`payload_schema`), the event to the `events_status` description, a section
here, then run `cargo xtask mcp-golden`; the `events/list` results in
`crates/omni-mcp/tests/golden/protocol.json` are generated from the catalog.

### `email.received`

Takes an explicit `folder` argument of `inbox` or `archive`. Subscribe
separately for each desired scope. The event contains a Message-ID, originating
folder, UIDVALIDITY and UID. Read the full message with the existing `email_get`
tool. Subjects, bodies, attachments, and unsubscribe URLs do not travel in
callbacks. Messages without a Message-ID are excluded. Origin comes from the
actual IMAP fetch, not an inferred display name or attachment.

The first observed folder is retained across replay. Moving the same Message-ID
from Inbox to Archive does not create a second receipt event. Queued archive
and restore operations also reserve a suppression marker before moving mail.
This covers messages that predate event observation and prevents automation
feedback loops.

### `claude.session.turn_finished`

Fires when a Claude Code session on the Claude Code host finishes a turn or
stops. The optional `project` argument limits it to one project. The payload
holds the session ID, short ID, project, status and transcript revision; read the
reply with `claude_session_get` (`includeResult`). No prompt or reply text
travels in callbacks.

While any subscription for it is active and the host link is online, the
`ClaudeSessionEvents` task lists the host's running sessions every 15 seconds and
compares each with the last state Omni recorded. A settled session whose
revision advanced, or that was working when last seen, produces one event per
session and revision. A session seen for the first time only sets the baseline.
`claude_session_start` and `claude_session_send` record the turn they begin, so
a turn that ends before the next poll still fires. A session that stops mid-turn
leaves the running list and is read individually. Turns last seen more than a
day ago do not fire when polling resumes.

### `livestream.status_changed`

Fires on a streamer's aggregate went-live and went-offline edges, the same
edges that send live and offline Pushover notifications. A sticky primary
switch is silent, and leaving Destiny.gg's top set retires a discovered stream
without an offline event. The `liveNotifications: false` mute does not apply: a
subscription is an explicit request.

- Arguments: `streamer` (optional exact ID from `livestreams_list`; discovered
  streams look like `dgg:twitch:name`), `transition` (`any` default,
  `went_live`, `went_offline`), `includeBackground` (`false` default, `true`;
  background-tier streamers are excluded by default, matching their muted
  notifications).
- Payload: `streamerId`, `displayName`, `transition`, `tier`, `platform` (the
  session's primary), `title`, `startedAt`, `endedAt` (offline only),
  `viewerCount` (went-live only), `maxViewerCount`.
- Dedup key: `<streamerId>:<startedAt ms>:<went_live|went_offline>`.
- Source: `crates/omni-live/src/task.rs`. Went-live publishes after the live
  status is written, so a crash in between loses that event (as with
  Pushover). Went-offline publishes before the session and offline status are
  written, so a re-detected offline edge replays the same key.
- Polling: `livestreams_list`, `livestream_get`.

### `presspods.job_finished`

Fires when the PressPods worker finishes a job: the episode was published, or
the job failed permanently. Retryable failures do not fire.

- Arguments: `outcome` (`any` default, `published`, `failed`).
- Payload: `outcome`, `episodeId`, `jobId`, `title` (episode title), the
  article's hostname only (`articleUrlHost`), `durationSeconds`, and `attempts`
  (failures only).
- Dedup keys: `episode:<episodeId>`; `failed:<jobId>:<attempts>`.
- Source: `crates/omni-presspods/src/task.rs`. A published episode is announced
  before its job completes, so a crash in between recovers the episode from the
  job and replays the same key. A permanent failure publishes after the failure
  is recorded; a crash in between loses that event.
- Polling: `presspods_list` (failed jobs), `presspods_episode_get`.

### `task.run_finished`

Fires when an Omni task run finishes.

- Arguments: `task` (optional exact task name from `tasks_list`), `status`
  (`error` default; `error_or_degraded` also includes runs that skipped their
  work because an upstream failed; `any` every finished run). `any` requires
  `task`, because the live check alone finishes a run every 20 seconds.
- Payload: `runId`, `taskName`, `trigger`, `status` (`success`, `error`,
  `degraded`), `startedAt`, `finishedAt`. No error text or logs; read them with
  `task_run_get`.
- Dedup key: the run ID.
- Source: the `TaskRunEvents` task in
  `crates/omni-mcp/src/events/task_runs.rs` (no port, since `omni-mcp` owns
  it). While any subscription is active it scans run history at :15 and :45
  each minute for runs that finished in the last ten minutes and publishes
  those some subscription matches. Receipts make rescans idempotent, so no
  cursor is stored, and runs nobody matches leave no receipts. Runs that
  finished earlier (while nobody was subscribed, or before a long outage) are
  history, not news; runs a restart marks interrupted finish at boot and are
  inside the window.
- Polling: `task_runs_list`, `task_run_get`.

### `calendar.event_changed`

Fires when an event in the primary iCloud calendar is created, updated or
deleted, by any client.

- Arguments: `origin` (`external` default, `any`) and `kinds` (`all` default,
  `created`, `updated`, `deleted`). `external` skips changes whose resulting
  version Omni's calendar tools wrote (their write echoes), so an agent does
  not react to its own writes. A tool write holds the sync lock from send
  until its echo is stored, so a concurrent sync cannot tag it external.
  Email-pipeline writes bypass the echo rows and count as external.
- Payload: `eventId`, `uid`, `changeKind`, `summary` (at most 200 characters)
  with `summaryTruncated`, `start` (the next occurrence at or after detection,
  else the event's start), `allDay`, `recurring`, `changedFields` (empty for
  created and deleted), `version` (the new ETag, null for a deletion),
  `origin` (`omni`, `external`) and `detectedAt`. No notes, location,
  URL or attendees; read them with `calendar_event_get`.
- Dedup key: `eventId` plus the resulting ETag, or `deleted:` plus the last
  ETag for a deletion. A resource recreated at the same href has a new ETag
  and so a new event.
- Source: `crates/omni-calendar/src/primary/events.rs`. Each sync commits its
  change rows (`calendar-primary-change`) first; the pass that follows hands
  rows after `published_seq` on the sync state to the port and then advances
  it, so a crash replays the same keys. A transient port failure keeps the
  cursor for the next sync; a rejected payload is skipped. Without a
  subscriber (or with MCP Events disabled) the cursor advances without
  publishing, so subscribing never delivers history; rows older than a day
  are never published. The first sync of a collection is a silent baseline,
  and ETag churn without a semantic change records no change at all.
- Cadence: `CalendarPrimarySync` syncs every minute while any `calendar.*`
  subscription is active (every five minutes otherwise); reads that sync
  publish too.
- Polling: `calendar_changes_list` (same `origin` filter and default).

### `calendar.event_starting`

Fires when an occurrence in the primary calendar is about to start or one of
its alerts is due.

- Arguments: `trigger` (`start` default, `alarm`), `leadMinutes` (`start`
  only: `0`, `5`, `10`, `15` default, `30`, `60`, `120`, `1440`; rejected with
  `alarm`) and `includeAllDay` (`false` default, `true`; an all-day
  occurrence starts at local midnight in the default zone).
- Payload: `eventId`, `uid`, `recurrenceId` (null for a single event),
  `summary` with `summaryTruncated`, `start`, `end`, `allDay`, `timeZone`,
  `trigger`, `leadMinutes` (null for alarms), `alarmId` (null for starts),
  `fireAt`, `late`, `hasLocation`, `includeAllDay`. `leadMinutes` and
  `includeAllDay` echo the subscription tuple that produced the publication;
  matching requires the exact tuple, so each subscription receives only its
  own publications.
- Dedup key: UID, recurrence ID (or `single`), trigger with lead or alarm ID,
  `includeAllDay` and the occurrence's UTC start. An unchanged occurrence
  fires once; a rescheduled one fires again for its new time.
- Source: the `CalendarStartingEvents` task
  (`crates/omni-calendar/src/primary/starting.rs`) every 30 seconds. Without
  an active subscription it returns after one lookup. Otherwise it refreshes
  the mirror when older than a minute, expands it around now and publishes
  once per distinct subscription tuple:
  - `start` fires at `start - leadMinutes`. A fire time missed by more than
    two minutes (Omni was down, or the event was created inside the lead)
    still fires with `late: true` while the occurrence has not started.
  - `alarm` fires at each VALARM's trigger, at most ten minutes late.
    `ACKNOWLEDGED` at or after the fire time (dismissed on a device)
    suppresses it, `ACTION:NONE` never fires, and absolute triggers fire only
    for single events and overrides.
  - Cancelled occurrences never fire; EXDATEs are not expanded.
- Polling: `calendar_events_list` with `from` now and `to` now plus the lead;
  `calendar_event_get` reports alarms.

## Other servers' events

The adapter also offers the events of other MCP servers that implement MCP
Events themselves, configured in `EVENT_UPSTREAMS` (a JSON array of
`{key, url, token}` in the adapter's `deployment.env`). Today that is
artifacts.boo: `{"key": "michael", "url": "https://michael.artifacts.boo/mcp",
"token": "<agent token>"}` and the same for `support`.

- `events/list` returns Omni's events, then each upstream's (fetched at most
  every five minutes; an unreachable upstream is left out, never failing the
  list). An event several upstreams offer is listed once, with a `workspace`
  argument naming them (required when there is more than one). Omni's own
  events win a name clash.
- `events/subscribe` and `events/unsubscribe` for an upstream's event go to that
  upstream with its token, without `workspace` or the caller's `_meta`; the
  answer is returned as is. The upstream then checks the callback with its own
  challenge, signs deliveries (Standard Webhooks) and retries them, and keeps
  its own cursors, so none of Omni's outbox, receipts or withheld-token logic
  applies. Unsubscribe identity is the upstream's: name, arguments and callback
  URL.
- The caller is still authenticated as the pinned Executor owner before
  anything reaches an upstream. The upstream token acts as its owner there (for
  artifacts.boo, an operator's agent token: it sees what that person can open),
  so only configure tokens of the same person who owns the Executor.
- Nothing polls an upstream: it is called only when a client lists or
  subscribes.

Tokens for artifacts.boo come from a workspace's settings (Agent tokens), or
from its operator API (`POST /api/admin/workspaces/<slug>/tokens` with
`{"email": "<operator>", "label": "Events adapter"}`).

## Delivery

Publishing writes the receipt and outbox rows in one transaction, then wakes the
delivery worker, so a subscriber normally hears about new mail (or any other
published event) seconds after the source observes it. For email this happens before the IMAP cursor commits.
Boot recovery and a scheduled 30-second sweep retry pending work without
changing event IDs. Outbox state changes hold one short lock; webhook calls and
Executor authorization checks run outside it, so a slow callback never delays
email dispatch.

Finished deliveries are kept for seven days, receipts for 30 days (beyond the
IMAP seven-day INTERNALDATE guard, so replays stay deduplicated) and
subscriptions for seven days after they expire.

## Subscription and callback security

Subscription identity binds the authenticated owner, canonical arguments,
event name and callback URL. Refresh uses the same identity. Unsubscribe is
idempotent and scoped to that owner. Expired or revoked subscriptions do not
receive mail.

The adapter pins the configured Executor owner and validates the current OAuth
session. Omni encrypts forwarded OAuth credentials at rest and checks them
again through the fixed internal `OMNI_EVENTS_EXECUTOR_AUTH_URL` before delivery.
No user-supplied authorization URL is accepted. The storage encryption key is
derived from `OMNI_MCP_TOKEN` for this purpose. Rotating that token invalidates
old event credentials; resubscribe after rotation.

Executor 1.6.10 locks Better Auth 1.6.22, which issues one-hour MCP access
tokens and whose session check returns `null` once a token expires. Omni never
delivers without a stored token that validates at delivery time, checked once per
subscription per delivery pass. When it does not validate, all of that
subscription's pending events stay `withheld` and are rechecked after 15 minutes;
when Executor cannot answer, after one minute. Other subscriptions keep
delivering. A subscription refresh that stores a validating token releases
withheld events immediately. Withheld events fail only when their subscription
expires, is removed or is replaced by a new generation.

So that a valid token is normally present, a delegated subscription's
`refreshBefore` is the earlier of its granted lifetime and one minute before the
expiry of the token it was created or refreshed with, never earlier than the
request and never later than that expiry. Each refresh revalidates the client's
current token through the adapter. The stored subscription lifetime (24 hours by
default, at most seven days) is unchanged, so an event that arrives after the
token expires and before a late refresh stays withheld rather than failing or
being delivered.

Callback signing uses the Standard Webhooks HMAC format over the stable delivery
ID, fresh Unix signing timestamp, and exact JSON body bytes. Secrets must decode
from `whsec_` base64 to 24–64 bytes. Secret rotation briefly signs with both keys.
Callback verification uses a fresh random challenge and constant-time echo
comparison before any application data is sent.

Callbacks must use HTTPS without URL credentials or fragments. DNS is checked
at socket lookup, including all returned addresses; the connection uses those
validated addresses and retains normal hostname/TLS verification. Private,
loopback, link-local, special-purpose and non-global IPv6 ranges are blocked.
Redirects and automatic HTTP retries are disabled. Requests time out after ten
seconds, responses are bounded to 4 KiB, and event bodies to 256 KiB. Durable
outbox retry policy owns retries; HTTP 410 and 413 are terminal.

## Diagnostics

The read-only `events_status` MCP tool reports what Omni has observed for every
event: the
30 most recent `events/list`, `events/subscribe` and `events/unsubscribe`
requests with outcomes, up to 20 subscriptions with event, arguments, state, callback hostname and
advertised `refreshBefore`,
delivery counts including `withheld`, and the ten most recent deliveries with
HTTP status, transport error, failure or `withheld` reason. Owners appear as a 12-character hash prefix. It
never returns secrets, bearer tokens, callback paths or message content.
`server/discover` and `tools/list` are answered by the adapter and do not appear
here; an `events/list` row is the first Omni-side evidence of discovery. Requests
rejected by parameter validation or an invalid delegated-owner header fail before
logging, so a missing `events/subscribe` row does not prove ChatGPT never called.

## Connected verification still required

A healthy deployment and successful raw MCP calls do not prove an idle dot wakes.
The parent must use the existing Executor connection to:

1. Open the existing Executor connection in ChatGPT Plugins and select **Refresh**.
   Confirm `email.received` is listed alongside the Executor tools, then start a
   new Work conversation. This is the documented developer-mode
   [metadata refresh procedure](https://developers.openai.com/plugins/deploy/connect-chatgpt#refresh-metadata).
   Refreshing Omni's tool inventory inside Executor does not refresh ChatGPT's
   native event-source catalog. Published plugins follow continuous review.
   Confirm `events_status` shows a new `events/list` request; if it does
   not, ChatGPT did not reach the adapter with MCP 2026-07-28 discovery.
2. Create an explicit Inbox or Archive subscription in the intended Work chat
   or dot. Confirm `events/subscribe`, successful challenge verification and
   durable subscription acceptance (`accepted` or `refreshed` in
   `events_status`). Over the next hours, confirm `refreshed` rows arrive
   about hourly with an advancing `refreshBefore`; repeated refreshes without
   progress mean ChatGPT is reusing an unrotated token.
3. Select a harmless test message or send a new test message to the chosen
   folder. Confirm a matching outbox delivery receives HTTP 2xx and the intended
   chat receives the identifiers. Fetch content with `email_get` if needed.
4. Confirm a message outside the selected folder does not trigger it. Repeat a
   source observation and confirm it does not create another event ID.
5. Stop monitoring and confirm `events/unsubscribe` stops subsequent delivery.

For the port-published events, after the Refresh in step 1 confirm all seven
events are listed, then subscribe and observe one harmless delivery each:

- `task.run_finished` with `{"task": "TaskRunEvents", "status": "any"}` fires
  within a minute of subscribing; unsubscribe afterwards.
- `livestream.status_changed` for a primary-tier streamer fires on the next
  real go-live or offline edge.
- `presspods.job_finished` fires after submitting a disposable article with
  `presspods_submit` (or on the next real job).
- `calendar.event_changed` with `{}` fires within about a minute after editing
  a disposable `[omni-test]` event on a device; a tool-written change fires
  only with `origin: any`.
- `calendar.event_starting` with `{"leadMinutes": "5"}` fires about five
  minutes before a disposable `[omni-test]` event created for that purpose.
  Delete the test event by its exact eventId afterwards.

Native automation setup belongs to the parent. Do not archive real mail to test
this integration without selecting and authorizing the exact test messages.

On 2026-10-01 the public authenticated protocol checks passed: modern discovery,
event definitions, the six native Executor tools, legacy client tools, OAuth
challenge preservation, and callback rejection before unsafe network access.
ChatGPT still listed only GitHub as a native event source; its connection refresh
was not completed. No native subscription or idle-dot delivery is proven. Keep
the parent's hourly fallback until that complete lifecycle succeeds.
