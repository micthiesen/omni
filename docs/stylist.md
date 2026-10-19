# Stylist

The `omni-stylist` subsystem is a fit-first personal stylist ledger: a
structured, case-based record of one person's body, garments, purchases, fit
outcomes and preferences, with a versioned runbook that tells any assistant
how to use it. Writes and private evidence go only through authenticated MCP
tools; the LAN web UI has a read-only `/stylist` page (see "Web page"). There
is no scheduled message, no shopping integration and no receipt scanning.

The ledger is the source of truth. Assistants read it at the start of every
session (`stylist_context_get`) instead of relying on chat memory, and write
plain-language feedback back as typed events.

## Design

- **Records** (`stylist_record_save`) have kinds `variant` (a product as sold:
  brand, model, size label and system, cut, colour, fabric, version),
  `piece` (one owned garment, linked to its variant, optionally a sizing
  reference), `measurement` (body, piece or a variant's published chart),
  `profile_snapshot`, `preference`, `prediction`, `rule` and `need`.
  Variants, pieces, preferences, rules and needs are revised with optimistic
  concurrency (`id` + `expectedRevision`) and every revision is kept.
  Measurements, profile snapshots and predictions are immutable; a correction
  is a new record with `supersedes` and a `correctionReason`.
- **Events** (`stylist_event_append`) are immutable: `acquired`, `try_on`,
  `wear`, `wash`, `alteration`, `decision`, `returned`, `retired`,
  `feedback`, `outcome`, `contradiction`, `note`. A correction is a new event
  of the same type with `supersedesEventId`. A piece's status (`owned`,
  `undecided`, `return_pending`, `returned`, `retired`) is derived from its
  lifecycle events.
- **Provenance** on every observation record (variant, piece, measurement,
  profile snapshot, preference, need) and event: `certainty` (`explicit`,
  `inferred`, `unknown`), `source`, `observedOn`, evidence ids. An event
  without provenance is stored as an explicit user statement. Predictions and
  rules carry their evidence ids and basis instead. The server records
  `recordedAt` and the ledger revision.
- **Measurements** keep the original value and unit and add `valueMm`. They
  distinguish body from finished garment, flat width from circumference,
  linear and curve lengths, endpoints, stretch and wash state, wash count and
  tolerance. A body flat width is rejected. `stylist_measurements_compare`
  compares only the same dimension, method and subject class and lists every
  other pair as incompatible without converting it.
- **Predictions** freeze a forecast before any outcome: the exact variant and
  size, per-region expectations, likely failure mode, evidence gaps,
  references and rules used, return risk, the context it was made from, and
  the runbook version.
- **Context proof.** Every `stylist_context_get` result carries
  `snapshot.contextToken`: `ctx1.<parameters>.<digest>`, naming the
  namespace, ledger revision, the date it was judged on, the focus, the list
  limit and the runbook version, plus the content digest. A new prediction
  must carry that token (with `contextRevision` equal to its revision). Inside
  the write transaction Omni re-renders the context the token names from the
  ledger at that revision and compares digests; a malformed, altered,
  foreign-namespace or non-reproducible token is rejected with
  `[context_mismatch]`. A genuine but stale token is accepted with a warning
  counting the writes since. The digest leaves out what changes as the ledger
  grows (`generatedAt`, `newestRevision`, runbook text and version list, the
  `historical_snapshot` warning), so passing the token back to
  `stylist_context_get` re-renders the identical context on any later day. A
  deploy that changes the context's shape invalidates outstanding tokens:
  read the context again. Forecasts frozen before tokens existed keep their
  stored `contextRevision` and any client-supplied `contextSha256` in their
  data; neither was verified, and their `derived` has no `context` entry.
- **Basis** (`basisRule: fit-evidence-v4`; v4 stopped counting reports
  without a known region verdict as fit). A piece's fit evidence is `proven`
  (fitting try-ons, wear or feedback with at least one known region verdict
  and none `too_*`, a matched outcome, or a `baseline` reference
  role; comfort, style or value without a known region verdict is no fit
  evidence, and in history it is unresolved, "no regional fit reported"),
  `failed` (a `too_*` region, a wrong-size
  try-on, a missed outcome, a fit-related return, a too-small or too-large
  reference role, or a wash that shrank or stretched it), `mixed`, or
  `untested`, and records whether its latest report (by `occurredOn`, then
  ledger order; a reference role counts at its latest revision) was positive
  or adverse. `exact_repeat` requires a piece of the same brand, model, cut,
  size label and system, the same version (or none recorded on either) and
  the same material, with proven fit and no failed or mixed match;
  `known_brand_new_cut` requires recorded fit evidence for some piece of the
  brand; otherwise the basis is `new_brand_inference`. A stronger claim is
  rejected with `[basis_mismatch]` and the reasons (untested, version or
  material differs with each difference, unrecorded). A compatible piece that
  failed, or whose latest report is adverse after earlier positive ones (it
  fit at try-on, then shrank), is a known failure: a plain `recommend` is
  rejected with `[known_failure]`; `recommend_with_caution` or `abstain` are
  accepted with a warning. A mixed piece whose latest report is positive only
  warns. `derived.basisAssessment` records the matches, material
  differences, latest signals and reasons. Forecasts frozen under earlier
  rules keep the `computedBasis` and `basisRule` they stored.
