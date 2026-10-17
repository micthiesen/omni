# Castro sync: integration notes

Status: **authenticated client implemented and live-read verified**.
`crates/omni-podcasts/src/castro/client.rs` reads subscriptions, the ordered queue, and
180 days of playback history. It can enqueue a resolvable episode at the front
or back of the queue, search podcasts and episodes, resolve direct RSS feeds,
and subscribe to arbitrary shows. Podcast recommendations use this resolver to
auto-enqueue episodes from shows that are not already subscribed.

This document separates directly observed behavior from inference. Typed serde models
for captured payloads live in `crates/omni-podcasts/src/castro/protocol.rs`; HMAC
signing primitives live in `crates/omni-podcasts/src/castro/auth.rs`, and the
signed request layer in `castro/api.rs`.

## What we know (as of July 2026)

- Castro (Bluck Apps) shipped iPad support + cross-device sync in August 2025.
  Sync covers listening progress, playback history, episode states, queue,
  folders, and podcast settings. Sideloads and appearance prefs do not sync.
- The sync engine is **event-driven CRDT**: clients exchange events through
  Castro's servers rather than polling full state. Queue and episode states are
  stored **on Castro's servers**, linked to subscriptions.
- Auth: there is **no user-visible account or password**. Each installation
  gets auto-created credentials on Castro's server, stored in the user's
  **iCloud Keychain** (this is how a second device joins the same account).
- There is **no public API** and no official third-party integration surface.

Sources:
- https://castro.fm/blog/device-sync-and-ipad (sync architecture + keychain credentials)
- https://support.supertop.co/ (Castro support site)

## Implications

