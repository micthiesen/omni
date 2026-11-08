//! The contacts tools' contracts: metadata, policy and the wire types their
//! schemas derive from. After changing anything here run
//! `cargo xtask mcp-golden` and review the snapshot diff.

use omni_mcp_kit::{Annotations, ApprovalPolicy, Policy, ToolDef, ToolDefinition, ToolInfo};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const READ: Annotations = Annotations {
    read_only_hint: true,
    destructive_hint: false,
    idempotent_hint: true,
    open_world_hint: false,
};

const READ_POLICY: Policy = Policy {
    side_effects: &[],
    cost: "None; a few CardDAV reads",
    recommended: ApprovalPolicy::Allow,
};

const WRITE: Annotations = Annotations {
    read_only_hint: false,
    destructive_hint: false,
    idempotent_hint: true,
    open_world_hint: true,
};

const DESTRUCTIVE: Annotations = Annotations {
    read_only_hint: false,
    destructive_hint: true,
    idempotent_hint: true,
    open_world_hint: true,
};

const WRITE_COST: &str = "No paid API; a few CardDAV requests";

pub static CONTACTS_STATUS: ToolDef<StatusInput, StatusOutput> = ToolDef::new(ToolInfo {
    name: "contacts_status",
    title: "Inspect Contacts Status",
    description: "Report whether iCloud contacts are reachable, list every address book of the connected iCloud account with its contact, group and unreadable-card counts, and state what the tools cannot reach or verify (other accounts, device-only contacts, Contact Poster data kept outside the vCard). Never reveals URLs or credentials.",
    annotations: READ,
    policy: READ_POLICY,
});

pub static CONTACTS_SEARCH: ToolDef<SearchInput, SearchOutput> = ToolDef::new(ToolInfo {
    name: "contacts_search",
    title: "Search Contacts",
    description: "Search contacts across every iCloud address book by name, organization, email, phone, address or note (an empty query lists all), optionally only members of one group (list) or contacts in no group. Sorted by name; page with cursor.",
    annotations: READ,
    policy: READ_POLICY,
});

pub static CONTACTS_CONTACT_GET: ToolDef<ContactGetInput, ContactDetail> = ToolDef::new(ToolInfo {
    name: "contacts_contact_get",
    title: "Get Contact",
    description: "Read one contact fresh from iCloud: fields, labeled values with their ids (for edits), the groups it belongs to, the names of every other property it carries (photos, Apple extensions), and its etag.",
    annotations: READ,
    policy: READ_POLICY,
});

pub static CONTACTS_GROUPS_LIST: ToolDef<GroupsListInput, GroupsListOutput> = ToolDef::new(
    ToolInfo {
        name: "contacts_groups_list",
        title: "List Contact Groups",
        description: "List the contact groups (lists) of every iCloud address book with member counts and etags. List a group's members with contacts_search groupId.",
        annotations: READ,
        policy: READ_POLICY,
    },
);

pub static CONTACTS_CHANGE_PREVIEW: ToolDef<PreviewInput, PreviewOutput> = ToolDef::new(ToolInfo {
    name: "contacts_change_preview",
    title: "Preview Contacts Change",
    description: "Plan any contacts write (exactly one of groupCreate, groupUpdate, groupDelete, membersChange, contactCreate, contactsUpdate) against current iCloud state and show each request with the exact lines it removes and adds, without changing anything. Lines not shown are kept byte for byte.",
    annotations: READ,
    policy: READ_POLICY,
});

pub static CONTACTS_WRITE_STATUS: ToolDef<WriteStatusInput, WriteStatusOutput> = ToolDef::new(
    ToolInfo {
        name: "contacts_write_status",
        title: "Check Contacts Write",
        description: "Report what happened to a contacts write by its idempotencyKey. An uncertain request is settled by reading the card back; it is never sent again.",
        annotations: READ,
        policy: READ_POLICY,
    },
);

pub static CONTACTS_GROUP_CREATE: ToolDef<GroupCreateInput, WriteOutput> = ToolDef::new(ToolInfo {
    name: "contacts_group_create",
    title: "Create Contact Group",
    description: "Create a contact group (list) in an iCloud address book, optionally with initial members from the same book. Member contacts are not modified. Repeating the idempotencyKey returns the first result. Requires approval.",
    annotations: WRITE,
    policy: Policy {
        side_effects: &["Creates a group in the owner's iCloud contacts"],
        cost: WRITE_COST,
        recommended: ApprovalPolicy::RequireApproval,
    },
});

