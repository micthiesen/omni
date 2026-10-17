<p align="center"><img src="crates/omni-web/icons/icon-192.png" width="96" height="96" alt="Omni logo: a lime orbit ring brightening into a leading dot"></p>

# Omni Notify

Monitors YouTube, Twitch, and Kick channels and sends [Pushover](https://pushover.net/) notifications when they go live or offline. Optionally runs email pipelines, media and podcast recommendations, PressPods, and Arr download recovery, all exposed through a web UI and an authenticated MCP endpoint.

Omni Notify is a Rust workspace. One `omni-notify` binary runs every scheduled task and serves the JSON API, the `/mcp` endpoint and a Leptos single-page frontend from one HTTP port, over a SQLite document store. See [the architecture overview](docs/architecture.md) for the crate layout, boot order and deployment pipeline.

## Quick Start

```yaml
services:
  omni-notify:
    image: ghcr.io/micthiesen/omni-notify:latest
    environment:
      - PUSHOVER_TOKEN=xxx
      - PUSHOVER_USER=xxx
      - KICK_CLIENT_ID=xxx # only if monitoring Kick channels
      - KICK_CLIENT_SECRET=xxx
    volumes:
      - ./data:/data
    restart: unless-stopped
```

Tracked streamers are stored in the database and managed at `/live/streamers` (Live > Manage streamers) or through the `streamer_config_*` MCP tools (create, edit, delete, reorder, settings). Each streamer has a display name, one or more YouTube handles, Twitch logins or Kick slugs, a tier and an optional live-notification override. A source can belong to only one streamer. A per-streamer Pushover app token can be set or cleared in the UI only; it is never returned by the API or accepted over MCP.

The `dggTopEmbeds` setting (0-20) adds that many currently hosted or most-watched embeds from Destiny.gg. These are refreshed on the relaxed background cadence, never send live/offline/title notifications, and disappear when they leave the top set. A source already represented by a configured streamer is enriched with its DGG audience/host status instead of duplicated; its configured tier, platform polling, and notification behavior remain authoritative. `0` disables discovery.

**One configured streamer = one identity.** A streamer live on multiple platforms gets **one** "went live" notification when they start streaming anywhere and **one** "went offline" notification when all platforms go offline, so multistreams never double-ping.

## How It Works

Checks every 20 seconds (with random jitter) whether monitored channels are live. Sends a notification on aggregate status transitions (offline-everywhere → live-anywhere, or live-anywhere → offline-everywhere). State is persisted in SQLite so it survives restarts.

- **YouTube**: Scrapes the channel's `/live` page HTML. No API key needed, but could break if YouTube changes its page structure.
- **Twitch**: Uses Twitch's public GraphQL API. No authentication required. More stable than YouTube scraping.
- **Kick**: Uses Kick's official public API (`api.kick.com/public/v1/channels`). Requires registering an app at [dev.kick.com](https://dev.kick.com) and providing `KICK_CLIENT_ID` + `KICK_CLIENT_SECRET` (scope: `channel:read`). The app-only access token is cached and refreshed automatically.

**Primary binding.** When a streamer is live on multiple platforms, one binding is chosen as the "primary" for the notification URL and title. The first platform to go live wins, and sticks for the rest of the session. If they go live simultaneously, priority order is YouTube → Twitch → Kick.

Set `OFFLINE_NOTIFICATIONS=false` to only get notified when channels go live.

### Livestream Intelligence

Set `LIVESTREAM_INTELLIGENCE_ENABLED=true` to add a Boris-local semantic layer to live-check. The production image bundles Silero VAD, an English CAM++ speaker model, Parakeet TDT v3 INT8, and yt-dlp. It samples live audio without storing recordings, uses local inference for speech and speaker recognition, and sends compact transcripts to Luna. There is no separate title-only LLM call. The dashboard then shows transcript-backed current summaries, relevance score, viewer anomalies, and recent topic chapters. Each streamer page links to a dedicated Intelligence Details view with live queue health, per-stage status, voice evidence, transcript excerpts, budget usage, and a bounded decision timeline that remains available after the stream ends.

Destiny guest detection runs only on third-party DGG streams. It requires two multi-window speaker matches within five minutes, followed by transcript confirmation that he is participating live rather than appearing in a clip. His own configured or DGG-discovered streams are always excluded. Alerts cover confirmed guest appearances, viewer surges, substantive debates, breaking news, major announcements, and notable guests. Feedback on the streamer detail page raises the confidence threshold when an alert type accumulates negative corrections.

Metered calls are restricted to `openai:gpt-6-luna` and capped at $3 per calendar month. Local audio inference records zero-cost usage events on the Costs page. Generate the persistent Destiny voiceprint inside the production container from at least two public clips where he is the only common speaker:

```bash
docker exec omni-notify omni-voice-enroll \
  --source 'https://www.youtube.com/watch?v=FIRST' --seek 600 \
  --source 'https://www.youtube.com/watch?v=SECOND' --seek 600 \
  --output /data/livestream-intelligence/destiny.json
```

Set `LIVESTREAM_DESTINY_VOICEPRINT_PATH=/data/livestream-intelligence/destiny.json` after enrollment. `docker exec omni-notify omni-intel-doctor --url <URL>` captures a bounded sample and reports transcript speed and speaker-match evidence.

If the bundled speech or speaker model files are missing, empty or corrupt, boot logs `Livestream intelligence disabled: ...` and the rest of the service, including live checks, keeps running.

## iOS app

The private **Omni** iOS/iPadOS app receives every Omni notification over APNs,
with streamer avatars on go-live and title-change alerts. Pushover is only the
fallback when the app is not set up: APNs is unconfigured, no device has alerts
enabled, or every device rejected the push. The app also provides On Air and
Deliveries widgets, four Control Center live slots, and a full-screen view of
the Omni UI.

See [the iOS app guide](docs/ios-app.md) for the notification fallback rule,
the signed API, Apple/APNs setup, the `scripts/ios-live/*.sh` commands, and
private device installation.

## Per-Streamer Options

Alongside its sources, each streamer has:

- A Pushover app token (UI only): overrides the Pushover token for this streamer's notifications.
- Tier: set to background for second-tier streamers you check on the dashboard but never want pushed about. Mutes live/offline/title-change notifications, restricts viewer-record notifications to all-time highs only, and polls at a relaxed cadence (every 60s instead of 20s). Tracking, the dashboard, and `/api/trigger-channels` are unaffected. It cannot be combined with a live-notification override.
- Live notifications: turn off to mute live/offline/title-change notifications while keeping full-rate polling (e.g. for external integrations that need fast state). Viewer-record notifications (7d/30d/90d/all-time highs) still fire.

Viewer records are confirmed once the count falls 5 percent below the peak, or when the stream goes offline. Primary-tier streamers are notified of new 7, 30 and 90-day highs as well as all-time highs; a record already confirmed today is not repeated.

Title-change notifications are eagerly debounced: the first change fires immediately, further changes within 10 minutes are held with the last one winning.

## Media Recommendations

A scheduled pipeline (default: Mon/Wed/Fri at 5pm) that picks at most one movie or TV title per run, acquires missing titles through Radarr or Sonarr, and sends a Pushover notification explaining the pick. Titles already available in Plex are recommended without another acquisition request.

Each run:

1. Polls Plex history, series-level progress, local availability, and the Radarr/Sonarr tracked catalog. It labels passive outcomes (started, watched, abandoned, ignored) and incorporates explicit "good pick" or "not for me" feedback.
2. Builds a candidate pool from TMDB (recommendations seeded by recent watches, genre discovery, trending, plus a novelty bucket outside your usual genres).
3. Hard-filters in code: anything watched, in progress, tracked by Radarr/Sonarr, explicitly rejected, or recently recommended is dropped before a model sees it.
4. Enriches candidates with structured TMDB commitment and creative metadata, then scores the pool with a cheap model (`RECS_SHORTLIST_MODEL`) and keeps the top 5.
5. Researches the finalists with web search, then a strong model (`RECS_SELECTION_MODEL`) picks exactly one title or decides to add nothing that day.

Plex is the source of watch history, in-progress state, and local availability. Radarr handles movie acquisition and Sonarr handles TV acquisition. All reads fail closed: an unavailable service skips the run instead of treating missing state as an empty library.

A separate weekly `TasteReflection` task maintains a versioned taste profile. It converts Plex observations and recommendation outcomes into an idempotent evidence ledger, computes behavioral statistics, and performs a bounded draft-and-critic reflection. Every learned claim must cite stored evidence. The latest profile is added to recommendation context and shown in the UI; code, prompts, and scoring rules are never self-modified. If no evidence changed, reflection exits without a model call.

See [the recommendation review checkpoint](docs/recommendations-review.md) for the decisions intentionally deferred until enough real recommendations have outcomes.

## Arr Download Recovery

Sonarr and Radarr download recovery runs independently every five minutes. It
waits for persistent import failures, imports verified matches with a guarded Luna
fallback, removes redundant/bad downloads, and batches actions through Pushover.
See [Arr recovery](docs/arr-recovery.md) for safeguards and deployment settings.

## Podcast Recommendations

A sibling pipeline (default: Mon/Wed/Fri at 11am, enabled by setting `PODCAST_TASTE_PATH`) that recommends fresh podcast **episodes** from shows you don't already follow, and sends a Pushover notification per pick. It's **people-first**: the main goal is surfacing episodes where a voice you follow guests somewhere new (see [docs/podcast-recs.md](docs/podcast-recs.md)).

Each run:

1. Reads subscribed shows and listen history from the Castro account (see [docs/castro-sync.md](docs/castro-sync.md)). Subscribed shows are excluded and double as taste evidence alongside the seed profile and explicit feedback; a failed account read aborts the run. Without Castro credentials it runs off the seed profile and feedback alone.
2. **Tier 1: guest appearances.** For a rotating batch of the people in your profile's `## Voices` list, finds recent episodes featuring them as guests via Podcast Index (`byperson`, free) with a Tavily person-search fallback for non-podcasters. A model gate default-includes them (following the person is the signal), capped so a press-tour week can surface several.
3. **Tier 2: topic/drama.** Multi-angle web search → cheap shortlist → strong-model one-pick, conservative and suppressed once Tier 1 delivered enough.
4. Verifies every candidate's release date against the show's actual RSS feed (or Podcast Index's resolved data), and hard-filters in code: older than 7 days, already recommended, show on 30-day cooldown, rejected, or subscribed.

Recommended episodes are never repeated. When Castro credentials are set, listen history labels outcomes (listened / abandoned / ignored) automatically and each selected episode is resolved by RSS URL and added to the end of the Castro queue before notification. Resolution or enqueue failure falls back safely to the recommendation deep link. The good-pick/not-for-me feedback buttons in the web UI are always available.

The same Castro credentials enable an independent `CastroInboxCleanup` task.
It runs every six hours and silently clears Inbox episodes whose description begins with
`This is a free preview`, the standard marker used by Substack preview
episodes. It changes only the Inbox `is_new` state and never removes an episode
from the Queue. Matching is deliberately case-sensitive and prefix-only.

## PressPods (Article → Podcast)

Enabled by setting `PRESSPODS_AUTH_TOKEN` (plus a Google API key and a TTS backend: either a self-hosted Higgs server via `PRESSPODS_TTS_URL`, or `ELEVENLABS_API_KEY` with `PRESSPODS_TTS_PROVIDER=elevenlabs`). Submit an article URL (from an iOS Shortcut via `POST /pods/episodes?authToken=…` with `{ "url": "…" }`, or from the `/pods` page) and it becomes an episode in a private podcast RSS feed (`GET /pods/rss?authToken=…`) your podcast app subscribes to.

Each submission becomes a durable job (visible on `/pods`, with retries and per-run logs) processed by the `PressPods` task:

1. Seven article retrievers run in parallel (Postlight, Readability, Extractus, Wayback, removepaywall, raw fetch, and Jina.ai when `JINA_API_KEY` is set); an LLM rates each result's extraction quality and the best wins.
2. A broadcast-style cleaning pass rewrites the article for the ear (one-idea sentences, attribution-first quotes, number rounding, a cold-open hook and a spoken outro) and marks major sections for chapters.
3. The configured TTS backend (self-hosted Higgs v3, or ElevenLabs v3) synthesizes each chunk separately, with a length-verify retry for the local model; ffmpeg denoises (Higgs), per-chunk-levels, two-pass loudness-normalizes to -16 LUFS, and joins the intro jingle click-free; the lead image and chapter markers are embedded as ID3 tags.
4. The MP3 is stored on disk (next to the SQLite DB by default) and served at `/pods/audio/<id>.mp3` with an unguessable content-addressed name; a Pushover notification announces the episode.

Transient failures (TTS 429/5xx, network blips) retry automatically with backoff; permanent failures surface on `/pods` with a retry button. Submitted articles are also bookmarked in Karakeep when `KARAKEEP_URL`/`KARAKEEP_API_KEY` are set. To expose the feed publicly, reverse-proxy just the `/pods/*` paths and set `PRESSPODS_PUBLIC_URL` to the public origin (or let it derive from `X-Forwarded-*` headers).

## Web UI

The built-in server (port `FRONTEND_PORT`, default 3000) serves the Omni Notify dashboard, a Leptos single-page app built by trunk and served from `OMNI_WEB_DIST`:

- `/` shows live streamer status (who's live now, title, uptime, peak viewers), a stat strip, every scheduled task with its cron schedule, ticking next-run countdown, "Run now" button and expandable run history, plus a recent-activity feed with per-task filtering.
- `/pets` is the pet weight tracker.
- `/media` (`/recommendations` redirects there) lists every recommendation with poster, status, reasoning, service links, explicit feedback controls, filters, the current evidence-backed taste profile, and recent pipeline activity.
- `/podcasts` lists podcast episode recommendations with show artwork, status filters, episode/discussion links, good-pick/not-for-me feedback controls, and the podcast taste profile.
- `/feedback/recommendations/:id` and `/feedback/podcasts/:id` are mobile-first one-tap rating pages. Pushover recommendation notifications deep-link here ("Rate this pick"), and the page links onward to the full recommendation view.
- `/pods` lists PressPods episodes with an inline player, costs, and processing logs, plus a submit-URL form and retry controls for failed jobs.
- `/emails` shows what the parcel and calendar email pipelines did with each email (why it was admitted or filtered, per-item results, honest processed/partial/failed outcomes) with per-email processing logs, one-click reprocess, block-sender and not-relevant/missed feedback actions, a forget-tracking-number escape hatch, and a user-editable sender-rules section. A shared LLM triage call gates both pipelines; corrections feed back into its prompt.
- `/streamers/:id` (with `/streamers/:id/intelligence`), `/costs`, `/data`, `/operations`, `/mcp-activity`, `/claude` and `/reminders` cover per-streamer detail and livestream intelligence, model and service costs, the data manager, task operations, MCP call history, Claude Code sessions and server iCloud Reminders sign-in.

Updates are pushed in realtime over SSE (`/api/events`) on the same HTTP port, so no extra ports are needed; the UI falls back to polling `/api/snapshot` (and shows a "Reconnecting" badge) if the stream drops. Task runs are persisted in SQLite (last 50 per task) so history survives restarts.

### Executor MCP

Omni exposes its existing personal capabilities through a streamable-HTTP MCP
endpoint at `/mcp` on the same `FRONTEND_PORT`, advertised under the general
server name `omni`. Every MCP request must send
`Authorization: Bearer <OMNI_MCP_TOKEN>`. Production refuses to start if the
token is missing, shorter than 32 characters, or lacks sufficient character
diversity. Generate and store the production token in deployment secrets; do
not put it in this repository.

The server provides bounded, typed tools for iCloud email and calendar,
tasks and run logs, livestreams, media and podcast services,
PressPods, pets, costs, and an optional fixed IPP printer. The
printer tools report status and print bounded public PDF URLs in monochrome,
with long-edge duplex by default; every physical print requires approval. It
calls the underlying services directly rather than looping through Omni's HTTP
API. It does not expose raw
credentials, environment variables, arbitrary shell/filesystem access, generic
database entities, credential-bearing HTTP, or attachment/audio bytes.

Executor must enforce [the generated policy inventory](docs/mcp-policy.json).
Tools that communicate externally, mutate calendars or accounts, start media
acquisition, publish content, send notifications, run paid models/search/TTS,
or otherwise have consequential effects are marked `require_approval` there.
MCP annotations describe behavior but are not an approval mechanism. The policy
file is generated from the same definitions registered by the server and is
checked in tests for drift. See [the MCP operations guide](docs/mcp.md) for the
transport, authentication, tool-family, and deployment contract.

To iterate on the frontend without real credentials, `omni-notify --preview` boots the real server over a throwaway database seeded with fake streamers, runs, recommendations, email activity and a pet, with every side effect recorded and outgoing HTTP refused (see [Development](#development)).

## AI Model Configuration

Models are configured via environment variables using `provider:model` format. Supported providers: `google`, `anthropic`, `openai`. You only need an API key for the provider you're using.

| Variable | Default | Used for |
|---|---|---|
| `EXTRACTION_MODEL` | `openai:gpt-6-luna` | Parcel email extraction |
| `CALENDAR_EXTRACTION_MODEL` | `openai:gpt-6-sol` | Calendar email extraction |
| `TRIAGE_MODEL` | `openai:gpt-6-luna` | Shared email relevance triage |
| `RECS_SHORTLIST_MODEL` | `openai:gpt-6-luna` | Recommendation shortlist scoring |
| `RECS_SELECTION_MODEL` | `openai:gpt-6-sol` | Recommendation research + final pick |
| `TASTE_REFLECTION_MODEL` | `openai:gpt-6-luna` | Weekly evidence-backed taste reflection |
| `PRESSPODS_METADATA_MODEL` | `openai:gpt-6-luna` | PressPods per-retriever metadata + rating |
| `PRESSPODS_CLEANING_MODEL` | `openai:gpt-6-sol` | PressPods narration rewrite |

Examples:

```bash
TRIAGE_MODEL=openai:gpt-6-luna
TRIAGE_MODEL=anthropic:claude-sonnet-5
TRIAGE_MODEL=google:gemini-3.5-flash
```

## Environment Variables

| Variable | Required | Description |
|---|---|---|
| `PUSHOVER_USER` | Yes | Pushover user key |
| `PUSHOVER_TOKEN` | Yes | Pushover app token |
| `OMNI_MCP_TOKEN` | In production | Random bearer token for authenticated streamable HTTP at `/mcp`; minimum 32 diverse characters |
| `OMNI_DEVICE_LINK_TOKEN` | No | Device token for the Mac's `omni-link` agent; enables the `claude_*` MCP tools. Minimum 32 diverse characters, different from `OMNI_MCP_TOKEN` |
| `PRINTER_IPP_URL` | No | Fixed `ipp://` or `ipps://` endpoint that enables MCP printer status and approved monochrome PDF printing |
| `KICK_CLIENT_ID` | No | Kick OAuth client ID ([dev.kick.com](https://dev.kick.com)); required to poll Kick sources |
| `KICK_CLIENT_SECRET` | No | Kick OAuth client secret |
| `OFFLINE_NOTIFICATIONS` | No | Send offline notifications (default: `true`) |
| `LIVESTREAM_INTELLIGENCE_ENABLED` | No | Enable Boris-local transcription, summaries, speaker detection, and semantic alerts (default: `false`) |
| `LIVESTREAM_MONTHLY_BUDGET_USD` | No | Hard Luna spend ceiling for livestream intelligence, from $0 to $10 (default: `$3`) |
| `LIVESTREAM_DESTINY_VOICEPRINT_PATH` | No | Persistent Destiny enrollment JSON; guest detection stays disabled when omitted |
| `LIVESTREAM_DESTINY_SPEAKER_THRESHOLD` | No | CAM++ cosine threshold (default: `0.62`) |
| `LIVESTREAM_MAX_VOICE_TARGETS` | No | Maximum concurrent DGG voice targets (default: `3`) |
| `LIVESTREAM_VOICE_SAMPLE_SECONDS` / `LIVESTREAM_VOICE_SAMPLE_INTERVAL_SECONDS` | No | Voice sample length / cadence (defaults: `18` / `45`) |
| `LIVESTREAM_SUMMARY_INTERVAL_SECONDS` | No | Length of each continuously recorded summary window (default: `480`) |
| `EXTRACTION_MODEL` | No | AI model for parcel email extraction (default: `openai:gpt-6-luna`) |
| `CALENDAR_EXTRACTION_MODEL` | No | AI model for calendar email extraction (default: `openai:gpt-6-sol`) |
| `TRIAGE_MODEL` | No | AI model for shared email triage (default: `openai:gpt-6-luna`) |
| `EMAIL_SELF_ADDRESS` | No | Own receiving address for self-sent-mail filtering (default: `ICLOUD_USERNAME`) |
| `ICLOUD_USERNAME` | No | iCloud primary username (`user@icloud.com`, not the custom-domain address); IMAP + CalDAV |
| `ICLOUD_APP_PASSWORD` | No | iCloud app-specific password (IMAP + CalDAV) |
| `ICLOUD_CALENDAR_NAME` | No | Display name of the iCloud calendar to write to (e.g. `Personal`) |
| `ICLOUD_CALENDAR_URL` | No | Pins the iCloud calendar collection URL (default: RFC 6764 discovery) |
| `GOOGLE_GENERATIVE_AI_API_KEY` | No | Required for `google:` models |
| `ANTHROPIC_API_KEY` | No | Required for `anthropic:` models |
| `OPENAI_API_KEY` | No | Required for `openai:` models |
| `TAVILY_API_KEY` | No | Tavily web search (required for recommendations) |
| `TMDB_API_KEY` | No | TMDB API key (required for recommendations; v3 key or v4 read token) |
| `RECS_SCHEDULE` | No | Recommendation cron (default: `0 0 17 * * 1,3,5`) |
| `TASTE_REFLECTION_MODEL` | No | Model for evidence-backed taste reflection (default: `openai:gpt-6-luna`) |
| `TASTE_REFLECTION_SCHEDULE` | No | Taste-profile reflection cron (default: `0 0 4 * * 0`, Sunday 4am) |
| `RECS_PUBLIC_URL` | No | Public/LAN Omni base URL used by notification links (default: `http://omni.boris`) |
| `PUSHOVER_RECS_TOKEN` | No | Pushover token for recommendations (falls back to `PUSHOVER_TOKEN`) |
| `PODCAST_TASTE_PATH` | No | Markdown listener profile (required to enable podcast recommendations) |
| `PODCAST_RECS_SCHEDULE` | No | Podcast recommendation cron (default: `0 0 11 * * 1,3,5`) |
| `PUSHOVER_PODCAST_TOKEN` | No | Pushover token for podcast recs (falls back to `PUSHOVER_TOKEN`) |
| `CASTRO_ACCESS_ID` / `CASTRO_SECRET_KEY` | No | Castro device credentials (account reads, queue writes, and six-hourly Inbox cleanup) |
| `PODCASTINDEX_KEY` / `PODCASTINDEX_SECRET` | No | Podcast Index API (guest-appearance discovery; quote the secret, which contains `#`) |
| `PODCAST_VOICE_ROTATION_MAX` / `PODCAST_MAX_GUEST_PICKS` | No | Voices searched per run (default 12) / Tier-1 guest cap (default 6) |
| `PODCAST_TASTE_REFLECTION_MODEL` | No | Model for weekly podcast taste reflection (default: `openai:gpt-6-luna`) |
| `PODCAST_TASTE_REFLECTION_SCHEDULE` | No | Podcast taste reflection cron (default: `0 0 5 * * 0`, Sunday 5am) |
| `PRESSPODS_AUTH_TOKEN` | No | Long random secret; enables PressPods and authenticates `/pods/episodes` + `/pods/rss` |
| `PRESSPODS_TTS_PROVIDER` | No | TTS backend: `higgs` (self-hosted, default) or `elevenlabs` |
| `PRESSPODS_TTS_URL` | For higgs | mlx-audio server URL, e.g. `http://10.10.1.90:8000` |
| `PRESSPODS_TTS_MODEL` | No | Higgs model repo (default `bosonai/higgs-audio-v3-tts-4b`) |
| `ELEVENLABS_API_KEY` | For elevenlabs | ElevenLabs v3 TTS |
| `ELEVENLABS_VOICE_MALE` / `ELEVENLABS_VOICE_FEMALE` | No | Voice-id overrides for narration (default: Brian / Matilda) |
| `PRESSPODS_PUBLIC_URL` | No | Public origin for RSS enclosure URLs (else derived from forwarded headers) |
| `PRESSPODS_AUDIO_DIR` | No | Episode MP3 directory (default: `press-pods-audio` next to the DB) |
| `JINA_API_KEY` | No | Enables the Jina.ai Reader retriever |
| `PUSHOVER_PRESSPODS_TOKEN` | No | Pushover token for PressPods (falls back to `PUSHOVER_TOKEN`) |
| `KARAKEEP_URL` / `KARAKEEP_API_KEY` | No | Bookmark submitted articles in Karakeep |
| `PLEX_URL` / `PLEX_TOKEN` | For recommendations | Plex server URL and token |
| `PLEX_ACCOUNT_ID` | For shared Plex servers | Account ID used to scope viewing history; multiple detected accounts fail closed without it |
| `RADARR_URL` / `RADARR_API_KEY` | For recommendations | Radarr v3 API connection |
| `RADARR_ROOT_FOLDER_PATH` / `RADARR_QUALITY_PROFILE_ID` | For recommendations | Defaults for acquired movies |
| `SONARR_URL` / `SONARR_API_KEY` | For recommendations | Sonarr v3 API connection |
| `SONARR_ROOT_FOLDER_PATH` / `SONARR_QUALITY_PROFILE_ID` | For recommendations | Defaults for acquired series |
| `LOG_LEVEL` | No | `debug`, `info`, `warn`, or `error` |

## Development

Prerequisites: rustup (the toolchain is pinned in `rust-toolchain.toml`) with the
`wasm32-unknown-unknown` target, [trunk](https://trunkrs.dev/) 0.21.14 and
wasm-bindgen-cli 0.2.129 for the frontend, `cargo-deny`, `cargo-sweep`, and
[dotenvx](https://dotenvx.com/) for running against a local `.env`. ffmpeg is
needed only for the ignored real-audio PressPods test. The livestream intelligence
build links the sherpa-onnx static library, which its build script downloads
unverified by default; CI and the Docker build instead run
`deploy/sherpa-onnx/fetch-static-lib.sh <dir>` and set `SHERPA_ONNX_ARCHIVE_DIR=<dir>`
to use a SHA-256-verified copy.

```bash
rustup target add wasm32-unknown-unknown
cargo xtask gate                  # fmt, clippy (native + wasm32), deps-check, tests
cargo deny --locked check         # advisories, licenses, bans, sources
cargo test -p omni-live           # one crate
cargo fmt --all                   # format

# Frontend: build once, or serve with hot reload on :8081 (proxies /api to :3000)
(cd crates/omni-web && trunk build --release)
(cd crates/omni-web && trunk serve)

# Run against .env (FRONTEND_PORT defaults to 3000)
dotenvx run -- cargo run -p omni-notify -- --web-dist crates/omni-web/dist

# Fake data, no credentials, nothing leaves the machine
DB_NAME=/tmp/omni-preview.db cargo run -p omni-notify -- --preview --port 3999 \
  --web-dist crates/omni-web/dist
```

Other `omni-notify` modes: `--server-only` (HTTP without the scheduler),
`--run-task <Name>` (one task, then exit), `--side-effects=record` (record every
outgoing mutation instead of sending it), `healthcheck`, `doctor [--image]`, and
`compat-audit --db <copy>`. `cargo xtask help` lists the
repository tooling (golden fixtures, MCP snapshots, dependency rules, target
hygiene).

Inspired by [youtube_live_alert](https://github.com/your-diary/youtube_live_alert).