- **Material.** Two variants have the same material only when both record a
  composition and every fiber is present on both or neither; stretch fibers
  (elastane, spandex, lycra, elastomultiester, elasterell_p, polyurethane,
  rubber) differ by at most 0.5 points and other fibers by at most 3 points
  (label rounding); and construction, recorded stretch (other than
  `unknown`) and `preShrunk` agree wherever both record them. So 100% cotton
  and 95% cotton with 5% elastane differ, and so do 2% and 3% elastane, while
  60/40 and 62/38 cotton/polyester agree. A variant whose material differs
  from an existing one is a separate product and is saved without
  `allowDuplicate`; one with the same material, or with material recorded on
  only one side, is a `[duplicate_variant]` (revise the existing variant to
  add a missing material). A variant something uses cannot change a recorded
  material.
- **Frozen inputs.** The forecast also stores the variant's revision and identity and the
  revisions of the references and rules it used, and a variant that anything
  uses cannot change its brand, model, category, size, cut, colour, version
  or SKU. Events that link a forecast (`acquired`, `try_on`, `outcome`,
  `feedback`) must be dated no earlier than the day before the freeze, must
  use a piece of the forecast's exact variant, and are rejected as
  `[post_hoc]` when that piece's fit or decision was recorded before the
  freeze (superseded reports included). A forecast with any linked event can
  no longer be superseded. `outcome` events (one per stage: `immediate`,
  `after_wear`, `after_wash`, `delayed`) score the frozen expectations per
  region (`exact`, `close`, `miss`, ...) without changing the prediction.
- **Rules** are narrow (a brand, model or category is required) and linked to
  evidence. A rule becomes `active` only when every in-scope fit case has a
  regression verdict and none is inconsistent. Later in-scope cases are listed
  as unchecked, and the rule appears in `rules.needsReview` until it is
  re-checked. Confidence is expressed through the basis tiers and qualitative
  risk, never percentages.
- **Context** (`stylist_context_get`) returns, from one ledger revision: the
  runbook and checklist, the profile and current body measurements, reference
  garments with their newest measurements and wash history, fit history
  (successes, failures, unresolved; a kept piece without a fit assessment and
  a forecast without an outcome are unresolved), brand history, current
  preferences and recent changes, rules, contradictions, warnings, due items
  and open needs. `asOfRevision` reconstructs any earlier state, evaluated on
  the date of that revision's last write so it is reproducible. A piece's
  status folds its lifecycle events in `occurredOn` order, so backfilled
  history lands where it happened.
- **Contradictions** are computed (repeated measurements disagreeing beyond
  the summed tolerances, or 15 mm for the body and 10 mm for garments when a
  tolerance is missing; conflicting fit reports with no wash or alteration
  between them; opposing preferences) or flagged with a `contradiction`
  event. A `contradiction` event with `status: "resolved"` closes a flagged
  one (`resolvesEventId`) or a computed one (listing the ids it named).

## Storage

Tables beside `blobs` in the docstore (`crates/omni-stylist/src/ledger.rs`):
`stylist_namespaces`, `stylist_records` (every revision),
`stylist_events`, `stylist_evidence` and `stylist_idempotency`. Every write is
one `BEGIN IMMEDIATE` transaction: idempotency lookup, validation against the
loaded namespace, insert, namespace revision bump and idempotency receipt.
Each committed record, event or evidence row takes the next ledger revision;
an evidence upload that matches stored bytes only records its receipt.
Reusing an `idempotencyKey` with identical input returns the stored result
(`replayed: true`); with different input it fails with
`[idempotency_key_reused]`. SQLite triggers make every ledger row
append-only and refuse any deletion in `real`; unique indexes allow one
correction per original. Stored bodies must keep decoding: new fields are
added as optional.

Evidence bytes are content-addressed files under
`/data/stylist-private/evidence/sha256/` (`.local/stylist-private` outside
Docker), directories mode 0700 and files 0600, written atomically and
re-hashed on every read. Accepted types: JPEG, PNG, WebP, HEIC/HEIF, PDF,
text, Markdown and JSON, checked against their magic bytes, at most 3,000,000
decoded bytes per upload (the MCP request limit is 4 MiB). Identical bytes
in one namespace return the existing evidence id and its original metadata.
Bytes leave only through `stylist_evidence_get` (one embedded resource) and
`stylist_export` archives; nothing is served over HTTP and no public URL
exists. MCP activity records only identifiers for `stylist_*` calls
(namespace, keys, ids, record kind or event type) and only the `[code]` of an
error; bodies, notes and bytes are stored as `[private]`.