pub static CONTACTS_GROUP_UPDATE: ToolDef<GroupUpdateInput, WriteOutput> = ToolDef::new(ToolInfo {
    name: "contacts_group_update",
    title: "Rename Contact Group",
    description: "Rename a contact group (it replaces the current name). Its members and other properties are unchanged. Requires approval.",
    annotations: DESTRUCTIVE,
    policy: Policy {
        side_effects: &["Renames a group in the owner's iCloud contacts"],
        cost: WRITE_COST,
        recommended: ApprovalPolicy::RequireApproval,
    },
});

pub static CONTACTS_GROUP_DELETE: ToolDef<GroupDeleteInput, WriteOutput> = ToolDef::new(ToolInfo {
    name: "contacts_group_delete",
    title: "Delete Contact Group",
    description: "Delete a contact group (the list only; its member contacts stay). Requires the group's current etag. Requires approval.",
    annotations: DESTRUCTIVE,
    policy: Policy {
        side_effects: &["Deletes a group from the owner's iCloud contacts"],
        cost: WRITE_COST,
        recommended: ApprovalPolicy::RequireApproval,
    },
});

pub static CONTACTS_GROUP_MEMBERS_CHANGE: ToolDef<MembersChangeInput, WriteOutput> = ToolDef::new(
    ToolInfo {
        name: "contacts_group_members_change",
        title: "Change Group Membership",
        description: "Add contacts to groups (lists), remove them, or move them between groups of the same address book. Only the group cards are written; contact cards are never written, so a contact's own data cannot change. Groups gaining members are written before groups losing them, and a refused or uncertain write stops the rest, so a move never leaves a contact in neither group. A call in which one group both gains and loses members while other groups change (a swap or chain) is refused: send the additions first, then the removals. Requires approval.",
        annotations: DESTRUCTIVE,
        policy: Policy {
            side_effects: &["Changes group membership in the owner's iCloud contacts"],
            cost: WRITE_COST,
            recommended: ApprovalPolicy::RequireApproval,
        },
    },
);

pub static CONTACTS_CONTACT_CREATE: ToolDef<ContactCreateInput, WriteOutput> = ToolDef::new(
    ToolInfo {
        name: "contacts_contact_create",
        title: "Create Contact",
        description: "Create a contact in an iCloud address book, optionally adding it to groups of that book. Repeating the idempotencyKey returns the first result. Requires approval.",
        annotations: WRITE,
        policy: Policy {
            side_effects: &["Creates a contact in the owner's iCloud contacts"],
            cost: WRITE_COST,
            recommended: ApprovalPolicy::RequireApproval,
        },
    },
);

pub static CONTACTS_CONTACTS_UPDATE: ToolDef<ContactsUpdateInput, WriteOutput> = ToolDef::new(
    ToolInfo {
        name: "contacts_contacts_update",
        title: "Update Contacts",
        description: "Edit up to 50 contact cards, each at the etag it was read with: set or clear single fields, and add, change (value, label, preferred) or remove labeled values by id. Only the named vCard lines change; every other line (photo, unknown and Apple-specific properties) is sent byte for byte, and the card iCloud stores for the write is read back and compared. data_not_preserved means that comparison failed after the write applied: the remaining cards are not written, nothing is rolled back, and the card's earlier text is kept privately in Omni for 30 days for manual recovery. Data iOS keeps outside the vCard, such as Contact Poster assets, is not visible here and its preservation is not verified. Every edit is validated before anything is sent. Requires approval.",
        annotations: DESTRUCTIVE,
        policy: Policy {
            side_effects: &["Changes contacts in the owner's iCloud contacts"],
            cost: WRITE_COST,
            recommended: ApprovalPolicy::RequireApproval,
        },
    },
);

/// Every contacts tool, in serving order.
pub static TOOLS: [&dyn ToolDefinition; 12] = [
    &CONTACTS_STATUS,
    &CONTACTS_SEARCH,
    &CONTACTS_CONTACT_GET,
    &CONTACTS_GROUPS_LIST,
    &CONTACTS_CHANGE_PREVIEW,
    &CONTACTS_WRITE_STATUS,
    &CONTACTS_GROUP_CREATE,
    &CONTACTS_GROUP_UPDATE,
    &CONTACTS_GROUP_DELETE,
    &CONTACTS_GROUP_MEMBERS_CHANGE,
    &CONTACTS_CONTACT_CREATE,
    &CONTACTS_CONTACTS_UPDATE,
];

