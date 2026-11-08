//! The contacts MCP tools: bounded adapters over [`Contacts`].

pub mod defs;

use omni_core::digest::sha256_hex;
use omni_mcp_kit::{McpTool, ToolError, ToolMetaError, typed_tool};
use serde::Serialize;

use crate::edit::{AddressPatch, ContactPatch, NamePatch, NewValue, SingleField, ValueUpdate};
use crate::error::ContactsError;
use crate::labels::Field;
use crate::service::{Action, Contacts, SearchQuery, WriteRequest};
use defs::*;

fn tool_error(error: &ContactsError) -> ToolError {
    if error.code() == "invalid_input" {
        ToolError::input(error.to_string())
    } else {
        ToolError::execute(error.to_string())
    }
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::input(format!("[invalid_input] {}", message.into()))
}

/// SHA-256 of the tool name and its canonical input: a key reused with
/// another request is refused.
fn fingerprint<T: Serialize>(tool: &str, input: &T) -> String {
    sha256_hex(format!(
        "{tool}\u{1f}{}",
        serde_json::to_string(input).unwrap_or_default()
    ))
}

fn field(field: ValueField) -> Field {
    match field {
        ValueField::Email => Field::Email,
        ValueField::Phone => Field::Phone,
        ValueField::Address => Field::Address,
        ValueField::Url => Field::Url,
        ValueField::InstantMessage => Field::InstantMessage,
        ValueField::SocialProfile => Field::SocialProfile,
        ValueField::RelatedName => Field::RelatedName,
        ValueField::Date => Field::Date,
    }
}

fn address(input: &AddressIn) -> AddressPatch {
    AddressPatch {
        po_box: input.po_box.clone(),
        extended: input.extended.clone(),
        street: input.street.clone(),
        city: input.city.clone(),
        region: input.region.clone(),
        postal_code: input.postal_code.clone(),
        country: input.country.clone(),
    }
}

fn new_value(input: &NewValueIn) -> NewValue {
    NewValue {
        field: field(input.field),
        value: input.value.clone(),
        address: input.address.as_ref().map(address),
        label: input.label.clone(),
        preferred: input.preferred.unwrap_or(false),
        service: input.service.clone(),
        user: input.user.clone(),
    }
}

fn clearable(field: ClearableField) -> SingleField {
    match field {
        ClearableField::Nickname => SingleField::Nickname,
        ClearableField::Organization => SingleField::Organization,
        ClearableField::Department => SingleField::Department,
        ClearableField::JobTitle => SingleField::JobTitle,
        ClearableField::Birthday => SingleField::Birthday,
        ClearableField::Note => SingleField::Note,
    }
}

fn patch_from(fields: Option<&FieldsIn>) -> ContactPatch {
    let fields = fields.cloned().unwrap_or_default();
    ContactPatch {
        full_name: fields.full_name,
        name: fields.name.map(|n| NamePatch {
            family: n.family,
            given: n.given,
            middle: n.middle,
            prefix: n.prefix,
            suffix: n.suffix,
        }),
        nickname: fields.nickname,
        organization: fields.organization,
        department: fields.department,
        job_title: fields.job_title,
        birthday: fields.birthday,
        note: fields.note,
        ..ContactPatch::default()
    }
}

fn change(input: &ContactChange) -> (String, String, ContactPatch) {
    let mut patch = patch_from(input.set.as_ref());
    patch.clear = input
        .clear
        .iter()
        .flatten()
        .copied()
        .map(clearable)
        .collect();
    patch.add = input.add.iter().flatten().map(new_value).collect();
    patch.update = input
        .update
        .iter()
        .flatten()
        .map(|u| ValueUpdate {
            id: u.id.clone(),
            value: u.value.clone(),
            address: u.address.as_ref().map(address),
            label: u.label.clone(),
            preferred: u.preferred,
        })
        .collect();
    patch.remove = input
        .remove
        .iter()
        .flatten()
        .map(|id| id.0.clone())
        .collect();
    (input.contact_id.clone(), input.etag.clone(), patch)
}

