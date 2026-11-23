# Pet health watch

`PetTracker` syncs Whisker litter-box weights every ten minutes and then runs
the health watch over the stored readings
(`crates/omni-personal/src/pets/health.rs`, `alerts.rs`). Agents read the same
evaluation through `pets_read` with `resource: "trend"`, and the UI through
`GET /api/pets/health?weeks=1..52` (`omni_api::pets::PetHealthResponse`).

## Readings

Whisker stamps readings without an offset, in UTC: a reading stamped
`04:27:58` was synced at `04:40Z`, before `04:27` local time existed. The
watch parses them as UTC. The scale attributes visits by weight and the cats
are about a pound apart, so every window takes the median, drops readings more
than 1 lb from it, and takes the median again.

## Rules

All windows are rolling and end at the evaluation time.

| Rule | Trips | Clears | Repeats in the same episode |
|---|---|---|---|
| `weight-drop-2w` | last 7 days' median >= 3% below the 7 days ending 14 days earlier, and >= 2% below the 7 days ending 28 days earlier; >= 8 readings in each window | 2-week drop < 2% or 4-week drop < 1% | after 4 weeks, if 2 points worse |
| `weight-drop-90d` | >= 5% below the 7 days ending 90 days earlier; >= 8 readings per window | drop < 4% | after 4 weeks, if 2 points worse |
| `visit-drop` | last 7 days' visits <= 50% of the median of the 8 preceding 7-day blocks (median >= 7), and no household silence over 24 h in the last 7 days | ratio > 75% | never |
| `data-gap` | no reading from any pet for 48 h (household-wide, key `*`) | a reading arrives; sends one "readings resumed" note | never |

Between the trip and clear thresholds, or without enough readings, the state
holds. The 4-week confirmation keeps a dip that rebounds within a month from
alerting.

## Delivery

One `pet-health-alert` row per `(petId, kind)` stores `active`, `notified`,
`episodeStartedAt`, `lastNotifiedAt`, `lastValue`, `lastMessage`,
`recoveredAt` and the last push's status. It is the only throttle: a new
episode pushes once, and never within 7 days of the previous push for that pet
and rule; an episode that starts inside that week pushes when the week ends if
it is still open. Each pass decides inside a write transaction and records the
push as `sending` before calling Pushover (General channel, link
`http://omni.boris/pets`). A definite rejection restores the previous row so the
next pass retries; an uncertain outcome keeps the reservation and is never
resent. Without Pushover configured, the rows are not touched.

While the household has no reading for 48 h, the run fails with
`No litter-box readings for N h (latest ...)`. `PetGapAlertGate` keeps that run
failure off the ERROR-alert Pushover path while the gap row is active, so the gap
pushes through one layer. A gap the ledger did not record alerts at once; any
other PetTracker failure alerts only after 30 minutes of consecutive failed runs
(`SUSTAINED_FAILURE`), so one Whisker network blip stays a failed run.

## Dismissing a finding

A long episode (Sandy's 90-day decline stays one episode) would otherwise stay
a warning on `/pets` and in Home's "pet health alerts" count. Each per-pet
finding row has a Dismiss button (`POST /api/pets/health/dismiss` with
`{petId, kind}`; `POST /api/pets/health/restore` undoes it). The `data-gap`
finding cannot be dismissed: it ends when readings resume.

A dismissal is one `pet-health-dismissal` row per `(petId, kind)` holding
`dismissedAt`. It hides that rule's current episode only:

- The rule's clear signal deletes it on the next PetTracker pass, so the next
  episode starts undismissed. This works without Pushover, when no alert rows
  are written.
- An alert row whose `episodeStartedAt` or `lastNotifiedAt` is newer than
  `dismissedAt` lifts it: a new episode, or the repeat push of a worsening weight
  episode (4 weeks on, 2 points worse), needs attention again. PetTracker then
  deletes the superseded row.

Dismissed findings stay in `GET /api/pets/health` and `pets_read` with
`dismissedAt` set; the page shows them muted with a Restore button and leaves
them out of the headline, the warn colouring and Home's count. Dismissal never
changes the `pet-health-alert` rows or Pushover delivery.

## Calibration

`OMNI_PROD_COPY=<copy> cargo test -p omni-personal --test it
prod_copy::health_rules -- --ignored --nocapture` replays the rules hourly from April over
a production copy with in-memory state and prints every push. On the
2026-10-09 copy (Sam 531 readings, Sandy 631, 2026-03-20 to 2026-10-09) it
sends:

| When (UTC) | Pet | Rule | Value |
|---|---|---|---|
| 06-20 09:00 | Sam | weight-drop-90d | -5.6% (14.71 -> 13.88 lb) |
| 06-24 05:00 | Sam | visit-drop | 8 visits vs usual 16.5 |
| 08-07 07:00 | Sandy | visit-drop | 13 vs usual 26.5 |
| 08-31 15:00 | Sam | weight-drop-2w | -3.1% / 2w, -4.4% / 4w |
| 09-13 23:00 | Sandy | weight-drop-90d | -5.2% (13.54 -> 12.84 lb) |
| 09-25 09:00 | household | data-gap | silent since 09-23 08:20 |
| 09-26 01:00 | household | resumed | after 2.7 days |
| 10-02 09:00 | household | data-gap | silent since 09-27 23:21 (capped until a week after 09-25) |
| 10-04 21:00 | household | resumed | after 6.9 days |
| 10-08 07:00 | Sam | weight-drop-2w | -3.0% / 2w (13.85 -> 13.43 lb), -2.5% / 4w |

Sandy's decline (-8.7% over 90 days by 10-09) alerts once and stays one
episode. Without the 4-week confirmation the 2-week rule also fired for Sandy on
05-23 (-3.03%, a dip that recovered by June); without visit hysteresis Sandy's
visit drop fired twice in August. The 2-week drop never exceeded 3.21% for
either cat over the history, so the current Sam decline sits just above
threshold, as do the 08-31 and 05-23 episodes; the confirmation rule separates
them.
