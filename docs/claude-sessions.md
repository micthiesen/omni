# Claude Code sessions on the Mac

Omni's `claude_*` MCP tools control Claude Code sessions on Michael's MacBook
through `claude-for-dot`, the JSON session client in the dotfiles repository.
Any MCP client connected to the gateway (`/gateway/mcp`) can use them.

## Link

The Mac connects to Omni; Omni never connects to the laptop. The dotfiles
`omni-link` LaunchAgent long-polls `POST /device-link/poll` on
`https://omni.syas.ca` and returns each job's output to `POST /device-link/result`.
This works on the home LAN and over the WireGuard VPN, and needs no inbound port,
SSH, or stable Mac address.

Both endpoints accept only `Authorization: Bearer <OMNI_DEVICE_LINK_TOKEN>`. The
device token must be strong and separate from every gateway token; it cannot call
the gateway, and a gateway token cannot poll for jobs. On the Mac the token is stored in
the login Keychain (`omni-link set-token`).

Omni holds each poll for up to 25 seconds. The Mac counts as online while a poll
is open or one ended in the last 45 seconds. Tool calls fail immediately with:

- `offline`: the Mac is asleep, away without VPN, or the agent is stopped.
- `disabled`: the Mac's kill switch is on (`omni-link disable`).
- `not_picked_up`: no poll took the job within 30 seconds. The job is withdrawn
  and can no longer run.
- `outcome_unknown`: the Mac took the job but no result arrived in time. Omni
  never retries; read the session before repeating a start or send.

Omni waits 15 seconds past each command's own timeout so the Mac can report
its timeout first. A newer poll releases an older held one, so a connection
left behind by an agent restart cannot take jobs. If an MCP caller disconnects
after the Mac takes a job, the job still runs and its result is discarded.
Job state is in memory. An Omni restart fails in-flight calls with
`outcome_unknown` or `offline`; it does not replay them.

## Projects and permissions

New sessions start only in the projects the Mac's `claude-rc` Remote Control
supervisor uses: `claude-rc/config.toml` in dotfiles plus the Mac's
`~/.config/claude-rc/local.toml`, enabled entries only. `claude_link_status`
returns that list and `claude_session_start` takes a project name. All other
tools work on every session on the Mac, whatever its directory or origin.
Every session result names the project holding the session's directory (the
deepest one), or `null`. `claude-for-dot` names it only in `list` and `start`
replies, so Omni fills it in for `get` and `send` from the project list,
cached for a minute.

Sessions intentionally run with the same full access as `claude-for-dot`
(`--dangerously-skip-permissions`), so a background session never stalls on a
permission prompt. Bypass mode does not cover every prompt: a project whose own
settings enable Claude Code's sandbox can still stop a command on a sandbox
network request, and dialogs or questions can open. The host then reports the
session `waiting`, with `waitingFor` naming the prompt (`sandbox request`,
`dialog open`, `input needed`, ...). Only the session itself, for example
through Remote Control, can answer it. Approval happens before the session runs instead, in the connected client:
`start`, `send`, and `stop` have the recommended policy `require_approval`; listing, reading,
and waiting are `allow`. This is a deliberate design, not a gap to close.

Starts require an `idempotencyKey`, so a retried start returns the existing
session. Sends accept an optional `idempotencyKey`: a retry after a launched send
returns it with `reused: true`, and a retry whose earlier attempt never recorded
its launch fails with `send_in_doubt` instead of sending twice.

## Tools

The MCP surface calls the machine "the Claude Code host" and never names it.
Tool text cannot mention Mac, macOS, laptops, or the hostname (a policy test
enforces this). Results and errors replace the hostname with "the host" and
home directories with `~`, transcript text included. `claude_link_status` omits
the hostname. Omni's own UI and stored data may still show it.

