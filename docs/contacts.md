# Contacts

`omni-contacts` reads and edits the owner's iCloud contacts over CardDAV and
manages contact groups (the lists iOS shows in Contacts). It uses the same
`ICLOUD_USERNAME` and `ICLOUD_APP_PASSWORD` as iCloud mail and calendar: an
app-specific password already covers contacts, so no new credential exists.
No task polls iCloud; contacts are read only when a `contacts_*` tool runs.

## Reach

Discovery follows RFC 6764 and RFC 6352 from `https://contacts.icloud.com/`
(which redirects to the account's `pNN-contacts.icloud.com` shard) through the
current-user principal and `addressbook-home-set` to every collection whose
resource type is an address book. "Every book" means every address book of
that one iCloud account. Contacts in other accounts on the phone (Google,
Exchange, other CardDAV servers) and contacts stored only on a device are not
reachable.

Credentials go only to `contacts.icloud.com` and its `pNN-contacts` shards over
HTTPS; any other host in a redirect or href is refused before a request is
sent. Requests are bounded (30 s, 16 MiB per multistatus, 8 MiB per card).

Ids are `<bookId>/<resource>` (for example `card/0D5D….vcf`). A resource
segment may not decode (after percent escapes) to `.` or `..`, a slash or a
backslash, and the resolved URL must be exactly a direct child of its book.

## Index

Search and group listings use an in-memory index per book. A read refreshes it
when it is older than 30 seconds, after a write, or on `fresh`: a Depth 0
PROPFIND compares the book's CTag, and when it changed a Depth 1 ETag listing
names the cards to fetch by `addressbook-multiget` in batches of 50, halved
when an answer is too large or not UTF-8. A card that is too large, not UTF-8,
not a single vCard, has content outside `BEGIN:VCARD`/`END:VCARD`, or is left
out of a multiget answer is counted as unreadable (`unreadableCount`), never
silently dropped. The index keeps projections only (no photo bytes) and is not
persisted.

## Lossless cards

A card is kept as its logical lines with their exact source bytes (folding
and line terminators included). Unedited cards serialize byte for byte. An
edit replaces, removes or inserts only the lines it names; a changed value
keeps its line's group, name and parameter text, and a changed label keeps
every other parameter as written. Structured values (N, ORG, ADR) change only
the named components; the others keep their raw text, so an unescaped comma
in a street stays a list separator. Values are addressed by line id (a digest
of the line and its position among identical lines, valid with the etag it
was read with); a single field that appears twice, two edits of one line, or
a label shared by two values in one `itemN` group is refused. Changing an IM
or social profile value is refused (its service metadata would go stale);
remove it and add a new one with `service`. Only vCard 3.0 cards (what iCloud
stores) are edited.

Labels follow Apple's encoding: TYPE parameters for the standard phone, email,
address and URL labels, and an `X-ABLabel` line in the value's `itemN` group
for custom text and Apple's built-in tokens (`_$!<Other>!$_`). Reads report
the label, whether it is custom, and the raw token (`labelToken`), so an
unknown built-in can be copied exactly. Echoing a label as read changes
nothing. A birthday without a year is Apple's `X-APPLE-OMIT-YEAR=1604` form,
shown as `--MM-DD`.

Every plan is checked before it is sent: the planned card must show each
requested field and value, every other field and value as before, and the
same set of other properties (less the label lines of a removed value).

## Writes

Write tools take an `idempotencyKey`. A write is planned against cards read
fresh by GET (strict UTF-8, with their ETag), reserved durably
(`contacts-mcp-operation`, which keeps ids, states, property names and
digests, never card values), then sent one conditional request at a time:
If-Match for existing cards (with the GET's ETag exactly as received),
If-None-Match for creates. A card whose ETag is weak is never written
(`weak_etag`): a weak ETag cannot guard a conditional write. ETags are
compared ignoring surrounding quotes, never ignoring `W/`. The write lock is an
owned guard held by the detached write, so a caller that goes away cannot let
another write or a replay of the same key start before it settles; a replay
of a record still marked reserved is refused as `in_progress`.

Each write is verified by reading the card back and comparing every line
digest (grouped lines include their siblings, so a label moved to another
value counts as a change) with the plan:

- `verified` only when the read shows the planned lines and every unedited
  line.
- `unverified` when the read still shows the planned-against version (the
  write is not visible yet), or when the PUT answered with a strong ETag and
  the read carries a different strong ETag (a later version): another client
  changed the card, so differences cannot be attributed to this write.
- `diverged` for any other mismatch. The tool fails with
  `data_not_preserved`, every later write of the call is skipped, and the
  write has applied: nothing is rolled back. A PUT answered with no ETag or a
  weak one never excuses a mismatch, because RFC 6352 (6.3.2.3) lets a server
  that stored other octets than were sent answer exactly that way.

A request whose answer was lost is `uncertain`. It is never resent. It
settles by reading: applied when the card shows the write, not applied only
when the card is still provably the version it was planned against (same
ETag, or a create whose card still does not exist) ten minutes after the
reservation, and otherwise it stays uncertain. A boot step marks writes
interrupted while sending as uncertain and unsent ones as failed.

Before a card is overwritten or deleted, its text is kept privately as a
`contacts-recovery-copy` row for 30 days. The copy of a write that verifies
is dropped; the copies of diverged, unverified and uncertain writes, and of
every deleted group, stay. No tool reads them; they exist for manual recovery.

## Groups

iCloud groups are vCards with `X-ADDRESSBOOKSERVER-KIND:group` that list
members as `X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:<contact UID>` (vCard 4
`KIND`/`MEMBER` groups are read too). Groups hold members of their own book
only. Membership changes rewrite only group cards; contact cards are never
written by them. Groups gaining members are written before groups losing
them, and a refused or uncertain write stops the rest, so a move never
leaves a contact in no group. A call in which one group both gains and loses
members while other groups change too (a swap or a chain of moves) is refused
as `split_required`: send the additions first, then the removals. Deleting a
group deletes the list only.

There is no special archive feature: an archive is an ordinary group the
owner creates and moves contacts into. Contacts cannot be deleted through
these tools.

## Limits

- Verification covers the vCard only. Data iOS may keep outside the vCard,
  such as Contact Poster assets, is not visible to CardDAV, so its
  preservation is not verified. A PHOTO stored as a URL is kept as that URL.
- No live iOS canary has been run: preservation has been tested on synthetic
  Apple-style cards only. A canary needs the owner to create a throwaway
  contact with a photo and poster on the phone and authorize editing it.
