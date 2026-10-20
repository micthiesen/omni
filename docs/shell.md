# Shell

`shell` runs a bash script in a sandboxed virtual shell over Omni's read-only
tools and returns only what the script prints. It is an experiment in the
virtual-filesystem style of agent interface (AgentVFS, VFS.md): instead of
loading every tool schema and passing each full result through the model, an
agent filters tool results with `jq`, `grep` and friends inside one call. It
is served beside the ordinary tools; nothing else changed. `shell_usage`
measures whether it saves context.

The code is `crates/omni-mcp/src/tools/shell/`.

## What a script sees

The shell is [bashkit](https://github.com/everruns/bashkit), an in-process bash
interpreter written in Rust with about 120 reimplemented commands (`jq` through
the embedded jaq engine, `grep`, `rg`, `sed`, `awk`, `sort`, `find`, ...). It
runs no host process and reaches no host file or network: the `curl`, `git`,
`ssh`, Python, SQLite and host-filesystem features are not compiled in.

- **Commands.** Every read-only Omni tool except `stylist_*` is a command named
  after the tool. `email_search --from amazon --limit 5` runs it with
  `{"from":"amazon","limit":5}` and prints its result as indented JSON. Flags
  are the input fields in kebab-case or camelCase, typed by the input schema;
  array flags repeat; a bare boolean flag means true; one JSON object argument
  (or a JSON object on stdin, with no flags) is the whole input. `tools` lists
  the commands and `<tool> --help` shows a tool's flags. A failing tool prints
  `<tool>: <message>` on stderr and exits 1 (2 for bad arguments).
- **Files.** Every command that needs no arguments is also
  `/omni/<tool>.json`: its result with default arguments, read from the tool on
  the first read or `stat` in a call and reused for the rest of that call.
  Listing `/omni` reads nothing, so sizes show 0, but a glob over every file
  runs every argument-free tool. `/omni` is read-only.
- **Scratch.** `/tmp` is the working directory and `HOME`, in memory, at most
  16 MiB per shell and 8 MiB per file.
- **`/README.md`** describes the layout and lists the files and commands.

Calls go to the tool's own handler with the gateway's input defaults and
output projection, so a command prints exactly what a direct call returns.
They are not separate MCP calls and do not appear in MCP activity; the shell
run's record lists them.

Write tools are not commands. Stylist tools stay out because their activity
records keep only identifiers, and a script is stored in its run record.

## Limits

- 60 seconds per script (bashkit's limit, with a 5-second margin enforced
  outside it), bashkit's default command, loop and memory limits.
- 200 tool calls per script, at most 4 at once.
- At most 4 scripts run at once across every caller; more wait their turn.
- The response carries up to 60,000 UTF-16 units of stdout and 8,000 of
  stderr; a cut stdout adds a notice to stderr and sets `truncated`.
- `date` follows the app clock in UTC.

## Sessions

Each call gets a fresh shell unless it passes `session` (`[A-Za-z0-9_-]{1,64}`).
A session keeps `/tmp`, the working directory, variables and functions between
calls; tool data is never carried over, since `/omni` is read anew in every
call. Sessions end after 30 idle minutes; at most 16 are kept, least recently
used first out (a session with a running script is never evicted). Calls in one
session run one at a time. A session whose last call was dropped mid-script,
or stopped by the outer timeout, is replaced with a fresh shell. Sessions live in
memory and end with the process.

## Measurements

Every run is a `shell-run` row (newest 5,000 kept): the script (first 2,000
UTF-16 units), session, outcome (`ok`, `failed`, `limited`, `error`, or
`interrupted` when the request went away first), exit
code, duration, stdout and stderr bytes before and after the response cut,
the tool calls it made (count, bytes, per tool, and the first 50 in order
with how each was reached, `command` or `file`), and the bytes of tool
results it read. Tool bytes are the compact JSON a direct call would return
(the shell prints it indented), so they compare with direct `resultBytes`.
Scripts are stored as written, so a secret pasted into one is kept with it.

Every MCP call row now also records its result size: `resultBytes` (the text
content a client passes to its model) and `structuredBytes` (the
`structuredContent` JSON). Rows from before the change have neither.

`shell_usage` (`days`, default 14) summarizes both:

- `shell`: runs, sessions, outcomes, median and p90 duration, tool calls,
  `toolBytes` read against `returnedBytes`, their `reduction`, the most-used
  tools and the latest failures with the start of each script.
- `direct`: every other recorded call in the window (activity keeps the newest
  2,000), its result sizes overall and per tool.
- `schemas`: the bytes of every Omni tool's `tools/list` entry against the
  shell's own, and the number of commands.

## Judging the experiment

The shell is worth keeping if agents use it for real work and it costs less
context than the direct calls it replaces, without more failures. Questions
`shell_usage` answers:

1. Does `reduction` stay high across real runs? Close to 0 means scripts only
   `cat` results through; high means filtering happens in the shell.
2. Are failures (`failed`, `limited`, `error`) rare, and are the failing
   scripts agent mistakes the README or help text could prevent?
3. How do typical `returnedBytes` per run compare with `medianResultBytes` of
   the same tools called directly? `shell` returns its output both as text
   and inside `structuredContent`; `resultBytes` counts only the text, and
   `structuredBytes` the structured copy a client may also pass on.
4. How large is `toolsListBytes` against `shellToolBytes`? That is the schema
   cost a shell-only client would avoid on clients that load every schema.

A switch-over would mean offering clients only `shell` plus the write tools,
which is a separate change. Writes stay typed tools until then.