| Tool | Purpose |
| --- | --- |
| `claude_link_status` | Link state (works while the Mac is offline) and the projects where sessions may start |
| `claude_sessions_list` | Sessions, newest first, optionally by project |
| `claude_session_get` | Status, revision, last assistant text; optionally waits up to 45 seconds and returns the turn's result |
| `claude_session_read` | Transcript pages; item text is capped at 8,000 characters |
| `claude_session_start` | Start a background session in a project |
| `claude_session_send` | Continue an idle background session; refuses a `waiting` one unless `interrupt` |
| `claude_session_stop` | Stop a background session and keep its conversation |

A typical flow is `start`, then `claude_session_get` with `afterRevision`,
`waitSeconds`, and `includeResult` until `timedOut` is false. Clients that
support MCP Events can instead subscribe to `claude.session.turn_finished`
([MCP Events](mcp-events.md)) and read the result once it fires. The polling
tools remain for every other client. Interactive terminal sessions can be read but
not sent input; reach those through Remote Control.

## Turn notifications

A caller that starts sessions can have Omni tell it when each turn ends,
without polling and without hearing about sessions it did not start.
`claude_session_start` takes an optional `notify`: the id of a stored event
target (`chief`, `business`; see "Event journal" in
[MCP Events](mcp-events.md), managed on `/gateway`; the old
`CLAUDE_NOTIFY_TARGETS` entries are imported at boot). Omni stores the target
with the session (`mcp-claude-session-notify`), so every later turn notifies
too.
`claude_session_send` takes `notify` to change the target, `null` to clear it,
and keeps it when the field is left out. Both results report the session's
target and whether it is stored. A name with no stored target is kept with a
warning in the log and never fails the call; its turns wait as
`unconfigured` and are sent if the target is created within seven days.

A send to a `waiting` session is refused unless `interrupt` is true. The host
continues a background session by stopping its process and resuming it with
the new input, so a send to a paused turn would silently kill it and the
command it was running (this happened to two sessions on 2026-11-23 before
the check). The host itself refuses only `busy` sessions.

Each finished turn, found by the same rule as `claude.session.turn_finished`
(see [MCP Events](mcp-events.md#claudesessionturn_finished)), is queued once
per session and revision as a notice in the event journal and posted as JSON
(`mcp-claude-turn-notification` rows are the old outbox; a boot step moved
their pending rows into the journal):

```json
{"event": "claude.session.turn_finished", "sessionId": "...", "shortId": "80f00d5b",
 "project": "omni", "title": "...", "status": "idle", "state": "done", "revision": 25,
 "blocked": false, "waitingFor": null, "finishedAt": "2026-11-12T21:15:29.804Z",
 "summary": "<last reply, at most 1,500 characters>", "summaryTruncated": false}
```

A turn paused on a prompt is posted once with `"status": "waiting"`,
`"blocked": true` and `waitingFor` (null when the host does not name it); its
summary is the last text so far, not a final report. Its later end is posted
as usual.

Targets are Rakazo bot webhooks (`https://<rakazo>/api/v1/bots/<botId>/webhook`):
the request carries `Authorization: Bearer <key>` and
`Idempotency-Key: omni-claude-turn:<sessionId>:<revision>`, which Rakazo uses to
collapse repeated deliveries into one run. Production targets use
`connect: npm.compose_default`: Nginx Proxy Manager admits Rakazo only from
the LAN and the `compose_default` network, and Omni's default route to the LAN
address leaves through `compose_agent-integrations`, which NPM refuses with 403.
Delivery, retries (a day of them, 30 seconds doubling to 30 minutes), dead
letters and retention are the journal's ([MCP Events](mcp-events.md)), and
run even while the host is offline. `events_status` reports the target names
(never URLs or keys), counts and the latest outcomes under
`claudeNotifications`. The summary passes the same host scrub as tool results.

Omni logs each forwarded command with its session or project, duration, and
outcome code, never prompt text. The `/claude` page shows the link state, the
Mac's live sessions with transcripts, and every `claude_*` call grouped by
session, including prompts and results (bounded, see [MCP activity](mcp.md#activity)).
Its live panels query the Mac read-only through the same link.

Sessions started through `claude-for-dot` are background `claude --bg` processes.
They do not count against the `capacity` of the project's `claude-rc` Remote
Control server, which counts only sessions it spawned itself.
