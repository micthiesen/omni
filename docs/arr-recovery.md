# Sonarr / Radarr recovery

`ArrRecovery` runs every five minutes and observes once at startup. It reuses
`SONARR_URL`/`SONARR_API_KEY`, `RADARR_URL`/`RADARR_API_KEY`, OpenAI, and the media
recommendations Pushover token (falling back to the general token). Set
`ARR_RECOVERY_ENABLED=false` to disable it. No acquisition root/profile settings
are needed. Runs, decisions, errors, and Luna costs appear in Omni's existing
activity and cost views.

## When it acts

A download must have an explicit Arr import rejection, zero remaining bytes,
and a settled warning/error state. Normal downloading, queued, paused, repair,
unpack, and import work is excluded. The same failure must persist across at
least two observations spanning 15 minutes. Progress, changed diagnostics, queue
absence, or a gap over 20 minutes starts a fresh wait. Manual runs use the same
guards; there is no force bypass.

Queue rows are grouped by download ID so a season pack is one operation. Immediately
before mutation, the job re-fetches the queue, preview, grab history, and target
metadata; changed evidence cancels that attempt. Source files must be within the
reported download directory, disjoint from the target library directory.

- Exact title/episode mappings with matching grab history and rejection-free
  previews are imported through Arr's `ManualImport` command.
- Luna (`openai:gpt-6-luna`) assesses ambiguous names, aliases, and obfuscated
  filenames using only the supplied metadata. Its output is schema-validated;
  it cannot invent IDs, change paths, override media/permission errors, or bypass
  the exact Arr mapping. Unmapped/contradictory cases remain for inspection.
- Inferior duplicates are removed only when every intended item already has a
  file and every preview file has a quality/custom-format downgrade rejection.
- No-files rejections require an independent terminal NZBGet health failure
  before deletion/replacement. `NZBGET_URL` enables this optional read-only check
  for a trusted local unauthenticated NZBGet API. Active NZBGet jobs, category or
  path mismatches, missing history, and inaccessible services cannot corroborate it.
- Unresolved cases are recorded and assessed again after 24 hours, or when the
  queue failure changes. At most five Luna assessments and 60 new actions run per
  service per pass. A Luna call has a 60-second timeout and no automatic retries.

## Removal, searches, and delivery

All deletions go through Arr with `removeFromClient=true`; the job never deletes
library files or directly runs filesystem deletion. Bad releases are blocklisted.
Arr's automatic re-search is suppressed; Omni verifies the queue entry and source
directory disappeared, then submits one targeted search for still-missing,
monitored items. Existing replacements prevent another search. Search submission
is asynchronous: the action says requested, not downloaded. Replacements have a
six-hour backoff and a maximum of three attempts per overlapping target in seven days.

Import verification checks the target's linked episode/movie file against the
source filename/scene name. A successful command response alone is insufficient.
Removal verification requires a readable parent directory. With read-only download
mounts and `ARR_RECOVERY_LOCAL_FILES=true`, Omni can verify even an empty parent.
Without those mounts, the Arr API fallback conservatively leaves an empty parent
unverified because its API conflates empty and inaccessible directories.

State is stored in `arr-recovery-state` via Docstore. A per-service 25-minute lease
protects the 20-minute bounded run across processes. Mutations are reserved before
HTTP calls. On uncertain responses/restarts, the next pass reconciles actual state
rather than submitting the mutation again. An unknown search submission requires
inspection rather than risking a duplicate download. Completed action history is
kept for 30 days; unresolved attempts are retained.

Pushover batches report completed actions and uncertain outcomes. Delivery is
reserved before sending; confirmed HTTP 4xx rejections are retried next run, while
an unacknowledged notification is not blindly resent.
This avoids duplicate messages at the cost of requiring inspection of a reserved
notification if the process dies during delivery. The durable task/action records
remain available.

## Boris deployment

The running Omni container already has Arr credentials. Add
`NZBGET_URL=http://nzbget:6789` to its environment for terminal health checks.
NZBGet mounts `/var/tmp/nzbget` at `/tmp`; completed files are under
`/media/storage/nzbget/completed`. Both Arr containers need this additional narrow
bind mount to delete parked failed-download data through their native cleanup:

```yaml
- /var/tmp/nzbget/inter:/tmp/inter
```

Without that mount, Arr cannot remove files still in NZBGet's temporary directory.
NZBGet's history deletion alone does not guarantee removal of that directory.
For unambiguous deletion verification, set `ARR_RECOVERY_LOCAL_FILES=true` in Omni
and add these **read-only** mounts to Omni:

```yaml
- /media/storage/nzbget/completed:/media/storage/nzbget/completed:ro
- /var/tmp/nzbget/inter:/tmp/inter:ro
```

Apply Compose changes only to `sonarr`, `radarr`, and `omni`.

After deployment, inspect the `ArrRecovery` task and allow the normal observation
window. Verify imported episode-file links and paths, removed NZBGet history/data,
and replacement search commands. A queue may still contain healthy active downloads.