fn ids(list: Option<&Vec<ContactIdIn>>) -> Vec<String> {
    list.into_iter().flatten().map(|id| id.0.clone()).collect()
}

fn members(list: Option<&Vec<GroupMembers>>) -> Vec<(String, Vec<String>)> {
    list.into_iter()
        .flatten()
        .map(|g| (g.group_id.clone(), ids(Some(&g.contact_ids))))
        .collect()
}

fn members_action(
    add: Option<&Vec<GroupMembers>>,
    remove: Option<&Vec<GroupMembers>>,
    moves: Option<&Vec<GroupMove>>,
) -> Action {
    Action::Members {
        add: members(add),
        remove: members(remove),
        moves: moves
            .into_iter()
            .flatten()
            .map(|m| {
                (
                    m.from_group_id.clone(),
                    m.to_group_id.clone(),
                    ids(Some(&m.contact_ids)),
                )
            })
            .collect(),
    }
}

fn contact_create_action(
    book_id: Option<&String>,
    fields: &FieldsIn,
    values: Option<&Vec<NewValueIn>>,
    group_ids: Option<&Vec<ContactIdIn>>,
) -> Action {
    let mut patch = patch_from(Some(fields));
    patch.add = values.into_iter().flatten().map(new_value).collect();
    Action::ContactCreate {
        book_id: book_id.cloned(),
        patch: Box::new(patch),
        group_ids: ids(group_ids),
    }
}

fn preview_action(input: &PreviewInput) -> Result<Action, ToolError> {
    let mut actions = Vec::new();
    if let Some(g) = &input.group_create {
        actions.push(Action::GroupCreate {
            book_id: g.book_id.clone(),
            name: g.name.clone(),
            contact_ids: ids(g.contact_ids.as_ref()),
        });
    }
    if let Some(g) = &input.group_update {
        actions.push(Action::GroupRename {
            group_id: g.group_id.clone(),
            etag: g.etag.clone(),
            name: g.name.clone(),
        });
    }
    if let Some(g) = &input.group_delete {
        actions.push(Action::GroupDelete {
            group_id: g.group_id.clone(),
            etag: g.etag.clone(),
        });
    }
    if let Some(m) = &input.members_change {
        actions.push(members_action(
            m.add.as_ref(),
            m.remove.as_ref(),
            m.r#move.as_ref(),
        ));
    }
    if let Some(c) = &input.contact_create {
        actions.push(contact_create_action(
            c.book_id.as_ref(),
            &c.fields,
            c.values.as_ref(),
            c.group_ids.as_ref(),
        ));
    }
    if let Some(u) = &input.contacts_update {
        actions.push(Action::ContactsUpdate {
            changes: u.changes.iter().map(change).collect(),
        });
    }
    match actions.len() {
        1 => Ok(actions.remove(0)),
        _ => Err(invalid(
            "give exactly one of groupCreate, groupUpdate, groupDelete, membersChange, contactCreate or contactsUpdate",
        )),
    }
}

impl Contacts {
    async fn write(
        &self,
        tool: &'static str,
        key: &str,
        fingerprint: String,
        action: Action,
    ) -> Result<WriteOutput, ToolError> {
        self.execute(WriteRequest {
            tool,
            idempotency_key: key.to_owned(),
            fingerprint,
            action,
        })
        .await
        .map_err(|e| tool_error(&e))
    }
}

macro_rules! tool {
    ($t:expr, $def:expr, |$tools:ident, $input:ident : $ty:ty| $body:expr) => {{
        let tools = $t.clone();
        typed_tool(&$def, move |$input: $ty, _cx| {
            let $tools = tools.clone();
            async move { $body }
        })?
    }};
}