- A server API **exists** (the app speaks HTTPS to Castro's backend); it is just
  private and device-credential-authenticated. Read/write is plausible if we
  can obtain one installation's credentials and mimic its requests.
- Because sync is CRDT-event-shaped, "fetch listen history" may not be a single
  REST GET. The client implementation may need to ingest the event stream and
  maintain a local replica, then answer the snapshot-shaped reads of
  the `PodcastAccount` trait from that replica. Callers only ever see snapshots.

## Captured private API protocol

Captured July 16, 2026 from Castro iOS build 2396 with Proxyman, using the
owner's own Castro Plus account. Raw HAR files are intentionally not committed:
they contain a live access ID, request signatures, subscription identifiers,
and personal podcast activity.

### Host and common headers

All observed private API requests use `https://tentacles.castro.fm` and include:

| Header | Observed value or role |
|---|---|
| `Accept` | `application/vnd.tentacles.supertop.co+json; version=8` |
| `Content-Type` | `application/json`, including bodyless GETs |
| `X-Tentacles-App` | `castro-ios` |
| `X-Tentacles-Platform` | `iOS` |
| `User-Agent` | `Castro/2396 CFNetwork/3890.100.1 Darwin/27.0.0` |
| `Date` | RFC 7231 HTTP date, used by request signing |
| `X-Authorization-Content-SHA256` | Base64 SHA-256 of the exact request body; bodyless requests hash the empty string to `47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=` |
| `Authorization` | `APIAuth-HMAC-SHA256 <access-id>:<base64-signature>` |

The access ID is a UUID. The 44-character signature is a Base64-encoded
HMAC-SHA256 digest. The scheme matches the `api-auth` canonical form:

```text
METHOD,CONTENT_TYPE,CONTENT_SHA256,PATH_AND_QUERY,DATE
```

The canonical form was validated byte-for-byte against a captured request. The
full path **including the query string** is signed. The keychain secret is used
as its stored UTF-8 text, not Base64-decoded first. A fresh signed
`GET /profile/sync/status` made outside Proxyman returned 200, proving the
credential is sufficient on its own.

### Read and sync endpoints

| Method and path | Observed response |
|---|---|
| `GET /ping` | 200, empty body |
| `GET /profile/sync/status` | `{ device_status: number, account_status: number, latest_event_id: number }` |
| `GET /profile/events?since=<id>&limit=<n>` | `{ events: unknown[], latest_event_id: number }`; captured at the current cursor with an empty array |
| `GET /profile/sync/user_events?since=<id>&limit=<n>` | `{ user_events: unknown[], latest_event_id: number }`; captured at the current cursor with an empty array |
| `GET /profile/sync/podcast_state?…` | Podcast UUID plus per-episode new/starred/played/progress/last-played state |
| `GET /profile/subscriptions` | Current subscriptions as `{ podcast_id, private, will_notify_device }[]` |
| `GET /profile/sync/queue` | `{ queue_items: [{ fractional_position, episode_id, podcast_id }] }` |
| `GET /profile/sync/podcast_settings/<podcast-id>` | 404 for a show without custom settings |
| `GET /podcasts/<podcast-id>` | Public podcast metadata plus its episode records |
| `GET /episodes/<episode-id>` | Full metadata for one episode, including RSS GUID and duration |
| `GET /search?search_term=<query>` | Up to 25 podcast results with RSS URL, iTunes ID, and Castro `tentacles_id`; a direct RSS URL resolves to one exact result |
| `GET /episode_search?search_term=<query>` | Up to 25 episode results with episode `tentacles_id`, podcast name, artwork, and publication time |
| `GET /podcast_notes/<podcast-id>` | `{ notes: string }` |
| `GET /transcripts/<episode-id>` | 404 for both captured episodes |
| `GET /download/<short-id>` | 302 to episode media |

`GET /podcasts/<podcast-id>` uses the same UUID that subscription mutations
call `feed_id`. Its podcast object contains:

```text
public_id, short_id, title, sort_title, site_url, description, author_name,
artwork_url, last_event_number, podcast_type, itunes_category,
itunes_subcategory, private, funding_text, funding_url, episodes, people
```

Captured episode objects contain:

```text
guid, public_id, short_id, title, media_size, media_url, artwork_url,
author_name, link_url, duration.seconds, description, published_at,
predecessor_public_id, season_number, episode_number, episode_type, people
```

Castro `public_id` is the native episode ID. **`guid` is NOT reliably the RSS
`<guid>`.** For feeds hosted on platforms that rewrite guids (Simplecast,
Megaphone), Castro's stored `guid` equals its own `public_id` and differs from
the feed's `<guid>` entirely. Verified against Radiolab (Simplecast): for the
episode "The Builders", `castro.guid = 689112f0-…` (== `public_id`) while the
feed's `<guid> = a9adfa38-…`; none of the feed's recent guids appeared among
Castro's 661 stored episodes. The reliable cross-system episode key is the
**enclosure/media URL**: `castro.media_url` equals the RSS `<enclosure url>`
(both carry the same `awEpisodeId`). Episode matching in `client.rs`
(`match_episode`) therefore tries guid, then normalized media URL (host+path,
query stripped), then a unique title match.

Live probing after authentication found populated `podcast_state` responses.
Each `episode_states` item has:

```text
episode_id, is_new, is_starred, is_played, last_played, progress_seconds
```

`last_played` is a nullable ISO timestamp. The client fetches current
subscriptions, loads each podcast's state, keeps activity from the last 180
days, and resolves the retained episode IDs through `/episodes/<id>`. It then
computes completion from `progress_seconds / duration.seconds`, using 1 for an
explicitly played episode. Metadata lookups are cached for the lifetime of the
service and reads are capped at eight concurrent requests. This live history is
currently scoped to podcasts that remain subscribed. The backup proves older,
unsubscribed history exists locally, but the live API has not exposed a route
that lists every historical podcast ID.

### Action write endpoint

User and policy changes are batched to:

```http
POST /profile/sync/actions
Content-Type: application/json

{ "actions": [ ... ] }
```

Each observed action contains:

```text
id                 monotonically increasing device-local integer
episode_id         Castro episode UUID
origin_event_id    newly generated UUID per action
origin_timestamp   epoch milliseconds
source             "user" or "policy"
action_type        event name
event_data         optional action-specific JSON encoded as a string
```

The endpoint returned 200 with an empty body for every captured batch.
The reversible live write also confirmed that a process-local monotonic action
ID seeded from epoch milliseconds is accepted; `origin_event_id` remains the
globally unique idempotency identity.

Captured user flows:

| UI operation | Actions, in order |
|---|---|
| Queue Next | `episode_queued` with `event_data={"fractional_position":"ZME"}`, then `clear_episode_new` |
| Queue Last | `episode_queued` with `event_data={"fractional_position":"aE"}`, then `clear_episode_new` |
| Queue and begin playback | `episode_queued` with position `ZM`, then `episode_last_played` with epoch-seconds `last_played` |
| Pause after about four seconds | `episode_progress` with floating-point `seconds` (`3.8196825396825398` captured) |
| Clear from queue | `episode_dequeued`, then `clear_episode_new` |

The captured position strings are examples, not constants. They are
fractional-ordering keys computed relative to the existing queue. A client must
first reconstruct the ordered queue, then generate a key before the first item
for `next` or after the final item for `last`. `EnqueueEpisodeRequest.position`
exposes these two choices through `PodcastQueuePosition`. `castro/fractional.rs`
ports rocicorp `fractional-indexing` 4.0.0 `generateKeyBetween`, checked against
node-generated golden vectors in `crates/omni-podcasts/tests/golden/`. A live reversible smoke test added an
already-played episode at Queue Next, confirmed it became the first item, and
dequeued it again, restoring the original 29-item queue.

### Search and RSS resolution

Podcast and episode search use the same signed request flow as all other reads:

```http
GET /search?search_term=<RFC-3986-encoded query>
GET /episode_search?search_term=<RFC-3986-encoded query>
```

Podcast results contain `tentacles_id`, `feed_url`, `itunes_id`, title, author,
summary, artwork variants, and result position. Searching with a complete RSS
URL returned one result whose `feed_url` matched exactly. The client resolves a
write target only when the normalized feed URL or supplied iTunes ID matches,
then fetches `/podcasts/<tentacles-id>` to map the RSS episode GUID to Castro's
episode UUID.

General podcast and episode searches are exposed through
`CastroApi::search_podcasts` and `search_episodes`. Live verification
returned 25 results from each endpoint. Subscription reads also use podcast
search to enrich identities when `tentacles_id` matches; 33 of the current 39
subscriptions resolved to exact RSS and iTunes identities, while the remaining
six safely retain title-only identity.

### Subscription mutations

Subscribe and unsubscribe both accept a Castro podcast/feed UUID, not an RSS
URL directly:

```http
POST /profile/subscriptions/subscribe
POST /profile/subscriptions/unsubscribe
Content-Type: application/json

{ "feed_ids": ["<podcast-public-id>"] }
```

Subscribe returned 201 with:

```json
{
  "subscribed": [{ "feed_id": "<uuid>", "feed_url": "https://…" }],
  "latest_event_id": 123
}
```

Unsubscribe returned 200 with an empty body. Subscribing also produced an
`episode_new` action with `source: "policy"` for the inbox episode selected by
the show's policy. Unsubscribing produced `clear_episode_new` with
`source: "user"` for that episode.

### Other captured request

On launch, Castro called `POST /unlock/subscription` with App Store
`transaction_id` and `original_transaction_id` query parameters. This appears
to refresh Castro Plus entitlement and is not required for the account-client
contract. Never log or commit those identifiers.

## Remaining protocol unknown

Capture populated incremental `events` and `user_events` payloads. They are
no longer required for account snapshots because dedicated live read routes
were discovered, but documenting them would complete the protocol picture.

## Credential extraction and durability

The credential was recovered legitimately from the owner's encrypted Finder
backup after the iPhone was connected and trusted. The following files were
inspected in a mode-600 temporary directory and are not part of the repository:

```text
Castro.sqlite
sync_ledger.db
Castro-preferences.plist
keychain-backup.plist
```

The matching keychain records are in access group
`6N27F5GP4R.co.supertop.castro-keychain` with accounts:

```text
castro.account.device.deviceId
castro.account.device.secretKey
```

The access ID is a UUID and the secret is an 88-character string. Neither value
is documented or committed. They are stored as `CASTRO_ACCESS_ID` and
`CASTRO_SECRET_KEY` in the ignored local `.env`, whose permissions were changed
to 0600. Routine requests do not need Proxyman, the iPhone, iCloud access, or a
new Finder backup. Re-extraction is only expected if Castro revokes or rotates
the device credential, the account is reset, or the keychain item is deleted.

### The credential must belong to a sync-enabled, dedicated device (learned the hard way)

- **Writes require an active sync session.** With sync disabled for the
  credential's device, reads keep working (auth is still valid) but every
  `/profile/sync/actions` write returns `400` with body `Sync is disabled`. The
  server body is the only tell, and it surfaces in `CastroApi` error messages.
- **A write from a device that is not a live sync peer does not propagate.**
  It updates the server's queue projection (so our own `fetch_queue` sees it) but
  emits no sync event, so no other device ever renders it. Symptom: a queued
  episode is provably on the server yet never appears in the app, even after a
  full resync on multiple devices.
- **Toggling Castro sync off/on re-keys the device.** It disables the session
  the current credential belongs to and mints a fresh keychain credential for
  the app, orphaning ours (reads still work, writes say `Sync is disabled`).
  Do not toggle sync on the device whose credential omni uses.
- **Use a dedicated device.** The credential in use is from a spare iPad that
  the owner does not otherwise open. That device is a first-class sync peer, so
  its writes broadcast to the iPhone; and because nothing else drives it, its
  sync is never toggled. Enrolling omni as its own device is the durable
  setup; impersonating an actively-used phone invites exactly the re-key above.

### Backup database inventory

The one-time backup provided a useful cross-check of the server snapshot:

| Item | Captured count |
|---|---:|
| `SUPPodcast` rows | 178 |
| Currently subscribed (`subscribedState = 0`) | 40 |
| `SUPEpisode` rows | 55,611 |
| `SUPEpisodeQueuedState` rows | 29 |
| `SUPEpisodePlayedState` rows | 1,138 |
| Episodes with `lastPlayed > 0` | 1,655 |
| Episodes with `progress > 0` | 670 |
| Starred episodes | 2 |
| `SUPPlaySession` rows | 4,878 |

The live API subsequently returned 39 subscriptions and the same 29 queue
items. The one-show difference is consistent with the subscribe/unsubscribe
capture occurring after the backup. Relevant local mappings are:

- `SUPPodcast.publicId` is Castro's podcast UUID.
- `SUPEpisode.publicId` is Castro's episode UUID.
- `SUPEpisodeQueuedState.fractionalPosition` is the queue ordering key.
- `SUPEpisode.progress`, `lastPlayed`, and `starred` hold current playback state.
- `SUPPlaySession` records session-level played-from/to ranges and timestamps.
- `sync_ledger.db.sync_action_item` is the pending outbound action ledger.

Watch for credentials rotating when sync re-keys, app updates changing the
schema, and rate limiting. Transport failures are returned as `unavailable`,
never as empty state, per the project's three-state rule.

## Inbox cleanup every six hours

`CastroInboxCleanup` runs at `0 */6 * * *` (`castro/cleanup.rs`) whenever the
Castro credentials are configured. It reads each subscribed podcast's `podcast_state`, resolves only
episodes where `is_new` is true, and clears an episode only when its description
begins exactly with:

```text
This is a free preview
```

The comparison is case-sensitive and does not match the phrase later in a
description. Cleanup posts only `clear_episode_new`; it does not post
`episode_dequeued`, so an episode already in the Queue remains there. Normal
runs use task logs only and send no notification. Failed runs stay visible in
run history, but Pushover fires only after at least three consecutive failures
spanning twelve hours, allowing five minutes of schedule jitter
(`castro/alert_gate.rs`). A success resets the incident.

## Remaining fallbacks

| Capability | Fallback |
|---|---|
| Subscriptions | Recommendation run aborts when a configured live account read is unavailable |
| Listen history | Explicit feedback remains available if the account read is unavailable |
| Enqueue episode | Deep link remains the fallback when the show/GUID cannot be resolved |
| Subscribe to show | Returns `not_found` unless Castro search exactly resolves feed URL or iTunes ID |