// ---- status ----

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatusInput {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BookOut {
    pub book_id: String,
    pub name: String,
    pub contact_count: u64,
    pub group_count: u64,
    /// Cards that could not be read (too large or not a vCard).
    pub unreadable_count: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatusOutput {
    pub configured: bool,
    /// `ready`, `not_configured`, `stale` or `error`.
    pub state: String,
    pub error_code: Option<String>,
    pub message: Option<String>,
    pub books: Vec<BookOut>,
    pub synced_at: Option<String>,
    /// What these tools can reach and what their verification covers.
    pub limits: Vec<String>,
}

// ---- search ----

const ID_PATTERN: &str = r"^[A-Za-z0-9._~%@+=:,-]{1,200}/[A-Za-z0-9._~%@+=:,-]{1,200}$";
const BOOK_PATTERN: &str = r"^[A-Za-z0-9._~%@+=:,-]{1,200}$";
const KEY_PATTERN: &str = r"^[A-Za-z0-9_-]{16,128}$";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchInput {
    /// Substring of a name, organization, email, phone (digits), address or note.
    #[schemars(length(max = 200))]
    pub query: Option<String>,
    #[schemars(regex(pattern = BOOK_PATTERN))]
    pub book_id: Option<String>,
    /// Only members of this group.
    #[schemars(regex(pattern = ID_PATTERN))]
    pub group_id: Option<String>,
    /// Only contacts in no group of their book. A group card that could not
    /// be read (see unreadableCount) cannot count its members.
    pub ungrouped: Option<bool>,
    /// From a previous nextCursor.
    #[schemars(range(max = 100000))]
    pub cursor: Option<u32>,
    #[schemars(range(min = 1, max = 200))]
    pub limit: Option<u32>,
    /// Refresh from iCloud first even if the index is recent.
    pub fresh: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContactSummary {
    pub contact_id: String,
    pub book_id: String,
    pub etag: String,
    pub display_name: String,
    pub organization: Option<String>,
    pub emails: Vec<String>,
    pub phones: Vec<String>,
    pub group_ids: Vec<String>,
    pub has_photo: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchOutput {
    pub contacts: Vec<ContactSummary>,
    pub total: u64,
    pub next_cursor: Option<u32>,
    /// Cards in the searched books that could not be read (not searched).
    pub unreadable_count: u64,
    pub synced_at: Option<String>,
    /// The last refresh failed; results are from syncedAt.
    pub stale: bool,
}

// ---- contact detail ----

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContactGetInput {
    #[schemars(regex(pattern = ID_PATTERN))]
    pub contact_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ValueField {
    Email,
    Phone,
    Address,
    Url,
    InstantMessage,
    SocialProfile,
    RelatedName,
    Date,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NameOut {
    pub family: String,
    pub given: String,
    pub middle: String,
    pub prefix: String,
    pub suffix: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AddressOut {
    pub street: String,
    pub city: String,
    pub region: String,
    pub postal_code: String,
    pub country: String,
    pub po_box: String,
    pub extended: String,
    /// Apple's X-ABADR country code.
    pub country_code: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ValueOut {
    /// Names this exact line for update or remove (valid with this etag).
    pub id: String,
    pub field: ValueField,
    /// The value (an address on one line).
    pub value: String,
    /// `mobile`, `home`, `work`, `other`, ... or a custom label.
    pub label: Option<String>,
    /// The Apple label token as stored (`_$!<Son>!$_`), to copy the exact
    /// label to another value.
    pub label_token: Option<String>,
    pub custom_label: bool,
    pub preferred: bool,
    pub address: Option<AddressOut>,
    /// An IM address's or social profile's user name.
    pub user: Option<String>,
    /// The IM or social service (`X-SERVICE-TYPE`, or a profile's type).
    pub service: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupRef {
    pub group_id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PropertyCount {
    pub name: String,
    pub count: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContactDetail {
    pub contact_id: String,
    pub book_id: String,
    /// Pass as `etag` to update or delete exactly this version.
    pub etag: String,
    pub uid: Option<String>,
    pub display_name: String,
    pub full_name: Option<String>,
    pub name: Option<NameOut>,
    pub nickname: Option<String>,
    pub organization: Option<String>,
    pub department: Option<String>,
    pub job_title: Option<String>,
    /// `YYYY-MM-DD`, or `--MM-DD` when the year is hidden.
    pub birthday: Option<String>,
    /// Apple's X-APPLE-OMIT-YEAR hides the birthday's year.
    pub birthday_year_omitted: bool,
    pub note: Option<String>,
    pub values: Vec<ValueOut>,
    pub groups: Vec<GroupRef>,
    /// Properties kept byte for byte on every edit that does not name them
    /// (photo, Apple extensions, unknown properties), by name.
    pub other_properties: Vec<PropertyCount>,
    pub has_photo: bool,
    pub vcard_version: Option<String>,
    /// Whether the edit tools can change it, and why not.
    pub editable: bool,
    pub blockers: Vec<String>,
}

// ---- groups ----

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupsListInput {
    #[schemars(regex(pattern = BOOK_PATTERN))]
    pub book_id: Option<String>,
    pub fresh: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupOut {
    pub group_id: String,
    pub book_id: String,
    pub name: String,
    pub etag: String,
    pub member_count: u64,
    /// Member references naming no contact in the book.
    pub unresolved_member_count: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupsListOutput {
    pub groups: Vec<GroupOut>,
    /// Cards that could not be read; an unreadable group is not listed.
    pub unreadable_count: u64,
    pub synced_at: Option<String>,
    pub stale: bool,
}

// ---- writes ----

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupCreateInput {
    #[schemars(regex(pattern = KEY_PATTERN))]
    pub idempotency_key: String,
    /// Required when the account has more than one address book.
    #[schemars(regex(pattern = BOOK_PATTERN))]
    pub book_id: Option<String>,
    #[schemars(length(min = 1, max = 200))]
    pub name: String,
    /// Initial members (contacts of the same book).
    #[schemars(length(max = 1000))]
    pub contact_ids: Option<Vec<ContactIdIn>>,
}

/// A contactId.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ContactIdIn(#[schemars(regex(pattern = ID_PATTERN))] pub String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupUpdateInput {
    #[schemars(regex(pattern = KEY_PATTERN))]
    pub idempotency_key: String,
    #[schemars(regex(pattern = ID_PATTERN))]
    pub group_id: String,
    /// Refuse if the group changed since this etag was read.
    #[schemars(length(min = 1, max = 200))]
    pub etag: Option<String>,
    #[schemars(length(min = 1, max = 200))]
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupDeleteInput {
    #[schemars(regex(pattern = KEY_PATTERN))]
    pub idempotency_key: String,
    #[schemars(regex(pattern = ID_PATTERN))]
    pub group_id: String,
    #[schemars(length(min = 1, max = 200))]
    pub etag: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupMembers {
    #[schemars(regex(pattern = ID_PATTERN))]
    pub group_id: String,
    #[schemars(length(min = 1, max = 1000))]
    pub contact_ids: Vec<ContactIdIn>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupMove {
    #[schemars(regex(pattern = ID_PATTERN))]
    pub from_group_id: String,
    #[schemars(regex(pattern = ID_PATTERN))]
    pub to_group_id: String,
    #[schemars(length(min = 1, max = 1000))]
    pub contact_ids: Vec<ContactIdIn>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MembersChangeInput {
    #[schemars(regex(pattern = KEY_PATTERN))]
    pub idempotency_key: String,
    #[schemars(length(max = 50))]
    pub add: Option<Vec<GroupMembers>>,
    #[schemars(length(max = 50))]
    pub remove: Option<Vec<GroupMembers>>,
    /// Add to toGroupId, then remove from fromGroupId.
    #[schemars(length(max = 50))]
    pub r#move: Option<Vec<GroupMove>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NameIn {
    #[schemars(length(max = 200))]
    pub family: Option<String>,
    #[schemars(length(max = 200))]
    pub given: Option<String>,
    #[schemars(length(max = 200))]
    pub middle: Option<String>,
    #[schemars(length(max = 100))]
    pub prefix: Option<String>,
    #[schemars(length(max = 100))]
    pub suffix: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AddressIn {
    #[schemars(length(max = 500))]
    pub street: Option<String>,
    #[schemars(length(max = 200))]
    pub city: Option<String>,
    #[schemars(length(max = 200))]
    pub region: Option<String>,
    #[schemars(length(max = 50))]
    pub postal_code: Option<String>,
    #[schemars(length(max = 200))]
    pub country: Option<String>,
    #[schemars(length(max = 100))]
    pub po_box: Option<String>,
    #[schemars(length(max = 200))]
    pub extended: Option<String>,
}

/// Single fields to set; absent fields are left alone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FieldsIn {
    #[schemars(length(min = 1, max = 300))]
    pub full_name: Option<String>,
    /// Only the given parts change.
    pub name: Option<NameIn>,
    #[schemars(length(max = 200))]
    pub nickname: Option<String>,
    #[schemars(length(max = 300))]
    pub organization: Option<String>,
    #[schemars(length(max = 300))]
    pub department: Option<String>,
    #[schemars(length(max = 300))]
    pub job_title: Option<String>,
    /// `YYYY-MM-DD`, or `--MM-DD` for a birthday without a year.
    #[schemars(regex(pattern = r"^(\d{4}|-)-\d{2}-\d{2}$"))]
    pub birthday: Option<String>,
    #[schemars(length(max = 10000))]
    pub note: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ClearableField {
    Nickname,
    Organization,
    Department,
    JobTitle,
    Birthday,
    Note,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewValueIn {
    pub field: ValueField,
    /// The value; addresses use `address`.
    #[schemars(length(min = 1, max = 1000))]
    pub value: Option<String>,
    pub address: Option<AddressIn>,
    /// `mobile`, `iphone`, `home`, `work`, `main`, `home fax`, `work fax`,
    /// `other fax`, `pager`, `other`, `homepage`, `school`, a relation
    /// (`mother`, `friend`, ...), `anniversary`, an Apple label token as
    /// read in labelToken (`_$!<Son>!$_`), or any custom label.
    #[schemars(length(min = 1, max = 100))]
    pub label: Option<String>,
    pub preferred: Option<bool>,
    /// Required for instantMessage (X-SERVICE-TYPE, e.g. `Skype`) and
    /// socialProfile (e.g. `twitter`) values; not used otherwise.
    #[schemars(length(min = 1, max = 60))]
    pub service: Option<String>,
    /// A socialProfile's user name.
    #[schemars(length(min = 1, max = 200))]
    pub user: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ValueUpdateIn {
    /// A value id from contacts_contact_get.
    #[schemars(regex(pattern = r"^[0-9a-f]{12}$"))]
    pub id: String,
    #[schemars(length(min = 1, max = 1000))]
    pub value: Option<String>,
    /// Only the given address parts change.
    pub address: Option<AddressIn>,
    #[schemars(length(min = 1, max = 100))]
    pub label: Option<String>,
    pub preferred: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContactChange {
    #[schemars(regex(pattern = ID_PATTERN))]
    pub contact_id: String,
    /// The etag the contact was read with.
    #[schemars(length(min = 1, max = 200))]
    pub etag: String,
    pub set: Option<FieldsIn>,
    #[schemars(length(max = 6))]
    pub clear: Option<Vec<ClearableField>>,
    #[schemars(length(max = 50))]
    pub add: Option<Vec<NewValueIn>>,
    #[schemars(length(max = 100))]
    pub update: Option<Vec<ValueUpdateIn>>,
    /// Value ids to remove (with their label lines).
    #[schemars(length(max = 100))]
    pub remove: Option<Vec<ValueIdIn>>,
}

/// A value id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ValueIdIn(#[schemars(regex(pattern = r"^[0-9a-f]{12}$"))] pub String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContactsUpdateInput {
    #[schemars(regex(pattern = KEY_PATTERN))]
    pub idempotency_key: String,
    #[schemars(length(min = 1, max = 50))]
    pub changes: Vec<ContactChange>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContactCreateInput {
    #[schemars(regex(pattern = KEY_PATTERN))]
    pub idempotency_key: String,
    #[schemars(regex(pattern = BOOK_PATTERN))]
    pub book_id: Option<String>,
    pub fields: FieldsIn,
    #[schemars(length(max = 50))]
    pub values: Option<Vec<NewValueIn>>,
    /// Groups of the same book to add the new contact to.
    #[schemars(length(max = 20))]
    pub group_ids: Option<Vec<ContactIdIn>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StepOut {
    pub resource_id: String,
    /// `PUT` or `DELETE`.
    pub method: String,
    pub purpose: String,
    /// `planned`, `sending`, `acknowledged` (applied, not yet visible or
    /// changed elsewhere since), `verified`, `diverged` (applied, but the
    /// card read back lost or changed unedited lines), `rejected`,
    /// `uncertain` or `skipped`.
    pub state: String,
    pub http_status: Option<u16>,
    pub etag: Option<String>,
    /// Read back and matching the plan.
    pub verified: bool,
    /// Property names and actions.
    pub changes: Vec<String>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WriteOutput {
    /// `created`, `updated`, `deleted` or `unchanged`.
    pub status: String,
    /// `confirmed` (every write read back matching), `partial` (some
    /// independent writes were refused; see steps), `unverified` (a write is
    /// not visible yet, or the card changed elsewhere since; not a success)
    /// or `uncertain` (check contacts_write_status; never retry with a new
    /// key). Failed and diverged writes are tool errors.
    pub state: String,
    pub contact_id: Option<String>,
    pub group_id: Option<String>,
    pub steps: Vec<StepOut>,
    /// Resources the request already matched (nothing sent).
    pub unchanged: Vec<String>,
    /// Every write was read back and matches.
    pub verified: bool,
    pub warnings: Vec<String>,
    /// This idempotencyKey was already used; this is the recorded result.
    pub replayed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewInput {
    /// Used for the ids a create would assign; pass the key you will write with.
    #[schemars(regex(pattern = KEY_PATTERN))]
    pub idempotency_key: Option<String>,
    pub group_create: Option<PreviewGroupCreate>,
    pub group_update: Option<PreviewGroupUpdate>,
    pub group_delete: Option<PreviewGroupDelete>,
    pub members_change: Option<PreviewMembersChange>,
    pub contact_create: Option<PreviewContactCreate>,
    pub contacts_update: Option<PreviewContactsUpdate>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewGroupCreate {
    #[schemars(regex(pattern = BOOK_PATTERN))]
    pub book_id: Option<String>,
    #[schemars(length(min = 1, max = 200))]
    pub name: String,
    #[schemars(length(max = 1000))]
    pub contact_ids: Option<Vec<ContactIdIn>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewGroupUpdate {
    #[schemars(regex(pattern = ID_PATTERN))]
    pub group_id: String,
    #[schemars(length(min = 1, max = 200))]
    pub etag: Option<String>,
    #[schemars(length(min = 1, max = 200))]
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewGroupDelete {
    #[schemars(regex(pattern = ID_PATTERN))]
    pub group_id: String,
    #[schemars(length(min = 1, max = 200))]
    pub etag: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewMembersChange {
    #[schemars(length(max = 50))]
    pub add: Option<Vec<GroupMembers>>,
    #[schemars(length(max = 50))]
    pub remove: Option<Vec<GroupMembers>>,
    #[schemars(length(max = 50))]
    pub r#move: Option<Vec<GroupMove>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewContactCreate {
    #[schemars(regex(pattern = BOOK_PATTERN))]
    pub book_id: Option<String>,
    pub fields: FieldsIn,
    #[schemars(length(max = 50))]
    pub values: Option<Vec<NewValueIn>>,
    #[schemars(length(max = 20))]
    pub group_ids: Option<Vec<ContactIdIn>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewContactsUpdate {
    #[schemars(length(min = 1, max = 50))]
    pub changes: Vec<ContactChange>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlannedWrite {
    pub method: String,
    pub resource_id: String,
    pub purpose: String,
    /// `if-match:<etag>` or `if-none-match`.
    pub precondition: String,
    pub changes: Vec<String>,
    /// Unfolded lines the write removes (long values shortened).
    pub removed_lines: Vec<String>,
    /// Unfolded lines the write adds.
    pub added_lines: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewOutput {
    pub status: String,
    pub contact_id: Option<String>,
    pub group_id: Option<String>,
    pub writes: Vec<PlannedWrite>,
    pub unchanged: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WriteStatusInput {
    #[schemars(regex(pattern = KEY_PATTERN))]
    pub idempotency_key: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WriteErrorOut {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WriteStatusOutput {
    pub found: bool,
    /// `reserved`, `confirmed`, `partial`, `unverified`, `uncertain`,
    /// `diverged` or `failed`.
    pub state: Option<String>,
    pub tool: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub result: Option<WriteOutput>,
    pub error: Option<WriteErrorOut>,
}
