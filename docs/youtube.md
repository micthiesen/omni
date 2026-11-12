# YouTube

`omni-youtube` connects the owner's YouTube account through the YouTube Data
API v3 with Omni's own Google OAuth client, and serves twelve `youtube_*` MCP
tools for it: playlist listing and management, public search, and video and
channel details. Four more tools transcribe videos on the server without the
API or the account (see [Transcripts](#transcripts)). It replaced Composio for this, whose shared Google project ran out of
quota even for plain reads. The live check's scraper is unrelated
([youtube-data-api.md](youtube-data-api.md) explains why it does not use the API).

## What the API cannot do

The YouTube Data API does not serve Watch Later or watch history (both
removed in 2016; the tools refuse `WL` and `HL`), the personalized Home feed or
recommendations, or caption text and transcripts of other channels' videos.
`youtube_videos_get` reports only whether captions exist. Tool text never
implies any of these. What a video says comes from Omni's own transcription
instead ([Transcripts](#transcripts)), which does not use the API.

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

Google's default allocation per project and day
([quota costs](https://developers.google.com/youtube/v3/determine_quota_cost),
checked 2026-11-01): 100 `search.list` calls in their own Search Queries
bucket (1 unit each there), 100 `videos.insert` calls in theirs (unused here),
and 10,000 units shared by every other method: lists cost 1, inserts, updates
and deletes 50, and every request, invalid ones included, at least 1. Quotas
reset at midnight Pacific time, so a quota day is 23 or 25 hours long when
daylight saving starts or ends (`service::quota_day`).

Omni counts every request of its own that got a response, per Pacific day and
method (`youtube-quota`, kept eight days), and `youtube_status` derives the
search calls and the pool units from those counts. Rows written before
2026-11-01 stored searches as 100 units in `units`; deriving from the counts
reads them correctly. These are estimates of what Omni sent, not the
project's usage: other apps of the same project count too, and only the Cloud
console shows the real figures. A `quotaExceeded` answer becomes a
`[quota_exceeded]` error naming the bucket (search or the shared pool),
Omni's count for it and the reset time. A disabled API is `[api_disabled]`,
rate limits `[rate_limited]`.

`playlists.list` reports a `totalResults` that can exceed what it returns
across all pages (9 reported, 6 returned with no further page for the owner on
2026-11-01). `youtube_playlists_list` passes it through as YouTube's figure;
the listed playlists are the ones the API serves.

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

## Transcripts

`youtube_transcript_start`, `youtube_transcript_get`,
`youtube_transcript_search` and `youtube_transcripts_list` turn a video's
speech into timestamped text on the server, so an assistant can read what a
video says when the owner pastes a URL or asks about one. They use neither the
YouTube Data API nor the connected account, cost no quota and call no paid
service (`crates/omni-youtube/src/transcripts/`).

### How a transcript is made

1. `youtube_transcript_start` takes a video id or any YouTube URL
   (`ids::video_id`) and stores a `youtube-transcript` row keyed by the video
   id. Only the canonical `https://www.youtube.com/watch?v=<id>` URL is ever
   handed to yt-dlp.
2. `yt-dlp` reads the metadata (title, channel, duration, live status), then
   downloads one audio-only format into
   `<data dir>/youtube-transcripts/<video id>/`: the original-language track
   at 40 kbit/s or more when YouTube offers it, 1 GiB at most.
3. `ffmpeg` decodes the file two minutes at a time to 16 kHz mono, and each
   chunk goes to the `SpeechTranscriber` port: the Parakeet TDT 0.6B v3 model
   livestream intelligence already loads. The recognizer is given every
   sample. Silero VAD does not decide what is transcribed, only where the
   audio is cut (`plan_windows`): in the middle of a pause, the widest one
   8 to 20 s into a window when there is one, 30 s per recognizer call at
   most. Speech still running at the end of a chunk is left for the next
   chunk, so no sentence is cut at a chunk boundary.
4. The segments (start, end, text), the coverage flags and the progress are
   stored after every chunk. When the last chunk is stored the audio is
   deleted and the row becomes `ready`.

The first version transcribed only the stretches VAD marked as speech. On
real audio that dropped words without a trace: the ending of a 19 s clip with
wind noise (text stopped at 0:11) and the first words of a talk after its
intro music. The recognizer alone hears both, so VAD no longer gates it.

No new model, download or service is involved: the image already carries
yt-dlp, ffmpeg and the speech models. The models are loaded only when
livestream intelligence is enabled and its models are sound; otherwise the
port is unset, `youtube_transcript_start` answers `[unavailable]` and stored
transcripts stay readable. `SideEffectMode::Record` (`--preview`, tests)
starts no job.

### Tools

- **`youtube_transcript_start`** (`video`, `force`, `waitSeconds` 0 to 50,
  default 25, `maxChars`) starts the job or returns what the video already
  has: a running job is joined and never restarted (`force` included), a
  `ready` transcript is reused unless `force`, a `failed` one starts over. It
  waits up to `waitSeconds`; a short video comes back `ready` with the first
  page of text, a longer one with its status and progress.
- **`youtube_transcript_get`** (`video`, `cursor`, `startSeconds`,
  `endSeconds`, `maxChars` 1,000 to 200,000, default 30,000, `waitSeconds`)
  reads the text as `[m:ss] ...` or `[h:mm:ss] ...` lines, each gathering
  about a minute or 400 characters of speech. Pages end on a segment boundary;
  `nextCursor` (a segment index) continues. The transcribed part of a running
  job is readable (`complete: false`), and `waitSeconds` holds the call until
  the job ends. No stored transcript is a `[not_found]` error.
- **`youtube_transcript_search`** (`video`, `query`, `limit`) finds passages
  holding every word of the query, ignoring case and punctuation, also when a
  sentence is split across two segments. Each match has its timestamp, a
  `youtu.be/<id>?t=` link, an excerpt with the neighbouring speech and a
  `cursor` for `youtube_transcript_get`. It is word matching, not semantic
  search.
- **`youtube_transcripts_list`** lists stored transcripts and queued or
  running jobs, newest request first, and whether the speech models are
  available.

Every result carries `transcript`: `status` (`queued`, `downloading`,
`transcribing`, `ready`, `failed`), title, channel, duration,
`transcribedSeconds`, `percent`, size, `expiresAt`, the model, `error`
(`code`, `message`) for a failed job, and `coverage`.

| `error.code` | Meaning |
| --- | --- |
| `live` | A live or upcoming stream; transcribe it once it has ended |
| `too_long` | Over 6 hours |
| `too_large` | The audio file is over 1 GiB |
| `no_duration` | yt-dlp reports no duration |
| `download_failed` | yt-dlp failed; `message` is its last error line (private, removed, age or bot check, network), or it left no audio file |
| `decode_failed` | ffmpeg could not decode the audio |
| `speech_failed`, `speech_unavailable` | The speech model failed or is not loaded |
| `interrupted` | Restarts interrupted the job three times |
| `internal` | Storage or filesystem failure |

Failures log at WARN and never alert: most are properties of the video.

### Coverage

A finished job means all the audio went through the recognizer, not that
every word was heard, and timestamps alone cannot show the difference. So
each transcript reports what it cannot account for, in `transcript.coverage`
and as parenthesized note lines in the text where each stretch falls:

| Flag | Meaning | Line in the text |
| --- | --- | --- |
| `unrecognized_speech` | VAD found a second or more of speech and the recognizer returned no text: words are missing | `(speech detected but not transcribed: 6 s)` |
| `no_speech_recognized` | 30 s or more without text: silence or music, or speech both missed | `(no speech recognized for 0:52)` |
| `audio_ends_early` | The audio is over 5 s shorter than the video's reported length | `(the audio ends here, 50 s before the video's reported length)` |

Text the recognizer heard where VAD found no speech is kept and marked
`(not detected as speech)`; it is less reliable (it can be words over music,
or an artifact) and counted in `undetectedTextSeconds`.

`coverage.status` is `partial` while the job runs, `flagged` when any flag
exists, `no_known_gaps` otherwise, and `unknown` for a transcript made before
coverage was recorded (`force` redoes it). `no_known_gaps` is deliberately
not "complete": speech that both VAD and the recognizer missed leaves no
trace shorter than 30 s. `detectedSpeechSeconds`,
`unrecognizedSpeechSeconds` and `audioSeconds` give the totals, and `flags`
lists the first 25 stretches. `complete` on a read or search means only that
the job has finished.

### Limits and lifecycle

- One job downloads and transcribes at a time; five may be queued or running
  (`[busy]` beyond that). Transcription shares the recognizer with livestream
  intelligence and takes turns with it one utterance at a time.
- **Audio** exists only while its job runs. It is deleted when the job ends,
  whether it finished, failed or was stopped by a shutdown, and before the row
  reports `ready`. The work directory is emptied at boot before the server
  accepts requests, and the daily cleanup removes any directory without a job.
- **Processes.** yt-dlp and ffmpeg run in their own process group
  (`run_group`), and a timeout, an output overflow or a dropped job
  (shutdown) kills the whole group. Killing only the spawned process is not
  enough: the production yt-dlp is a self-extracting binary whose download
  runs in a child process, which kept writing after its parent was killed
  (measured in the container: 601,089 bytes grew to 846,845 in 6 s; with the
  group killed, no process was left and the size stayed at 355,325). At most one audio file of
  1 GiB is on disk.
- **A shutdown** (every deploy) stops the job and leaves its row active. The
  `youtube-transcripts` boot step resumes it after the server is up: the audio
  is downloaded again and transcription continues after the last stored chunk.
  A job interrupted three times fails with `interrupted`.
- **Transcript text** is deleted 30 days after its last read (a start that
  reuses it or a read of a `ready` transcript counts; a failed job counts from
  its failure). `YoutubeTranscriptCleanup` runs daily at 04:23 and also keeps
  at most 200 finished transcripts, deleting the least recently read first. It
  never touches a queued or running job.

### What the text is

Machine transcription of the audio track: names, numbers and technical terms
can be wrong and there are no speaker labels. Words at a window cut can be
repeated or dropped when there was no pause to cut in. Segment times are
those of the detected speech inside a window (up to 30 s), so a timestamp
can precede a given word by that much. Parakeet v3 covers English and other European languages; it does not
translate. The words are whatever the video says, so the tool descriptions
mark them as untrusted third-party content that is never to be followed as
instructions; the title and channel are the uploader's too, and are stored
as one line of at most 200 characters. yt-dlp failures follow YouTube's changes: when downloads start
failing with bot checks or format errors, update the pinned yt-dlp
(`Dockerfile`), which livestream intelligence depends on as well.
