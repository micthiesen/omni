# YouTube

`omni-youtube` connects the owner's YouTube account through the YouTube Data
API v3 with Omni's own Google OAuth client, and serves twelve `youtube_*` MCP
tools: playlist listing and management, public search, and video and channel
details. It replaced Composio for this, whose shared Google project ran out of
quota even for plain reads. The live check's scraper is unrelated
([youtube-data-api.md](youtube-data-api.md) explains why it does not use the API).

## What the API cannot do

The YouTube Data API does not serve Watch Later or watch history (both
removed in 2016; the tools refuse `WL` and `HL`), the personalized Home feed or
recommendations, or caption text and transcripts of other channels' videos.
`youtube_videos_get` reports only whether captions exist. Tool text never
implies any of these.

## Google Cloud setup

- Project `omni-513406`, with **YouTube Data API v3** enabled.
- OAuth client of type **Web application**. Authorized redirect URI:
  `https://omni.syas.ca/api/youtube/oauth/callback` (`YOUTUBE_REDIRECT_URI`).
  `omni.syas.ca` is the LAN-only HTTPS host ([server-reminders.md](server-reminders.md),
  "Boris routing"); only the owner's browser has to reach it, so nothing is
  public. No JavaScript origins.
- Scope: only `https://www.googleapis.com/auth/youtube`, with
  `access_type=offline` and `prompt=consent` (Google issues a refresh token only
  on a consent screen), PKCE S256 and a single-use ten-minute state.
- Consent screen: External, published **In production**. In Testing, Google
  expires refresh tokens for this scope after seven days. An unverified
  production app works for the owner's account behind Google's "unverified
  app" warning (limit 100 users); verification is not needed.

### Why this scope, and what it grants

There is no playlist-only scope. `playlists.insert`, `playlists.update`,
`playlists.delete`, `playlistItems.insert`, `playlistItems.update` and
`playlistItems.delete` each accept only `youtubepartner`, `youtube` or
`youtube.force-ssl`. `youtube.readonly` cannot write. `youtube.force-ssl` is
accepted everywhere `youtube` is and also by comment and caption writes, and
`youtubepartner` is for content owners, so `youtube` is the narrowest that
works.

Google describes it as "Manage your YouTube account": the grant is broad. A
holder of the token could also upload or delete videos, change subscriptions
and ratings, and edit channel settings. Omni's tools use it only for the calls
listed below, and only the owner's consent creates it.

## Connecting

1. `YOUTUBE_CLIENT_ID` and `YOUTUBE_CLIENT_SECRET` are set in Omni's
   environment (production: `/home/michael/compose/.env`, passed through the
   `omni` service's `environment`).
2. Open `https://omni.syas.ca/api/youtube/oauth/start` on the home network
   (`youtube_status` shows the link). It redirects to Google's consent screen
   only for a direct navigation (`Sec-Fetch-Site` none or same-origin), from
   the callback's host (another host such as `omni.boris` is sent there first),
   and sets an HttpOnly, Secure, SameSite=Lax cookie holding the state. The
   callback requires that cookie, so a code from someone else's sign-in cannot
   connect their Google account in this browser.
3. The callback consumes the state, exchanges the code with the PKCE verifier,
   checks that the `youtube` scope was granted, reads the channel
   (`channels.list mine=true`) and stores the account.

The refresh token is sealed with AES-256-GCM under `youtube.key` (0600, in the
data directory beside the database, created on first boot) with the row as
associated data, in the `youtube-account` row; access tokens stay in memory and
are refreshed a minute before they expire, one refresh at a time. Nothing logs
a token, code or secret. A 401 refreshes once and retries (the request
certainly did not run).

When Google rejects the refresh token (`invalid_grant`: revoked, unused for six
months, a password change, a rotated client secret, or issued in Testing), the
account is marked `reconnect_required` with the reason, no further refresh is
tried, and every tool error names the reconnect link. Connecting again replaces
the row. To disconnect, remove Omni at https://myaccount.google.com/connections.

## Quota

The project's default quota is 10,000 units per day, resetting at midnight
Pacific. Reads cost 1, writes 50, `search.list` 100. Omni records an estimate
per Pacific day and method (`youtube-quota`, kept eight days) for every request
that got a response, and `youtube_status` shows it. A `quotaExceeded` answer
becomes a `[quota_exceeded]` error with the day's estimate and reset time;
other apps using the same project also count against the real quota, which
only the Cloud console shows. A disabled API is `[api_disabled]`, rate limits
`[rate_limited]`.

## Writes

Mutations are serialized, recommended `require_approval`, honor
`SideEffectMode::Record` (recorded, not sent; the OAuth token calls still run),
and change only playlists whose `channelId` is the connected channel.

- **Create** requires an `idempotencyKey`, reserved (`youtube-write`, 30 days)
  before the insert. The same key returns the playlist it created; a reused key
  with a different request is refused. A title the channel already uses
  (ignoring case) is refused unless `allowDuplicateTitle`. A create whose answer
  was lost is settled by listing the channel's playlists for that title created
  since the reservation, never by resending.
- **Update** reads the playlist and re-sends the fields it does not change,
  because `playlists.update` clears omitted snippet fields; nothing is sent when
  nothing changes.
- **Delete** requires `confirmTitle` equal to the current title.
- **Add** skips videos already in the playlist (`already_present`), checks ids
  with `videos.list` first, and stops the batch on quota or network failures.
- **Remove** checks that each entry belongs to the named playlist.
- **Move** works only on manually sorted playlists (`manualSortRequired`).

A write whose answer was lost (timeout, network, 5xx) is checked by reading
YouTube back and reported `uncertain` when the read does not confirm it; it is
never resent.
