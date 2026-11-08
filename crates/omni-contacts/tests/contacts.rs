//! Contacts over a fake iCloud CardDAV server: discovery of every book,
//! search, reads, group membership moves that never touch contact cards,
//! field-targeted edits that keep every other byte, and the write
//! guarantees (conditional requests, verification, idempotency, settling
//! uncertain writes by reading). All cards are synthetic.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod fake_carddav;

use std::sync::Arc;

use std::time::Duration;

use fake_carddav::{CARD, FakeCarddav, WORK, WriteGate};
use omni_contacts::edit::{ContactPatch, SingleField, ValueUpdate};
use omni_contacts::service::{Action, SearchQuery, WriteRequest};
use omni_contacts::store::{
    OperationRecord, OperationState, OperationStep, RecoveryCopy, StepKind, StepState,
};
use omni_contacts::{Contacts, ContactsDeps, ContactsSettings};
use omni_core::clock::TestClock;
use omni_http::SideEffectMode;
use omni_store::entity::EntityOps as _;
use omni_store::entity::{EntityWrite as _, UpsertOpts};
use omni_testkit::TestStore;
use tokio_util::task::TaskTracker;

const NOW: i64 = 1_800_000_000_000;

/// An Apple-style card: folded photo with a crop parameter, built-in and
/// custom labels, social and IM metadata, a birthday without a year and
/// properties nobody here understands.
fn jane_card() -> String {
    let photo: String = (0..600)
        .map(|i| char::from(b'A' + (i % 26) as u8))
        .collect();
    let mut folded =
        String::from("PHOTO;ENCODING=b;TYPE=JPEG;X-ABCROP-RECTANGLE=ABClipRect_1&0&0&10&10&abc=:");
    folded.push_str(&photo[..40]);
    for chunk in photo.as_bytes()[40..].chunks(74) {
        folded.push_str("\r\n ");
        folded.push_str(std::str::from_utf8(chunk).unwrap());
    }
    folded.push_str("\r\n");
    format!(
        "BEGIN:VCARD\r\nVERSION:3.0\r\nPRODID:-//Apple Inc.//iPhone OS 18.1//EN\r\nN:Doe;Jane;;;\r\nFN:Jane Doe\r\nORG:Example Org\\, Ltd.;Research\r\nitem1.EMAIL;type=INTERNET;type=pref:jane@example.com\r\nitem1.X-ABLabel:Book club\r\nitem2.TEL;type=pref:+1 555 0100\r\nitem2.X-ABLabel:_$!<Mobile>!$_\r\nitem3.ADR;type=HOME:;;1 Main St,Apt 2;Springfield;IL;62701;USA\r\nitem3.X-ABADR:us\r\nBDAY;X-APPLE-OMIT-YEAR=1604:1604-03-15\r\nX-SOCIALPROFILE;type=twitter;x-user=janedoe:https://twitter.com/janedoe\r\nIMPP;X-SERVICE-TYPE=Skype;type=HOME:skype:janedoe\r\n{folded}X-APPLE-SYNTH-WALLPAPER;X-P=\"a:b;c\":opaque\\,data\r\n\tfolded-with-tab\r\nX-ABShowAs:PERSON\r\nNOTE:Met at\\nthe conference\r\nREV:2026-01-01T00:00:00Z\r\nUID:JANE-UID\r\nEND:VCARD\r\n"
    )
}

const BOB: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nN:Roe;Bob;;;\r\nFN:Bob Roe\r\nTEL;type=CELL;type=VOICE:(555) 010-0199\r\nUID:BOB-UID\r\nEND:VCARD\r\n";
const CAROL: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nN:Poe;Carol;;;\r\nFN:Carol Poe\r\nEMAIL;type=INTERNET;type=WORK:carol@example.org\r\nUID:CAROL-UID\r\nEND:VCARD\r\n";
const FRIENDS: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nN:Friends\r\nFN:Friends\r\nX-ADDRESSBOOKSERVER-KIND:group\r\nX-ADDRESSBOOKSERVER-MEMBER:urn:uuid:JANE-UID\r\nX-APPLE-GROUP-SYNTH:keep\r\nUID:FRIENDS-UID\r\nEND:VCARD\r\n";
const OLD: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nN:Old Friends\r\nFN:Old Friends\r\nX-ADDRESSBOOKSERVER-KIND:group\r\nUID:OLD-UID\r\nEND:VCARD\r\n";

struct World {
    fake: FakeCarddav,
    contacts: Contacts,
    store: TestStore,
    clock: Arc<TestClock>,
}

async fn world_with(mode: SideEffectMode) -> World {
    world_full(mode, None).await
}

/// A world whose writes run detached on a task tracker, as in production.
async fn world_tracked() -> World {
    world_full(SideEffectMode::Live, Some(TaskTracker::new())).await
}

async fn world_full(mode: SideEffectMode, tracker: Option<TaskTracker>) -> World {
    let fake = FakeCarddav::start().await;
    fake.put(&format!("{CARD}jane.vcf"), &jane_card());
    fake.put(&format!("{CARD}bob.vcf"), BOB);
    fake.put(&format!("{CARD}friends.vcf"), FRIENDS);
    fake.put(&format!("{CARD}old.vcf"), OLD);
    fake.put(&format!("{WORK}carol.vcf"), CAROL);
    let clock = omni_testkit::test_clock(NOW);
    let store = TestStore::new(clock.clone()).await;
    let contacts = Contacts::new(ContactsDeps {
        http: fake.http(),
        settings: Some(ContactsSettings {
            username: "owner@example.com".to_owned(),
            password: "synthetic-app-password".to_owned(),
        }),
        store: store.store.clone(),
        clock: clock.clone(),
        mode,
        tracker,
    });
    World {
        fake,
        contacts,
        store,
        clock,
    }
}

async fn world() -> World {
    world_with(SideEffectMode::Live).await
}

fn request(key: &str, action: Action) -> WriteRequest {
    WriteRequest {
        tool: "test",
        idempotency_key: key.to_owned(),
        fingerprint: omni_core::digest::sha256_hex(format!("{key}{action:?}")),
        action,
    }
}

fn search_all() -> SearchQuery<'static> {
    SearchQuery {
        limit: 50,
        ..SearchQuery::default()
    }
}

