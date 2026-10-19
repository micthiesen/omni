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

Sessions intentionally run with the same full access as `claude-for-dot`
(`--dangerously-skip-permissions`), so a background session never stalls on a
permission prompt. Approval happens before the session runs instead, in the connected client:
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
| `claude_session_send` | Continue an idle background session |
| `claude_session_stop` | Stop a background session and keep its conversation |

A typical flow is `start`, then `claude_session_get` with `afterRevision`,
`waitSeconds`, and `includeResult` until `timedOut` is false. Clients that
support MCP Events can instead subscribe to `claude.session.turn_finished`
([MCP Events](mcp-events.md)) and read the result once it fires. The polling
tools remain for every other client. Interactive terminal sessions can be read but
not sent input; reach those through Remote Control.

Omni logs each forwarded command with its session or project, duration, and
outcome code, never prompt text. The `/claude` page shows the link state, the
Mac's live sessions with transcripts, and every `claude_*` call grouped by
session, including prompts and results (bounded, see [MCP activity](mcp.md#activity)).
Its live panels query the Mac read-only through the same link.

Sessions started through `claude-for-dot` are background `claude --bg` processes.
They do not count against the `capacity` of the project's `claude-rc` Remote
Control server, which counts only sessions it spawned itself.
