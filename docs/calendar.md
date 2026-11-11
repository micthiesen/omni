# Calendar

`omni-calendar` has two parts: the email pipeline that turns booking emails
into tracked events, and management of Michael's primary iCloud calendar
through MCP tools and read-only API routes.

## Primary calendar identity

The primary calendar is the single VEVENT collection named "iCloud" in the
CalDAV home set. When the server reports `schedule-default-calendar-URL`, it
must point at the same collection. The resolved collection is pinned durably
(`calendar-primary-pin`) and re-resolved every six hours, after sync errors,
and after a 401 or 403.

- Two or more matches fail closed with `calendar_identity_ambiguous`.
- A server default that names another collection fails with
  `calendar_identity_mismatch`.
- A different, unambiguous match re-pins automatically and logs a warning; the
  pin keeps the previous path hash and `repinnedAt`. There is no manual clear.

`calendar_status` reports the state, whether the calendar is the server
default, writable, sync-capable, and whether the email pipeline writes to it.
It never exposes collection URLs or credentials.

## Mirror and sync

`calendar-primary-resource` mirrors each `.ics` resource with its ETag.
Sync uses RFC 6578 `sync-collection` with the stored token, follows 507
truncation, and fetches changed resources by `calendar-multiget` in batches of
50 (halving a batch that is too large). A rejected token (`valid-sync-token`
403/409) or a server without sync falls back to a PROPFIND ETag diff.

`CalendarPrimarySync` runs every minute and syncs when the mirror is more than
five minutes old (every run while a `calendar.*` MCP Events subscription is
active); reads sync when it is older than 30 seconds or on `fresh`.

## Change feed

Each sync writes `calendar-primary-change` rows with a monotonic sequence key:
`created`, `updated` (with `changedFields` and before/after snapshots) or
`deleted`, and an `origin` of `omni` for Omni's own writes (matched through
`calendar-primary-write-echo`) or `external`. The first sync is a silent
baseline, DTSTAMP-only churn is ignored, and rows are kept for 30 days up to
5,000. `calendar_changes_list` (external changes by default, `origin: any` for
all) and `/api/calendar/changes` (all) page by cursor.

## MCP Events

After each sync, rows past `published_seq` are published as
`calendar.event_changed` (only while subscribed; otherwise the cursor skips
ahead). `CalendarStartingEvents` scans every 30 seconds while a
`calendar.event_starting` subscription is active and fires once per
occurrence, start instant and subscription tuple. Rules and payloads are in
`docs/mcp-events.md`.

## Writes

MCP writes follow one durable path (`calendar-mcp-operation`):

1. Validate and plan the iCalendar bodies against the current resource.
2. Reserve the idempotency key with a fingerprint of the input.
3. Mark each step `sending`, PUT or DELETE with If-Match or If-None-Match,
   and classify the response.
4. Verify by GET and record the echo for the change feed.

A 412 is `version_conflict`; a 403 `no-uid-conflict` is `uid_conflict`. A
transport error or 5xx makes the step `uncertain`: it is reconciled only by
reading the resource, and settles as `not_applied` after ten minutes if the old
content remains (measured from the reservation, so polling
`calendar_write_status` does not postpone it). Uncertain writes are never
resent. A boot step marks
operations interrupted while sending as uncertain and unsent ones as failed. A
`following` split truncates the original with UNTIL, creates a sibling series
(`RELATED-TO;RELTYPE=SIBLING`), and deletes the new series if the truncation
fails. Record mode records writes without sending them.

Invitations (another organizer) are read-only. The role is taken across the
master and every override, so one occurrence with guests counts. Events Michael
organizes with attendees are written only with `attendeeNotifications: "send"`, because iCloud
emails attendees. Scheduling writes beyond that are not implemented.

## Email pipeline updates

The pipeline never overwrites a calendar event wholesale. It GETs the event,
merges only the groups whose extracted values changed (title, time, location,
notes, recurrence, and its own alarm), and PUTs with If-Match. A 412 re-reads
and merges again, up to three attempts.

An update never recreates a missing event. On a 404 it looks the UID up, by
reading only, in the account's other event calendars: a time-range
`calendar-query` around the event's date per collection beside the write
calendar (iCloud answers a UID `prop-filter` with 412), each match confirmed
by its own `UID` line on the trusted iCloud host. A copy found there means
Michael moved the
event to another calendar; the merge targets that copy in place. No copy means
the event was deleted (or moved to another calendar and another date, which
makes it Michael's to manage): nothing is written and its tracked row is
tombstoned. "Deleted" is concluded only while the account still lists the
write calendar; a stale or removed calendar is an error, so its 404s never
tombstone anything. A calendar that refuses the query (4xx) is skipped; a
5xx or transport failure is an error for the retry queue, never a guess.

## Deleted events stay deleted

A tracked row (`calendar-created-event`) is retired with `status: cancelled`.
`deletedAt` is added when the calendar event itself was deleted: by
`calendar_event_delete` of the whole event (stored in the same transaction as
the confirmed operation, including one settled later by reading after a lost
response, so a delete is never confirmed without its tombstone), or when an
update finds it gone. Deleting one occurrence or truncating a series is a PUT
and leaves the series tracked. Only the event's active row is tombstoned; rows
an earlier reschedule left behind describe other times and stay cancelled.
Record mode never tombstones.

A create is suppressed only for the exact event (same normalized title, start
date and start time):

- an active row: `duplicate, skipped`;
- a row with `deletedAt`: `deleted from the calendar, not recreated`, for any
  email;
- a row cancelled or moved away by an email: suppressed only when the email
  being processed is one that wrote the row (`emailId` or
  `earlierEmailIds`, a replay), so it cannot undo a later cancellation or
  reschedule. Any other email may book that slot again.

A different date or time is a different event and is created as usual; a
tombstoned row is never a reschedule target. A create that would have moved a
tracked event which turns out to be retired or deleted creates the new time
as its own event. An explicit update of such an event is skipped. Event writes of one pipeline
are serialized, so the dispatcher, the retry task and a manual reprocess
holding the same email create an event once.

A create for a title that matches exactly one active tracked event updates that
event instead of creating a duplicate when it is on the same day at another
time (non-recurring), or on another date within 45 days and the email mentions
a reschedule, postponement, or new date or time. An email that lists the same
title more than once never reschedules (it describes several bookings).

## API

`omni-api::calendar` DTOs back `GET /api/calendar/status`,
`/api/calendar/events?from&to`, and `/api/calendar/changes?cursor&limit`.