Evidence files are deleted only by name: a rejected upload or restore
removes the files it created, and a fixture purge removes the purged
namespace's own files unless another namespace or a stored backup still
names them. Nothing sweeps the directory, so a database that lost its rows
cannot empty it. The serialization of uploads and deletions is per process;
run the operator restore while no fixture purge is in progress.

## Namespaces and fixtures

Every tool requires `namespace`: `real` is the owner's ledger and
`fixture-<slug>` (lowercase letters, digits and dashes) is disposable test
data. Namespaces never share records: an id from another namespace is
`[not_found]`. Context, backups and the runbook's workflow use only `real`.

For acceptance testing, use a fresh namespace per run, for example
`fixture-acceptance-20261012a`. When finished, call `stylist_fixture_purge`
with `confirmNamespace` repeating the namespace: it writes and verifies a
pre-purge archive with evidence bytes, deletes the namespace (refusing if it
changed after the archive was taken), and removes its evidence files that
nothing else names. `stylist_restore` with that archive id brings the
namespace back. `real` can be neither purged nor restored through MCP, and
archives never cross between `real` and fixtures in either direction.

## Tools

All tools are recommended `allow` in `docs/mcp-policy.json`: they change only
Omni's private ledger and files and have no external effects or cost. The
purge is annotated destructive but limited to fixtures and reversible from
its pre-purge archive.

| Tool | Annotations | Purpose |
| --- | --- | --- |
| `stylist_context_get` | read-only | Session context and runbook (above) |
| `stylist_records_query` | read-only | Exact filters (`kinds`, `ids`, `linkedTo`, `category`, `categoryPrefix`, `brand`, `statuses`, `from`/`to`), `includeSuperseded`, `includeHistory`, `asOfRevision`, `order`, `cursor`/`limit` (max 100) over records, events or evidence |
| `stylist_measurements_compare` | read-only | Compatible deltas in mm with caveats; incompatible pairs kept apart |
| `stylist_record_save` | keyed write | Create, revise or supersede a record |
| `stylist_event_append` | keyed write | Append or correct an event |
| `stylist_evidence_store` | keyed write | Store a private photo or document |
| `stylist_evidence_get` | read-only | Metadata plus verified bytes as an embedded resource |
| `stylist_export` | write (file) | Write and verify an archive; optionally attach it (at most 16 MiB) |
| `stylist_archives_list` | read-only | Stored archives with counts and checksums |
| `stylist_restore` | write | Restore an archive into an empty fixture namespace |
| `stylist_fixture_purge` | destructive | Archive, then delete a fixture namespace |

Errors are tool errors prefixed with a code: `[validation]`, `[not_found]`,
`[revision_conflict]`, `[idempotency_key_reused]`, `[immutable]`,
`[duplicate_variant]`, `[duplicate_receipt]`, `[duplicate_outcome]`,
`[basis_mismatch]`, `[post_hoc]`, `[regression_incomplete]`,
`[incompatible_measurement]`, `[namespace]`, `[integrity]`, `[too_large]`,
`[unsupported_media]`, `[archive]`. Optional input fields are omitted, never
`null`. The full schemas are in `crates/omni-mcp-kit/golden/tools-list.json`.

## Backup and restore

An archive (`omni-stylist-export`, format version 1) is one JSON document
with every record revision, event, evidence row (with base64 bytes unless
excluded), idempotency receipt, derived relationship and runbook version,
plus SHA-256 checksums per section and a `content` checksum that excludes the
namespace name, the export time and the bytes (row timestamps are included).
Verification also checks every runbook text against its digest, that the
rows cover ledger revisions 1..N exactly once, and, for stored archives, the
file digest in the manifest. These checks catch corruption and lost rows;
they are not a signature against someone who can rewrite the file and its
checksums. A restore inserts everything into an empty namespace inside one
transaction and commits only if the restored namespace reproduces the
`content` checksum and every evidence digest matches. An archive whose
written copy fails verification is deleted. Archives hold the whole document
in memory while exporting; that is fine for a personal wardrobe but grows
with the number of photos.

- `StylistBackup` runs hourly at 23 minutes past. When the `real` ledger
  changed since the last backup it writes
  `/data/stylist-private/archives/real-r<revision>-...-backup-*.json` (evidence
  metadata and digests, no bytes) and verifies every evidence file; the 04:23
  run re-verifies an unchanged backup and its evidence files, and replaces a
  backup that fails. Retention keeps the newest 72 backups, 20 other archives
  of `real` and 50 fixture archives, so test churn never evicts an archive of
  `real`. It sends no messages; a failed run is logged and alerted like any
  failed task.
