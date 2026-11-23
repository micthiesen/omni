# Kick stream relay

`/streamers/:id/watch` plays a streamer's Kick channel at its top quality
through Omni. Kick's own players adapt quality to measured throughput and drop
resolution on any hiccup; the relay pins the top variant and hides Kick's
stalls, token refreshes and playlist errors behind a cushion. The code is
`crates/omni-live/src/relay/`.

## How it works

1. Opening the watch page sends `POST /api/streamers/:id/relay`, which starts
   the channel's relay task unless one runs (a mutation, so Omni's same-origin
   guard keeps other sites from starting relays). The page then polls
   `GET` on the same path every 2 s; a `GET` never starts a relay and reports
   `stopped` when none runs. Play starts it again when it has stopped.
2. The task looks up `https://kick.com/api/v2/channels/<slug>/livestream`
   (Kick's web API; the public v1 API has no playback URL), fetches the signed
   AWS IVS master playlist and picks the variant with the largest picture,
   then frame rate, then bandwidth: `1080p60` (Kick's `chunked` source
   rendition, about 9 Mbps) on a typical channel.
3. It polls that variant's media playlist every second and downloads each new
   2 s MPEG-TS segment into memory unchanged. Nothing is transcoded or
   remuxed, so CPU use is that of an HTTP client.
4. Segments are held back and released into Omni's own playlist
   (`/api/streamers/:id/relay/<generation>/index.m3u8`) at real-time pace by
   `buffer::RelayBuffer`. Safari plays that playlist with native HLS.

## Buffer

- **Cushion 30 s** of content is held back from the player. While it lasts,
  the served playlist keeps advancing even if Kick sends nothing. Kick's own
  playlist window is about 28 s, so a stall shorter than that loses nothing:
  the relay catches up from the window when Kick answers again.
- **Startup release 18 s** (three 6 s target durations) is published at once
  when playback starts, and the playlist carries
  `#EXT-X-START:TIME-OFFSET=-18`, so the player joins 18 s back from the edge
  and keeps that much buffer of its own (Safari honors the tag; a player that
  ignores it joins nearer the edge with a smaller buffer of its own). Startup
  therefore needs 48 s of content: Kick's window gives about 28 s at once, and
  the remainder arrives in real time, so the first play is ready about 20 s
  after the page opens.
- **Delay**: Kick's latency (a few seconds) plus the cushion plus the player's
  18 s, about 50 s behind live. Stall tolerance is the same 48 s.
- If the cushion runs dry anyway, the relay counts a stall and waits until 15 s
  is held before releasing again. Cushion above 42 s is released early, so
  delay cannot creep up.
- Released segments stay 36 s for players still fetching them. Memory is
  capped at 160 MiB per relay (about 75 MiB at 9 Mbps); the oldest segments
  go first. At most two channels relay at once.

Memory rather than a temp directory: the buffer is small, is freed the moment
the relay drops, never leaves files behind after a crash, and costs no disk
writes. Boris has 64 GB of RAM.

## Refreshes and restarts

- The master URL's token expires ten minutes after issue; the media playlist
  outlives it. The relay re-resolves every 7 minutes anyway, and at once on a
  401, 403, 404 or 410 from the playlist or a segment, or on `#EXT-X-ENDLIST`.
  Kick keeps sequence numbers across re-resolves of one broadcast, so the
  relay continues from the last sequence it pulled with no gap.
- A failed lookup keeps the playlist already in hand and retries with backoff
  (0.5 s doubling to 15 s). Repeated 403s right after a re-resolve back off the
  same way.
- A new Kick livestream id (the streamer reconnected), a sequence that drops
  by more than 30, a gap, or Kick's own `#EXT-X-DISCONTINUITY` becomes an
  `#EXT-X-DISCONTINUITY` in the served playlist, with
  `#EXT-X-DISCONTINUITY-SEQUENCE` kept as segments age out.
- Kick reporting the channel offline shows the offline state; the relay keeps
  checking every 15 s while the page is open and continues with a
  discontinuity when the stream returns.

## Lifetime

Player fetches (playlist or segment) keep a relay alive. Status polls keep it
alive only during startup and, before any player fetch, for a five-minute
prebuffer while the viewer has not pressed Play. So an open page whose player
has stopped fetching does not keep Kick traffic going. 90 s after the last
fetch the task stops, drops its buffer and leaves the registry, and the page
shows "The relay stopped. Press Play to start it again." A new relay has a new
generation; the page loads the new playlist when the URL changes. After a
player error it never reloads the same playlist on its own: Play retries.

The pull loop never spins: a refused playlist or segment re-resolves once at
once and backs off on repeats, a segment still refused after a fresh resolve
is skipped with a discontinuity, and an ended playlist that stays ended backs
off the same way.

## Diagnosing buffering

The diagnostics line under the player shows the relay's cushion and the
player's own buffer. Relay stalls mean Kick or Boris's upstream failed for
longer than the cushion. Player stalls with a healthy cushion mean the iPad
could not fetch from Boris fast enough: its Wi-Fi, not Kick.

## Risks

`kick.com/api/v2` is unofficial and has been behind Cloudflare challenges in
the past; a block shows as "Retrying Kick" with the lookup's HTTP status.
yt-dlp in the image resolves the same playlists and is the fallback to wire in
if that happens.
