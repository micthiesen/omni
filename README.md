<p align="center"><img src="crates/omni-web/icons/icon-192-v2.png" width="96" height="96" alt="Omni logo: a lime orbit ring brightening into a leading dot"></p>

# Omni

Omni is a personal automation service. It watches livestreams, reads email,
keeps the calendar and deliveries current, recommends films, shows and podcast
episodes, turns articles into a private podcast, and exposes all of it to agents
through an authenticated MCP endpoint. It runs as one container on a home
server, with a web dashboard and a companion iOS app.

It is built for one household. The code is public; the data, credentials and
deployment are not.

## What it does

**Livestreams.** Polls YouTube, Twitch and Kick (plus Destiny.gg embeds) every
20 seconds. A streamer is one identity across platforms: one push when they go
live anywhere, one when they are offline everywhere, title changes debounced,
viewer records for 7, 30, 90-day and all-time highs. Optional local
livestream intelligence transcribes live audio on the server, attributes
speakers, writes rolling summaries, and flags viewer surges and notable guests.

**Email, calendar and deliveries.** Reads iCloud mail over IMAP without
dropping messages, triages it with one shared model call, and routes it to
pipelines: parcel tracking numbers go to Parcel, appointments and reservations
become calendar events over CalDAV (merged, never clobbering manual edits).
Delivery status is cached within Parcel's strict rate limits. Agents can search
mail, draft and send from `michael@thiesen.dev`, and read and edit the primary
calendar with durable idempotency.

**Media and podcasts.** Recommends at most one film or show per run from Plex
history, TMDB and web research, and acquires it through Radarr or Sonarr.
Recommends new podcast episodes, mostly where a voice you follow guests
somewhere new, and queues them in Castro. Both keep an evidence-backed taste
profile. Arr recovery imports stuck downloads, and observer repair fixes
reported playback problems.

**PressPods.** Submit an article URL and it becomes an episode in a private
podcast feed: best-of-seven article extraction, a rewrite for listening, TTS
(self-hosted Higgs or ElevenLabs), loudness-normalized audio with chapters.

**Personal.** Pet weight and litter-box health watch, server-side iCloud
Reminders, a LAN printer, a fit-first stylist ledger, Codex and Claude Code
usage-reset alerts, and Claude Code sessions started on the Mac through a
long-poll relay.

**Notifications.** Every alert goes to the Omni iOS app over APNs first, with
streamer avatars and widgets; Pushover is the fallback when the app is not set
up. Failures, degraded runs and recoveries are reported once per incident.

**MCP.** `/gateway/mcp` serves bounded, typed tools for everything above, plus MCP
Events (`email.received`, `livestream.status_changed`, `calendar.event_changed`
and others) through one outbox. Consequential tools are marked
`require_approval` in [the generated policy](docs/mcp-policy.json); the
connected client (Claude Code, Codex, ChatGPT) owns approval.

## How it is built

One Rust workspace and one `omni` binary: the scheduler, every task, an axum
server with the JSON API, `/gateway/mcp` and a Leptos single-page frontend on one port,
over a SQLite document store. Subsystems talk only through port traits, every
external write honors a record-only side-effect mode, and tests never reach the
network. See [the architecture overview](docs/architecture.md).

| Part | Where |
|---|---|
| Server and tasks | `crates/omni` and the `crates/omni-*` subsystems |
| Web dashboard | `crates/omni-web*` (Leptos, built by trunk), styled per [the design system](docs/design-system.md) |
| iOS app and widgets | `ios/OmniLive` ([guide](docs/ios-app.md)) |
| MCP gateway | `crates/omni-mcp/src/gateway`, `deploy/gateway` ([guide](docs/gateway.md)) |
| Omni Slate e-paper display | `slate/` (its own workspace and gate) |
| Logo and icons | `scripts/brand/icons.py` |

## Running it

Production is the `ghcr.io/micthiesen/omni` image, published by CI on every
push to `main` and deployed automatically on the home server:

```yaml
services:
  omni:
    image: ghcr.io/micthiesen/omni:latest
    container_name: omni
    env_file: .env
    environment:
      - FRONTEND_PORT=8080
    volumes:
      - ./volumes/omni:/data
    ports:
      - "8080:8080"
    restart: unless-stopped
```

Only `PUSHOVER_USER` and `PUSHOVER_TOKEN` are required; every feature turns on
when its configuration is present and stays off otherwise. [`.env.example`](.env.example) lists the variables
and their defaults, and `crates/omni-config` validates them at boot.

The `omni` binary has a few other modes:

```bash
omni --preview --port 3999        # fake data, every side effect recorded
omni --run-task <Name>            # run one task, then exit
omni --side-effects=record        # run normally, record instead of sending
omni healthcheck | doctor [--image] | compat-audit --db <copy>
```

## Development

Prerequisites: rustup (toolchain pinned in `rust-toolchain.toml`) with the
`wasm32-unknown-unknown` target, trunk 0.21.14, `cargo-deny`, `cargo-nextest`,
`cargo-sweep`, and [dotenvx](https://dotenvx.com/) for a local `.env`.

```bash
cargo xtask gate                  # fmt, clippy (native and wasm32), deps-check, tests
cargo deny --locked check         # advisories, licenses, bans, sources
(cd crates/omni-web && trunk build --release)

dotenvx run -- cargo run -p omni -- --web-dist crates/omni-web/dist
DB_NAME=/tmp/omni-preview.db cargo run -p omni -- --preview --port 3999 \
  --web-dist crates/omni-web/dist
```

[`AGENTS.md`](AGENTS.md) holds the working conventions and the invariants each
subsystem must keep. Feature guides live in [`docs/`](docs): [calendar](docs/calendar.md),
[MCP](docs/mcp.md), [MCP Events](docs/mcp-events.md),
[podcast recommendations](docs/podcast-recs.md), [PressPods audio](docs/presspods-audio.md),
[Arr recovery](docs/arr-recovery.md), [observer repair](docs/observer-repair.md),
[pet health](docs/pet-health.md), [server Reminders](docs/server-reminders.md),
[stylist](docs/stylist.md), [Claude sessions](docs/claude-sessions.md) and
[reset alerts](docs/codex-resets.md).