- Kopia snapshots `/home/michael/compose` (including
  `volumes/omni/stylist-private`) to R2 every 20 minutes. The archives
  and evidence files are immutable once written, so the snapshot holds a
  consistent copy; the live SQLite file itself is not snapshot-consistent.
- Restore after losing the database, with the private directory recovered:

  ```bash
  docker exec omni omni stylist-verify \
    --archive /data/stylist-private/archives/<id>.json
  docker exec omni omni stylist-restore \
    --archive /data/stylist-private/archives/<id>.json --namespace real --allow-real
  ```

  `stylist-export --namespace real --out FILE` writes a portable archive with
  bytes to any path. All three accept `--db PATH` and `--private DIR`.

The SQLite connection uses `synchronous=NORMAL` (WAL), shared with the rest
of Omni: a power loss can drop the last committed writes but not corrupt the
file. The recovery point for a lost database is the newest backup, at most
about an hour old while the ledger is changing.

Tested in the suite: export, verify and restore round trips (image bytes,
relationships, superseded records), restoring `real` into a fresh database
from a backup plus the evidence directory, fixture purges on that fresh
database leaving the backup's evidence intact, and the pre-purge restore. Not
tested: recovering the directory from an actual Kopia snapshot, so this is
not yet a demonstrated disaster recovery.

## Web page

`/stylist` in the LAN UI (`http://omni.boris/stylist`) shows one namespace at
a time (`?ns=`, default `real`; fixtures are marked as test data) from
`GET /api/stylist?namespace=` (`omni_api::stylist::StylistOverview`, built in
`crates/omni-stylist/src/ui.rs` from the same context the tools serve, with
no focus and up to 50 cases per history list): due items, warnings and
contradictions, the body profile, the wardrobe with fit evidence and
reference roles, fit history, frozen forecasts with their outcomes, rules,
preferences, needs, archives and the runbook checklist. It is read-only.
Because the LAN UI has no login, the overview carries no evidence bytes,
photos, captions or free-text evidence notes; photos stay behind
authenticated MCP. Measurements and labels are shown as recorded.
`--preview` seeds a synthetic wardrobe into its throwaway database
(`crates/omni/src/preview_stylist.rs`).

## Assistant setup

Every harness reaches the tools through Omni's MCP gateway (`/gateway/mcp`,
see `docs/gateway.md`): Claude Code and Codex with a gateway client token,
ChatGPT through OAuth. Give the assistant this instruction, as a project
instruction, custom instruction or `AGENTS.md` line:

> For any clothing, sizing, wardrobe or styling question, first call
> `stylist_context_get` with `namespace: "real"` and a `focus` for the
> category, brand or candidate, and follow the returned runbook and
> checklist. Treat the ledger as the canonical record: reconcile what I tell
> you with it, and record measurements, purchases, try-ons, decisions, washes
> and feedback with the stylist tools as they happen, never inventing values.
> Suggest useful things proactively, but never buy, contact retailers or sign
> up for anything without my explicit approval for that action.

The current runbook (`crates/omni-stylist/runbook/fit-first-v3.md`, returned
by the context with its checklist) covers the session order, measurement
protocols, body versus garment charts and ease, reference garments, the
recommendation and freeze workflow, shopping-time checks, post-purchase
events, narrow learning, contradictions, proactive suggestions, onboarding
and how to translate plain-language feedback into events. v2 replaced v1's
advice on adding ease to body charts, universal measuring landmarks, median
averaging of disagreeing readings, absolute shrinkage claims, abstaining when
only article measurements are missing, a blanket ban on proactive
suggestions, and treating all earlier conversations as non-evidence. v3
keeps v2's checklist and clarifies two points: a garment chart is compared
with references first and with an estimate of body plus learned ease only as
a labelled, weaker inference (never with the body directly, and ease never
applies to body charts); and the basis rules for material and latest-adverse
failures. Published versions (text and checklist) are frozen by a digest
test; earlier versions stay retrievable with `runbookVersion`, a change ships
as a new version, and predictions record the version they followed.

## Proactive scope

Context lists only genuine items: open needs whose `reviewAfter` has come,
return decisions with their deadlines, outcome follow-ups for frozen
forecasts, and post-wash checks for pieces bought on a forecast. The owner
wants assistants to raise these, and genuine opportunities for open needs,
proactively in the conversation or the owner's own notification channel; the
runbook (v2, section 12) says so. Omni itself schedules no stylist messages.
A suggestion is not permission: buying, contacting retailers and signing up
for external services each need explicit approval, given in the conversation
or by tool policy, never stored in the ledger. Budgets on needs are
preferences and never authorize a purchase.