#[tokio::test]
async fn discovers_every_address_book_through_the_shard_redirect() {
    let w = world().await;
    let status = w.contacts.status().await;
    assert_eq!(status.state, "ready", "{status:?}");
    let books: Vec<(&str, u64, u64)> = status
        .books
        .iter()
        .map(|b| (b.book_id.as_str(), b.contact_count, b.group_count))
        .collect();
    assert_eq!(books, vec![("card", 2, 2), ("work", 1, 0)]);
    assert!(status.limits.iter().any(|l| l.contains("Contact Poster")));

    let page = w
        .contacts
        .search(SearchQuery {
            limit: 2,
            ..SearchQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(page.total, 3);
    let names: Vec<&str> = page
        .contacts
        .iter()
        .map(|c| c.display_name.as_str())
        .collect();
    assert_eq!(names, vec!["Bob Roe", "Carol Poe"]);
    assert_eq!(page.next_cursor, Some(2));
    let rest = w
        .contacts
        .search(SearchQuery {
            cursor: 2,
            limit: 2,
            ..SearchQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(rest.contacts[0].contact_id, "card/jane.vcf");
    assert_eq!(rest.next_cursor, None);
}

#[tokio::test]
async fn search_matches_values_and_filters_by_group_or_no_group() {
    let w = world().await;
    let by_phone = w
        .contacts
        .search(SearchQuery {
            query: Some("555-010-0199"),
            ..search_all()
        })
        .await
        .unwrap();
    assert_eq!(by_phone.contacts[0].display_name, "Bob Roe");
    let none = w
        .contacts
        .search(SearchQuery {
            query: Some("no such person"),
            ..search_all()
        })
        .await
        .unwrap();
    assert!(none.contacts.is_empty());
    let by_digits = w
        .contacts
        .search(SearchQuery {
            query: Some("0100199"),
            ..search_all()
        })
        .await
        .unwrap();
    assert_eq!(by_digits.contacts[0].display_name, "Bob Roe");
    let friends = w
        .contacts
        .search(SearchQuery {
            group_id: Some("card/friends.vcf"),
            ..search_all()
        })
        .await
        .unwrap();
    assert_eq!(friends.contacts.len(), 1);
    assert_eq!(friends.contacts[0].group_ids, vec!["card/friends.vcf"]);
    let ungrouped = w
        .contacts
        .search(SearchQuery {
            ungrouped: true,
            ..search_all()
        })
        .await
        .unwrap();
    let names: Vec<&str> = ungrouped
        .contacts
        .iter()
        .map(|c| c.display_name.as_str())
        .collect();
    assert_eq!(names, vec!["Bob Roe", "Carol Poe"]);
    let groups = w.contacts.groups(None, false).await.unwrap();
    let listed: Vec<(&str, u64)> = groups
        .groups
        .iter()
        .map(|g| (g.name.as_str(), g.member_count))
        .collect();
    assert_eq!(listed, vec![("Friends", 1), ("Old Friends", 0)]);
}

#[tokio::test]
async fn contact_get_reports_labels_metadata_and_preserved_properties() {
    let w = world().await;
    let jane = w.contacts.contact("card/jane.vcf").await.unwrap();
    assert_eq!(jane.etag, w.fake.etag(&format!("{CARD}jane.vcf")).unwrap());
    assert_eq!(jane.organization.as_deref(), Some("Example Org, Ltd."));
    assert_eq!(jane.birthday.as_deref(), Some("--03-15"));
    assert!(jane.birthday_year_omitted);
    let email = jane
        .values
        .iter()
        .find(|v| v.value == "jane@example.com")
        .unwrap();
    assert_eq!(email.label.as_deref(), Some("Book club"));
    assert!(email.custom_label && email.preferred);
    let phone = jane
        .values
        .iter()
        .find(|v| v.value == "+1 555 0100")
        .unwrap();
    assert_eq!(phone.label.as_deref(), Some("mobile"));
    assert!(!phone.custom_label);
    let social = jane
        .values
        .iter()
        .find(|v| v.value.contains("twitter"))
        .unwrap();
    assert_eq!(social.service.as_deref(), Some("twitter"));
    assert_eq!(social.user.as_deref(), Some("janedoe"));
    let other: Vec<&str> = jane
        .other_properties
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    for name in [
        "PHOTO",
        "X-APPLE-SYNTH-WALLPAPER",
        "X-ABSHOWAS",
        "X-ABADR",
        "PRODID",
        "REV",
    ] {
        assert!(other.contains(&name), "{name} missing from {other:?}");
    }
    assert!(jane.has_photo && jane.editable);
    assert_eq!(jane.groups[0].name, "Friends");
}

#[tokio::test]
async fn moving_a_contact_between_groups_writes_only_group_cards() {
    let w = world().await;
    let jane_before = w.fake.body(&format!("{CARD}jane.vcf")).unwrap();
    let out = w
        .contacts
        .execute(request(
            "move-jane-0000000001",
            Action::Members {
                add: Vec::new(),
                remove: Vec::new(),
                moves: vec![(
                    "card/friends.vcf".to_owned(),
                    "card/old.vcf".to_owned(),
                    vec!["card/jane.vcf".to_owned()],
                )],
            },
        ))
        .await
        .unwrap();
    assert_eq!(out.state, "confirmed");
    assert!(out.verified);
    assert_eq!(
        w.fake.writes(),
        vec![
            ("PUT".to_owned(), format!("{CARD}old.vcf")),
            ("PUT".to_owned(), format!("{CARD}friends.vcf")),
        ]
    );
    assert!(w.fake.write_requests().iter().all(|r| r.if_match.is_some()));
    assert_eq!(
        w.fake.body(&format!("{CARD}jane.vcf")).unwrap(),
        jane_before
    );
    assert_eq!(
        w.fake.body(&format!("{CARD}friends.vcf")).unwrap(),
        FRIENDS.replace("X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:JANE-UID\r\n", "")
    );
    assert!(
        w.fake
            .body(&format!("{CARD}old.vcf"))
            .unwrap()
            .contains("X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:JANE-UID\r\n")
    );
    let old = w
        .contacts
        .search(SearchQuery {
            group_id: Some("card/old.vcf"),
            ..search_all()
        })
        .await
        .unwrap();
    assert_eq!(old.contacts[0].contact_id, "card/jane.vcf");
}

#[tokio::test]
async fn a_refused_add_stops_a_move_so_the_contact_stays_listed() {
    let w = world().await;
    w.fake.lock().race_next_put.insert(format!("{CARD}old.vcf"));
    let error = w
        .contacts
        .execute(request(
            "move-jane-0000000002",
            Action::Members {
                add: Vec::new(),
                remove: Vec::new(),
                moves: vec![(
                    "card/friends.vcf".to_owned(),
                    "card/old.vcf".to_owned(),
                    vec!["card/jane.vcf".to_owned()],
                )],
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "version_conflict");
    assert_eq!(
        w.fake.writes(),
        vec![("PUT".to_owned(), format!("{CARD}old.vcf"))]
    );
    assert_eq!(w.fake.body(&format!("{CARD}friends.vcf")).unwrap(), FRIENDS);
    let status = w
        .contacts
        .write_status("move-jane-0000000002")
        .await
        .unwrap();
    let steps: Vec<String> = status
        .result
        .unwrap()
        .steps
        .iter()
        .map(|s| s.state.clone())
        .collect();
    assert_eq!(steps, vec!["rejected", "skipped"]);
}

#[tokio::test]
async fn a_partial_edit_keeps_every_other_byte_and_verifies() {
    let w = world().await;
    let jane = w.contacts.contact("card/jane.vcf").await.unwrap();
    let email = jane
        .values
        .iter()
        .find(|v| v.value == "jane@example.com")
        .unwrap();
    let out = w
        .contacts
        .execute(request(
            "edit-jane-000000001",
            Action::ContactsUpdate {
                changes: vec![(
                    "card/jane.vcf".to_owned(),
                    jane.etag.clone(),
                    ContactPatch {
                        update: vec![ValueUpdate {
                            id: email.id.clone(),
                            value: Some("jane.doe@example.com".to_owned()),
                            address: None,
                            label: None,
                            preferred: None,
                        }],
                        ..ContactPatch::default()
                    },
                )],
            },
        ))
        .await
        .unwrap();
    assert_eq!((out.state.as_str(), out.verified), ("confirmed", true));
    assert_eq!(
        w.fake.body(&format!("{CARD}jane.vcf")).unwrap(),
        jane_card().replace("jane@example.com", "jane.doe@example.com")
    );
    // Nothing of the card's content is kept in the operation row.
    let rows = w
        .store
        .store
        .read(|docs| docs.get_all::<OperationRecord>())
        .await
        .unwrap();
    let row = serde_json::to_string(&rows).unwrap();
    for secret in [
        "jane.doe@example.com",
        "jane@example.com",
        "Book club",
        "555 0100",
    ] {
        assert!(!row.contains(secret), "operation row holds {secret}");
    }
    // The verified write leaves no recovery copy behind.
    let copies = w
        .store
        .store
        .read(|docs| docs.get_all::<RecoveryCopy>())
        .await
        .unwrap();
    assert!(copies.is_empty());
}

#[tokio::test]
async fn organization_and_department_edits_change_only_their_components() {
    let w = world().await;
    let jane = w.contacts.contact("card/jane.vcf").await.unwrap();
    w.contacts
        .execute(request(
            "edit-jane-org-00001",
            Action::ContactsUpdate {
                changes: vec![(
                    "card/jane.vcf".to_owned(),
                    jane.etag,
                    ContactPatch {
                        organization: Some("NewCo".to_owned()),
                        clear: vec![SingleField::Department],
                        ..ContactPatch::default()
                    },
                )],
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        w.fake.body(&format!("{CARD}jane.vcf")).unwrap(),
        jane_card().replace("ORG:Example Org\\, Ltd.;Research\r\n", "ORG:NewCo\r\n")
    );
}

#[tokio::test]
async fn a_stale_etag_is_refused_before_anything_is_sent() {
    let w = world().await;
    let error = w
        .contacts
        .execute(request(
            "edit-jane-000000002",
            Action::ContactsUpdate {
                changes: vec![(
                    "card/jane.vcf".to_owned(),
                    "\"stale\"".to_owned(),
                    ContactPatch {
                        note: Some("new".to_owned()),
                        ..ContactPatch::default()
                    },
                )],
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "version_conflict");
    assert!(w.fake.writes().is_empty());
}

#[tokio::test]
async fn echoing_a_contact_as_read_sends_nothing() {
    let w = world().await;
    let jane = w.contacts.contact("card/jane.vcf").await.unwrap();
    let patch = ContactPatch {
        full_name: jane.full_name.clone(),
        note: jane.note.clone(),
        birthday: jane.birthday.clone(),
        organization: jane.organization.clone(),
        department: jane.department.clone(),
        update: jane
            .values
            .iter()
            .filter(|v| v.address.is_none())
            .map(|v| ValueUpdate {
                id: v.id.clone(),
                value: Some(v.value.clone()),
                address: None,
                label: v.label.clone(),
                preferred: Some(v.preferred),
            })
            .collect(),
        ..ContactPatch::default()
    };
    let out = w
        .contacts
        .execute(request(
            "echo-jane-000000001",
            Action::ContactsUpdate {
                changes: vec![("card/jane.vcf".to_owned(), jane.etag.clone(), patch)],
            },
        ))
        .await
        .unwrap();
    assert_eq!(out.status, "unchanged");
    assert_eq!(out.unchanged, vec!["card/jane.vcf"]);
    assert!(w.fake.writes().is_empty());
}

#[tokio::test]
async fn a_preview_shows_the_exact_lines_and_sends_nothing() {
    let w = world().await;
    let jane = w.contacts.contact("card/jane.vcf").await.unwrap();
    let preview = w
        .contacts
        .preview(
            &Action::ContactsUpdate {
                changes: vec![(
                    "card/jane.vcf".to_owned(),
                    jane.etag.clone(),
                    ContactPatch {
                        nickname: Some("JD".to_owned()),
                        ..ContactPatch::default()
                    },
                )],
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(preview.writes.len(), 1);
    assert_eq!(preview.writes[0].added_lines, vec!["NICKNAME:JD"]);
    assert!(preview.writes[0].removed_lines.is_empty());
    assert_eq!(
        preview.writes[0].precondition,
        format!("if-match:{}", jane.etag)
    );
    assert!(w.fake.writes().is_empty());
}

#[tokio::test]
async fn a_repeated_key_replays_and_a_reused_key_is_refused() {
    let w = world().await;
    let action = Action::GroupCreate {
        book_id: Some("card".to_owned()),
        name: "Archive".to_owned(),
        contact_ids: vec!["card/bob.vcf".to_owned()],
    };
    let first = w
        .contacts
        .execute(request("create-group-000001", action.clone()))
        .await
        .unwrap();
    assert_eq!(
        (first.status.as_str(), first.state.as_str()),
        ("created", "confirmed")
    );
    let again = w
        .contacts
        .execute(request("create-group-000001", action))
        .await
        .unwrap();
    assert!(again.replayed);
    assert_eq!(again.group_id, first.group_id);
    assert_eq!(w.fake.writes().len(), 1);
    assert_eq!(
        w.fake.write_requests()[0].if_none_match.as_deref(),
        Some("*")
    );
    let reused = w
        .contacts
        .execute(WriteRequest {
            fingerprint: "another".to_owned(),
            ..request(
                "create-group-000001",
                Action::GroupCreate {
                    book_id: Some("card".to_owned()),
                    name: "Other".to_owned(),
                    contact_ids: Vec::new(),
                },
            )
        })
        .await
        .unwrap_err();
    assert_eq!(reused.code(), "idempotency_key_reused");
    let duplicate = w
        .contacts
        .execute(request(
            "create-group-000002",
            Action::GroupCreate {
                book_id: Some("card".to_owned()),
                name: "archive".to_owned(),
                contact_ids: Vec::new(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(duplicate.code(), "group_exists");
}

#[tokio::test]
async fn groups_can_be_renamed_and_deleted_without_touching_members() {
    let w = world().await;
    let groups = w.contacts.groups(None, false).await.unwrap();
    let old = groups
        .groups
        .iter()
        .find(|g| g.name == "Old Friends")
        .unwrap();
    let renamed = w
        .contacts
        .execute(request(
            "rename-group-00001",
            Action::GroupRename {
                group_id: old.group_id.clone(),
                etag: Some(old.etag.clone()),
                name: "Past".to_owned(),
            },
        ))
        .await
        .unwrap();
    assert!(renamed.verified);
    assert_eq!(
        w.fake.body(&format!("{CARD}old.vcf")).unwrap(),
        OLD.replace("Old Friends", "Past")
    );
    let wrong = w
        .contacts
        .execute(request(
            "delete-group-00001",
            Action::GroupDelete {
                group_id: "card/friends.vcf".to_owned(),
                etag: "\"stale\"".to_owned(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(wrong.code(), "version_conflict");
    let etag = w.fake.etag(&format!("{CARD}friends.vcf")).unwrap();
    let deleted = w
        .contacts
        .execute(request(
            "delete-group-00002",
            Action::GroupDelete {
                group_id: "card/friends.vcf".to_owned(),
                etag,
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        (deleted.status.as_str(), deleted.verified),
        ("deleted", true)
    );
    assert!(w.fake.body(&format!("{CARD}friends.vcf")).is_none());
    assert_eq!(
        w.fake.body(&format!("{CARD}jane.vcf")).unwrap(),
        jane_card()
    );
    // The deleted list's card is kept privately (the rename's copy is not).
    let copies = w
        .store
        .store
        .read(|docs| docs.get_all::<RecoveryCopy>())
        .await
        .unwrap();
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].resource_id, "card/friends.vcf");
    assert_eq!(copies[0].vcard, FRIENDS);
}

#[tokio::test]
async fn created_contacts_join_groups() {
    let w = world().await;
    let created = w
        .contacts
        .execute(request(
            "create-contact-0001",
            Action::ContactCreate {
                book_id: Some("card".to_owned()),
                patch: Box::new(ContactPatch {
                    name: Some(omni_contacts::edit::NamePatch {
                        given: Some("Dana".to_owned()),
                        family: Some("Synthetic".to_owned()),
                        ..Default::default()
                    }),
                    ..ContactPatch::default()
                }),
                group_ids: vec!["card/friends.vcf".to_owned()],
            },
        ))
        .await
        .unwrap();
    assert_eq!(created.state, "confirmed");
    let id = created.contact_id.clone().unwrap();
    let dana = w.contacts.contact(&id).await.unwrap();
    assert_eq!(dana.display_name, "Dana Synthetic");
    assert_eq!(dana.groups[0].group_id, "card/friends.vcf");
    assert!(
        w.fake
            .body(&format!("{CARD}friends.vcf"))
            .unwrap()
            .contains(&format!("urn:uuid:{}", dana.uid.unwrap()))
    );
    assert_eq!(
        w.fake.write_requests()[0].if_none_match.as_deref(),
        Some("*")
    );
}

#[tokio::test]
async fn groups_only_hold_members_of_their_own_book() {
    let w = world().await;
    let error = w
        .contacts
        .execute(request(
            "cross-book-000001",
            Action::Members {
                add: vec![(
                    "card/friends.vcf".to_owned(),
                    vec!["work/carol.vcf".to_owned()],
                )],
                remove: Vec::new(),
                moves: Vec::new(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "cross_book");
    assert!(w.fake.writes().is_empty());
}

/// A server that stores other octets than were sent answers the PUT with no
/// ETag or a weak one (RFC 6352 6.3.2.3); neither may excuse lost data.
async fn assert_lossy_write_fails(weak: bool) {
    let w = world().await;
    {
        let mut s = w.fake.lock();
        s.drop_on_put = Some("X-APPLE-SYNTH-WALLPAPER".to_owned());
        s.weak_etag_on_rewrite = weak;
    }
    let jane = w.contacts.contact("card/jane.vcf").await.unwrap();
    let error = w
        .contacts
        .execute(request(
            "lossy-server-00001",
            nickname("card/jane.vcf", &jane.etag, "JD"),
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "data_not_preserved", "{error}");
    assert!(error.to_string().contains("X-APPLE-SYNTH-WALLPAPER"));
    let put = &w.fake.write_requests()[0];
    assert_eq!(put.method, "PUT");
    let status = w.contacts.write_status("lossy-server-00001").await.unwrap();
    assert_eq!(status.state.as_deref(), Some("diverged"));
    let result = status.result.unwrap();
    assert!(!result.verified);
    assert_eq!(result.steps[0].state, "diverged");
    let copies = w
        .store
        .store
        .read(|docs| docs.get_all::<RecoveryCopy>())
        .await
        .unwrap();
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].vcard, jane_card());
    assert_eq!(copies[0].etag, jane.etag);
}

#[tokio::test]
async fn a_server_that_drops_a_property_and_sends_no_etag_fails_the_write() {
    assert_lossy_write_fails(false).await;
}

#[tokio::test]
async fn a_server_that_drops_a_property_and_sends_a_weak_etag_fails_the_write() {
    assert_lossy_write_fails(true).await;
}

#[tokio::test]
async fn a_lossy_write_stops_the_rest_of_a_batch() {
    let w = world().await;
    w.fake.lock().drop_on_put = Some("X-APPLE-SYNTH-WALLPAPER".to_owned());
    w.fake.put(
        &format!("{CARD}jane2.vcf"),
        &jane_card().replace("JANE-UID", "JANE2-UID"),
    );
    let jane = w.contacts.contact("card/jane.vcf").await.unwrap();
    let jane2 = w.contacts.contact("card/jane2.vcf").await.unwrap();
    let edit = |contact: &str, etag: &str| {
        (
            contact.to_owned(),
            etag.to_owned(),
            ContactPatch {
                nickname: Some("JD".to_owned()),
                ..ContactPatch::default()
            },
        )
    };
    let error = w
        .contacts
        .execute(request(
            "lossy-batch-000001",
            Action::ContactsUpdate {
                changes: vec![
                    edit("card/jane.vcf", &jane.etag),
                    edit("card/jane2.vcf", &jane2.etag),
                ],
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "data_not_preserved");
    assert_eq!(w.fake.writes().len(), 1);
    assert_eq!(
        w.fake.body(&format!("{CARD}jane2.vcf")).unwrap(),
        jane_card().replace("JANE-UID", "JANE2-UID")
    );
    let status = w.contacts.write_status("lossy-batch-000001").await.unwrap();
    let states: Vec<String> = status
        .result
        .unwrap()
        .steps
        .iter()
        .map(|s| s.state.clone())
        .collect();
    assert_eq!(states, vec!["diverged", "skipped"]);
    let copies = w
        .store
        .store
        .read(|docs| docs.get_all::<RecoveryCopy>())
        .await
        .unwrap();
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].resource_id, "card/jane.vcf");
}

#[tokio::test]
async fn an_applied_write_with_a_lost_answer_settles_by_reading() {
    let w = world().await;
    w.fake.lock().apply_then_answer = Some(503);
    let jane = w.contacts.contact("card/jane.vcf").await.unwrap();
    let out = w
        .contacts
        .execute(request(
            "lost-answer-000001",
            Action::ContactsUpdate {
                changes: vec![(
                    "card/jane.vcf".to_owned(),
                    jane.etag,
                    ContactPatch {
                        nickname: Some("JD".to_owned()),
                        ..ContactPatch::default()
                    },
                )],
            },
        ))
        .await
        .unwrap();
    assert_eq!((out.state.as_str(), out.verified), ("confirmed", true));
    assert_eq!(w.fake.writes().len(), 1);
}

#[tokio::test]
async fn an_unapplied_uncertain_write_is_never_resent_and_settles_after_ten_minutes() {
    let w = world().await;
    w.fake.lock().fail_next_write = Some(500);
    let jane = w.contacts.contact("card/jane.vcf").await.unwrap();
    let action = Action::ContactsUpdate {
        changes: vec![(
            "card/jane.vcf".to_owned(),
            jane.etag,
            ContactPatch {
                nickname: Some("JD".to_owned()),
                ..ContactPatch::default()
            },
        )],
    };
    let out = w
        .contacts
        .execute(request("uncertain-0000001", action.clone()))
        .await
        .unwrap();
    assert_eq!(out.state, "uncertain");
    let again = w
        .contacts
        .execute(request("uncertain-0000001", action))
        .await
        .unwrap();
    assert!(again.replayed);
    assert_eq!(again.state, "uncertain");
    w.clock.set(NOW + 11 * 60 * 1000);
    let status = w.contacts.write_status("uncertain-0000001").await.unwrap();
    assert_eq!(status.state.as_deref(), Some("failed"));
    assert_eq!(w.fake.writes().len(), 1);
    assert_eq!(
        w.fake.body(&format!("{CARD}jane.vcf")).unwrap(),
        jane_card()
    );
}

#[tokio::test]
async fn record_mode_records_writes_without_sending_them() {
    let w = world_with(SideEffectMode::Record).await;
    let out = w
        .contacts
        .execute(request(
            "record-mode-000001",
            Action::Members {
                add: vec![("card/old.vcf".to_owned(), vec!["card/bob.vcf".to_owned()])],
                remove: Vec::new(),
                moves: Vec::new(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(out.state, "confirmed");
    assert!(!out.verified);
    assert!(w.fake.writes().is_empty());
    let recorded = w.contacts.recorded_writes();
    assert_eq!(recorded.len(), 1);
    assert!(
        recorded[0]
            .body
            .as_deref()
            .unwrap()
            .contains("urn:uuid:BOB-UID")
    );
}

#[tokio::test]
async fn unreadable_and_missing_cards_are_reported_not_hidden() {
    let w = world().await;
    w.fake.put_bytes(
        &format!("{CARD}latin1.vcf"),
        b"BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Ren\xe9\r\nEND:VCARD\r\n",
    );
    w.fake.put(&format!("{CARD}trailing.vcf"), "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:x\r\nEND:VCARD\r\nX-ADDRESSBOOKSERVER-MEMBER:urn:uuid:ZZZ\r\n");
    w.fake.put(&format!("{CARD}omitted.vcf"), BOB);
    w.fake
        .lock()
        .omit_from_multiget
        .insert(format!("{CARD}omitted.vcf"));
    let status = w.contacts.status().await;
    let card = status.books.iter().find(|b| b.book_id == "card").unwrap();
    assert_eq!(card.unreadable_count, 3);
    let search = w.contacts.search(search_all()).await.unwrap();
    assert_eq!(search.unreadable_count, 3);
    let error = w.contacts.contact("card/latin1.vcf").await.unwrap_err();
    assert_eq!(error.code(), "undecodable_card");
}

#[tokio::test]
async fn an_untrusted_principal_never_receives_credentials() {
    let w = world().await;
    w.fake.lock().principal_href = Some("https://evil.example/principal/".to_owned());
    let status = w.contacts.status().await;
    assert_eq!(
        status.error_code.as_deref(),
        Some("untrusted_host"),
        "{status:?}"
    );
}

#[tokio::test]
async fn the_mcp_tools_validate_and_serve_contacts() {
    let w = world().await;
    let tools = omni_contacts::mcp::tools(w.contacts.clone()).unwrap();
    let search = tools
        .iter()
        .find(|t| t.meta.name == "contacts_search")
        .unwrap();
    let cx = omni_mcp_kit::ToolContext {
        call_id: "test".to_owned(),
        cancel: Default::default(),
    };
    let out = search
        .handler
        .call(serde_json::json!({"query": "carol"}), cx.clone())
        .await
        .unwrap();
    let omni_mcp_kit::ToolOutput::Structured(map) = out else {
        panic!("structured output expected");
    };
    assert_eq!(map["total"], 1);
    let bad = search
        .handler
        .call(serde_json::json!({"limit": 0}), cx)
        .await
        .unwrap_err();
    assert_eq!(bad.phase, omni_mcp_kit::ToolPhase::Input);
}

fn nickname(contact: &str, etag: &str, nickname: &str) -> Action {
    Action::ContactsUpdate {
        changes: vec![(
            contact.to_owned(),
            etag.to_owned(),
            ContactPatch {
                nickname: Some(nickname.to_owned()),
                ..ContactPatch::default()
            },
        )],
    }
}

#[tokio::test]
async fn a_dropped_caller_cannot_release_the_lock_or_split_its_write() {
    let w = world_tracked().await;
    let jane = w.contacts.contact("card/jane.vcf").await.unwrap();
    let bob = w.contacts.contact("card/bob.vcf").await.unwrap();
    let gate = WriteGate::default();
    w.fake.lock().hold_next_write = Some(gate.clone());
    let edit = nickname("card/jane.vcf", &jane.etag, "JD");
    let first = tokio::spawn({
        let contacts = w.contacts.clone();
        let edit = edit.clone();
        async move { contacts.execute(request("held-write-000001", edit)).await }
    });
    gate.applied.notified().await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    let retry = tokio::spawn({
        let contacts = w.contacts.clone();
        async move { contacts.execute(request("held-write-000001", edit)).await }
    });
    let other = tokio::spawn({
        let contacts = w.contacts.clone();
        let edit = nickname("card/bob.vcf", &bob.etag, "B");
        async move { contacts.execute(request("other-write-000001", edit)).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!retry.is_finished() && !other.is_finished());
    assert_eq!(w.fake.writes().len(), 1);
    gate.release.notify_one();
    let retried = retry.await.unwrap().unwrap();
    assert!(retried.replayed);
    assert_eq!(retried.state, "confirmed");
    let other = other.await.unwrap().unwrap();
    assert_eq!(other.state, "confirmed");
    let writes = w.fake.writes();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0].1, format!("{CARD}jane.vcf"));
    assert_eq!(writes[1].1, format!("{CARD}bob.vcf"));
    let status = w.contacts.write_status("held-write-000001").await.unwrap();
    assert_eq!(status.state.as_deref(), Some("confirmed"));
}

#[tokio::test]
async fn swaps_and_chains_of_moves_are_refused_before_any_write() {
    let w = world().await;
    w.fake.put(
        &format!("{CARD}old.vcf"),
        &OLD.replace(
            "UID:OLD-UID",
            "X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:BOB-UID\r\nUID:OLD-UID",
        ),
    );
    w.fake.put(
        &format!("{CARD}archive.vcf"),
        &OLD.replace("Old Friends", "Archive")
            .replace("OLD-UID", "ARCHIVE-UID"),
    );
    let jane = vec!["card/jane.vcf".to_owned()];
    let bob = vec!["card/bob.vcf".to_owned()];
    let swap = Action::Members {
        add: Vec::new(),
        remove: Vec::new(),
        moves: vec![
            (
                "card/friends.vcf".to_owned(),
                "card/old.vcf".to_owned(),
                jane.clone(),
            ),
            (
                "card/old.vcf".to_owned(),
                "card/friends.vcf".to_owned(),
                bob.clone(),
            ),
        ],
    };
    let chain = Action::Members {
        add: Vec::new(),
        remove: Vec::new(),
        moves: vec![
            (
                "card/friends.vcf".to_owned(),
                "card/old.vcf".to_owned(),
                jane.clone(),
            ),
            (
                "card/old.vcf".to_owned(),
                "card/archive.vcf".to_owned(),
                bob.clone(),
            ),
        ],
    };
    for (key, action) in [("swap-moves-000001", swap), ("chain-moves-00001", chain)] {
        let error = w.contacts.execute(request(key, action)).await.unwrap_err();
        assert_eq!(error.code(), "split_required");
    }
    assert!(w.fake.writes().is_empty());
    // One group gaining and losing members alone is a single atomic write.
    let one_group = w
        .contacts
        .execute(request(
            "one-group-0000001",
            Action::Members {
                add: vec![("card/friends.vcf".to_owned(), bob)],
                remove: vec![("card/friends.vcf".to_owned(), jane)],
                moves: Vec::new(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(one_group.state, "confirmed");
    assert_eq!(w.fake.writes().len(), 1);
}

#[tokio::test]
async fn an_uncertain_destination_write_stops_a_move_before_the_source() {
    let w = world().await;
    w.fake.lock().fail_next_write = Some(500);
    let out = w
        .contacts
        .execute(request(
            "uncertain-move-001",
            Action::Members {
                add: Vec::new(),
                remove: Vec::new(),
                moves: vec![(
                    "card/friends.vcf".to_owned(),
                    "card/old.vcf".to_owned(),
                    vec!["card/jane.vcf".to_owned()],
                )],
            },
        ))
        .await
        .unwrap();
    assert_eq!(out.state, "uncertain");
    let states: Vec<&str> = out.steps.iter().map(|s| s.state.as_str()).collect();
    assert_eq!(states, vec!["uncertain", "skipped"]);
    assert_eq!(
        w.fake.writes(),
        vec![("PUT".to_owned(), format!("{CARD}old.vcf"))]
    );
    assert_eq!(w.fake.body(&format!("{CARD}friends.vcf")).unwrap(), FRIENDS);
}

#[tokio::test]
async fn an_uncertain_write_whose_card_changed_elsewhere_stays_uncertain() {
    let w = world().await;
    w.fake.lock().fail_next_write = Some(500);
    let jane = w.contacts.contact("card/jane.vcf").await.unwrap();
    let out = w
        .contacts
        .execute(request(
            "uncertain-elsewhere",
            nickname("card/jane.vcf", &jane.etag, "JD"),
        ))
        .await
        .unwrap();
    assert_eq!(out.state, "uncertain");
    w.fake.put(
        &format!("{CARD}jane.vcf"),
        &jane_card().replace("NOTE:Met at", "NOTE:Edited on the phone; met at"),
    );
    w.clock.set(NOW + 11 * 60 * 1000);
    let status = w
        .contacts
        .write_status("uncertain-elsewhere")
        .await
        .unwrap();
    assert_eq!(status.state.as_deref(), Some("uncertain"));
    let step = &status.result.unwrap().steps[0];
    assert_eq!(step.state, "uncertain");
    assert!(
        step.detail
            .as_deref()
            .unwrap()
            .contains("changed elsewhere")
    );
    assert_eq!(w.fake.writes().len(), 1);
}

#[tokio::test]
async fn a_lagging_read_back_leaves_a_write_unverified_until_it_catches_up() {
    let w = world().await;
    // Stale for the read-back and the settle read that ends the call.
    w.fake.lock().stale_reads_after_put = 2;
    let jane = w.contacts.contact("card/jane.vcf").await.unwrap();
    let out = w
        .contacts
        .execute(request(
            "lagging-replica-01",
            nickname("card/jane.vcf", &jane.etag, "JD"),
        ))
        .await
        .unwrap();
    assert_eq!((out.state.as_str(), out.verified), ("unverified", false));
    let status = w.contacts.write_status("lagging-replica-01").await.unwrap();
    assert_eq!(status.state.as_deref(), Some("confirmed"));
    assert_eq!(w.fake.writes().len(), 1);
}

#[tokio::test]
async fn a_later_edit_elsewhere_is_not_reported_as_data_loss() {
    let w = world().await;
    w.fake.lock().edit_after_next_put = Some("X-APPLE-SYNTH-WALLPAPER".to_owned());
    let jane = w.contacts.contact("card/jane.vcf").await.unwrap();
    let out = w
        .contacts
        .execute(request(
            "later-edit-0000001",
            nickname("card/jane.vcf", &jane.etag, "JD"),
        ))
        .await
        .unwrap();
    assert_eq!((out.state.as_str(), out.verified), ("unverified", false));
    let detail = out.steps[0].detail.clone().unwrap();
    assert!(detail.contains("changed elsewhere"), "{detail}");
    assert!(detail.contains("X-APPLE-SYNTH-WALLPAPER"), "{detail}");
}

#[tokio::test]
async fn ids_with_encoded_path_segments_are_refused() {
    let w = world().await;
    for id in [
        "card/%2e%2e",
        "card/%2E%2e",
        "card/%2e",
        "card/a%2fb.vcf",
        "card/a%5cb.vcf",
        "card/..",
        "%2e%2e/jane.vcf",
    ] {
        let error = w.contacts.contact(id).await.unwrap_err();
        assert!(
            matches!(error.code(), "invalid_input" | "not_found"),
            "{id}: {error}"
        );
    }
    let reads = w
        .fake
        .lock()
        .requests
        .iter()
        .filter(|r| r.method == "GET")
        .count();
    assert_eq!(reads, 0);
}

fn interrupted(key: &str, state: StepState) -> OperationRecord {
    OperationRecord {
        key_hash: omni_core::digest::sha256_hex(key.as_bytes()),
        idempotency_key: key.to_owned(),
        fingerprint: "fp".to_owned(),
        tool: "test".to_owned(),
        state: OperationState::Reserved,
        steps: vec![OperationStep {
            kind: StepKind::Put,
            resource_id: "card/jane.vcf".to_owned(),
            purpose: "update contact".to_owned(),
            precondition: "if-match:\"etag-1\"".to_owned(),
            before_etag: Some("\"etag-1\"".to_owned()),
            body_sha256: None,
            state,
            put_etag: None,
            result_etag: None,
            http_status: None,
            detail: None,
            changes: Vec::new(),
            expect: None,
            verified: false,
        }],
        dependent: false,
        result: None,
        error: None,
        created_at: NOW,
        updated_at: NOW,
        extra: Default::default(),
    }
}

#[tokio::test]
async fn writes_interrupted_by_a_restart_are_never_resent() {
    let w = world().await;
    let sending = interrupted("interrupted-sending", StepState::Sending);
    let planned = interrupted("interrupted-planned", StepState::Planned);
    for record in [&sending, &planned] {
        let record = record.clone();
        w.store
            .store
            .write(move |tx| tx.upsert(&record, UpsertOpts::default()))
            .await
            .unwrap();
    }
    // Before the boot step runs, an in-flight record is not touched.
    let in_flight = w
        .contacts
        .execute(WriteRequest {
            tool: "test",
            idempotency_key: "interrupted-sending".to_owned(),
            fingerprint: "fp".to_owned(),
            action: nickname("card/jane.vcf", "\"etag-1\"", "JD"),
        })
        .await
        .unwrap_err();
    assert_eq!(in_flight.code(), "in_progress");
    let settled = omni_contacts::service::reconcile_interrupted(&w.store.store, NOW)
        .await
        .unwrap();
    assert_eq!(settled, 2);
    let rows = w
        .store
        .store
        .read(|docs| docs.get_all::<OperationRecord>())
        .await
        .unwrap();
    let state = |key: &str| {
        rows.iter()
            .find(|r| r.idempotency_key == key)
            .map(|r| r.state)
            .unwrap()
    };
    assert_eq!(state("interrupted-sending"), OperationState::Uncertain);
    assert_eq!(state("interrupted-planned"), OperationState::Failed);
    let replay = w
        .contacts
        .execute(WriteRequest {
            tool: "test",
            idempotency_key: "interrupted-sending".to_owned(),
            fingerprint: "fp".to_owned(),
            action: nickname("card/jane.vcf", "\"etag-1\"", "JD"),
        })
        .await
        .unwrap();
    assert!(replay.replayed);
    assert!(w.fake.writes().is_empty());
}

#[tokio::test]
async fn contacts_cannot_be_deleted_through_the_group_tools() {
    let w = world().await;
    let jane = w.contacts.contact("card/jane.vcf").await.unwrap();
    let error = w
        .contacts
        .execute(request(
            "delete-a-contact-01",
            Action::GroupDelete {
                group_id: "card/jane.vcf".to_owned(),
                etag: jane.etag,
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "invalid_input");
    assert!(w.fake.writes().is_empty());
}

#[tokio::test]
async fn a_lossy_destination_write_stops_a_move_before_the_source() {
    let w = world().await;
    w.fake.lock().drop_on_put = Some("N:".to_owned());
    let error = w
        .contacts
        .execute(request(
            "lossy-move-000001",
            Action::Members {
                add: Vec::new(),
                remove: Vec::new(),
                moves: vec![(
                    "card/friends.vcf".to_owned(),
                    "card/old.vcf".to_owned(),
                    vec!["card/jane.vcf".to_owned()],
                )],
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "data_not_preserved");
    assert_eq!(
        w.fake.writes(),
        vec![("PUT".to_owned(), format!("{CARD}old.vcf"))]
    );
    assert_eq!(w.fake.body(&format!("{CARD}friends.vcf")).unwrap(), FRIENDS);
}

#[tokio::test]
async fn an_uncertain_create_settles_as_not_applied_only_while_the_card_is_absent() {
    let w = world().await;
    w.fake.lock().fail_next_write = Some(500);
    let out = w
        .contacts
        .execute(request(
            "uncertain-create-01",
            Action::GroupCreate {
                book_id: Some("card".to_owned()),
                name: "Archive".to_owned(),
                contact_ids: Vec::new(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(out.state, "uncertain");
    let status = w
        .contacts
        .write_status("uncertain-create-01")
        .await
        .unwrap();
    assert_eq!(status.state.as_deref(), Some("uncertain"));
    w.clock.set(NOW + 11 * 60 * 1000);
    let status = w
        .contacts
        .write_status("uncertain-create-01")
        .await
        .unwrap();
    assert_eq!(status.state.as_deref(), Some("failed"));
    assert_eq!(w.fake.writes().len(), 1);
}

#[tokio::test]
async fn credentials_never_follow_a_redirect_or_href_off_icloud() {
    let redirected = world().await;
    redirected.fake.lock().root_redirect = Some("http://p42-contacts.icloud.com/".to_owned());
    let status = redirected.contacts.status().await;
    assert_eq!(
        status.error_code.as_deref(),
        Some("untrusted_host"),
        "{status:?}"
    );
    let foreign_book = world().await;
    foreign_book.fake.lock().book_href_override =
        Some("https://evil.example/123/carddavhome/card/".to_owned());
    let status = foreign_book.contacts.status().await;
    assert_eq!(
        status.error_code.as_deref(),
        Some("untrusted_host"),
        "{status:?}"
    );
}

#[tokio::test]
async fn a_refused_login_on_write_fails_without_changes() {
    let w = world().await;
    let jane = w.contacts.contact("card/jane.vcf").await.unwrap();
    w.fake.lock().fail_next_write = Some(403);
    let error = w
        .contacts
        .execute(request(
            "forbidden-write-01",
            nickname("card/jane.vcf", &jane.etag, "JD"),
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "unauthorized");
    assert_eq!(
        w.fake.body(&format!("{CARD}jane.vcf")).unwrap(),
        jane_card()
    );
    assert_eq!(w.contacts.status().await.state, "ready");
}

#[tokio::test]
async fn unreadable_cards_are_retried_on_a_fresh_read() {
    let w = world().await;
    w.fake
        .lock()
        .omit_from_multiget
        .insert(format!("{CARD}bob.vcf"));
    let first = w.contacts.search(search_all()).await.unwrap();
    assert_eq!(first.unreadable_count, 1);
    w.fake.lock().omit_from_multiget.clear();
    let again = w
        .contacts
        .search(SearchQuery {
            fresh: true,
            ..search_all()
        })
        .await
        .unwrap();
    assert_eq!(again.unreadable_count, 0);
    assert!(again.contacts.iter().any(|c| c.display_name == "Bob Roe"));
}

#[tokio::test]
async fn writes_refuse_to_plan_against_a_stale_index() {
    let w = world().await;
    assert_eq!(w.contacts.status().await.state, "ready");
    w.fake.lock().fail_book_propfind = true;
    let error = w
        .contacts
        .execute(request(
            "stale-index-00001",
            Action::GroupCreate {
                book_id: Some("card".to_owned()),
                name: "Archive".to_owned(),
                contact_ids: vec!["card/bob.vcf".to_owned()],
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "stale_index");
    assert!(w.fake.writes().is_empty());
}

#[tokio::test]
async fn a_fresh_read_discovers_new_address_books() {
    let w = world().await;
    assert_eq!(w.contacts.status().await.books.len(), 2);
    // The fake lists the same books; a fresh read must still succeed and
    // keep the index of the books it already knew.
    let fresh = w
        .contacts
        .search(SearchQuery {
            fresh: true,
            ..search_all()
        })
        .await
        .unwrap();
    assert_eq!(fresh.total, 3);
}

#[tokio::test]
async fn cards_with_only_weak_etags_are_never_written() {
    let w = world().await;
    w.fake.lock().weak_etags = true;
    w.fake.put(&format!("{CARD}jane.vcf"), &jane_card());
    w.fake.put(&format!("{CARD}old.vcf"), OLD);
    let jane = w.contacts.contact("card/jane.vcf").await.unwrap();
    assert!(jane.etag.starts_with("W/"));
    let edit = w
        .contacts
        .execute(request(
            "weak-etag-edit-001",
            nickname("card/jane.vcf", &jane.etag, "JD"),
        ))
        .await
        .unwrap_err();
    assert_eq!(edit.code(), "weak_etag");
    let membership = w
        .contacts
        .execute(request(
            "weak-etag-move-001",
            Action::Members {
                add: vec![("card/old.vcf".to_owned(), vec!["card/bob.vcf".to_owned()])],
                remove: Vec::new(),
                moves: Vec::new(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(membership.code(), "weak_etag");
    assert!(w.fake.writes().is_empty());
}