/// The contacts tools, in serving order.
pub fn tools(contacts: Contacts) -> Result<Vec<McpTool>, ToolMetaError> {
    let t = contacts;
    Ok(vec![
        tool!(t, CONTACTS_STATUS, |t, _input: StatusInput| Ok::<
            _,
            ToolError,
        >(
            t.status().await
        )),
        tool!(t, CONTACTS_SEARCH, |t, input: SearchInput| {
            t.search(SearchQuery {
                query: input.query.as_deref().filter(|q| !q.trim().is_empty()),
                book_id: input.book_id.as_deref(),
                group_id: input.group_id.as_deref(),
                ungrouped: input.ungrouped.unwrap_or(false),
                cursor: input.cursor.unwrap_or(0) as usize,
                limit: input.limit.unwrap_or(50) as usize,
                fresh: input.fresh.unwrap_or(false),
            })
            .await
            .map_err(|e| tool_error(&e))
        }),
        tool!(t, CONTACTS_CONTACT_GET, |t, input: ContactGetInput| t
            .contact(&input.contact_id)
            .await
            .map_err(|e| tool_error(&e))),
        tool!(t, CONTACTS_GROUPS_LIST, |t, input: GroupsListInput| t
            .groups(input.book_id.as_deref(), input.fresh.unwrap_or(false))
            .await
            .map_err(|e| tool_error(&e))),
        tool!(t, CONTACTS_CHANGE_PREVIEW, |t, input: PreviewInput| {
            let action = preview_action(&input)?;
            t.preview(&action, input.idempotency_key.as_deref())
                .await
                .map_err(|e| tool_error(&e))
        }),
        tool!(t, CONTACTS_WRITE_STATUS, |t, input: WriteStatusInput| t
            .write_status(&input.idempotency_key)
            .await
            .map_err(|e| tool_error(&e))),
        tool!(t, CONTACTS_GROUP_CREATE, |t, input: GroupCreateInput| {
            let action = Action::GroupCreate {
                book_id: input.book_id.clone(),
                name: input.name.clone(),
                contact_ids: ids(input.contact_ids.as_ref()),
            };
            let print = fingerprint("contacts_group_create", &input);
            t.write(
                "contacts_group_create",
                &input.idempotency_key,
                print,
                action,
            )
            .await
        }),
        tool!(t, CONTACTS_GROUP_UPDATE, |t, input: GroupUpdateInput| {
            let action = Action::GroupRename {
                group_id: input.group_id.clone(),
                etag: input.etag.clone(),
                name: input.name.clone(),
            };
            let print = fingerprint("contacts_group_update", &input);
            t.write(
                "contacts_group_update",
                &input.idempotency_key,
                print,
                action,
            )
            .await
        }),
        tool!(t, CONTACTS_GROUP_DELETE, |t, input: GroupDeleteInput| {
            let action = Action::GroupDelete {
                group_id: input.group_id.clone(),
                etag: input.etag.clone(),
            };
            let print = fingerprint("contacts_group_delete", &input);
            t.write(
                "contacts_group_delete",
                &input.idempotency_key,
                print,
                action,
            )
            .await
        }),
        tool!(
            t,
            CONTACTS_GROUP_MEMBERS_CHANGE,
            |t, input: MembersChangeInput| {
                let action = members_action(
                    input.add.as_ref(),
                    input.remove.as_ref(),
                    input.r#move.as_ref(),
                );
                let print = fingerprint("contacts_group_members_change", &input);
                t.write(
                    "contacts_group_members_change",
                    &input.idempotency_key,
                    print,
                    action,
                )
                .await
            }
        ),
        tool!(
            t,
            CONTACTS_CONTACT_CREATE,
            |t, input: ContactCreateInput| {
                let action = contact_create_action(
                    input.book_id.as_ref(),
                    &input.fields,
                    input.values.as_ref(),
                    input.group_ids.as_ref(),
                );
                let print = fingerprint("contacts_contact_create", &input);
                t.write(
                    "contacts_contact_create",
                    &input.idempotency_key,
                    print,
                    action,
                )
                .await
            }
        ),
        tool!(
            t,
            CONTACTS_CONTACTS_UPDATE,
            |t, input: ContactsUpdateInput| {
                let action = Action::ContactsUpdate {
                    changes: input.changes.iter().map(change).collect(),
                };
                let print = fingerprint("contacts_contacts_update", &input);
                t.write(
                    "contacts_contacts_update",
                    &input.idempotency_key,
                    print,
                    action,
                )
                .await
            }
        ),
    ])
}
