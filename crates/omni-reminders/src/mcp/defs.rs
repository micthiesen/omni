//! The Server Reminders tools' contracts: metadata, policy and the schema types
//! their input and output schemas derive from (`omni_mcp_kit::schema`). These
//! types describe the wire format only; the handlers decode and encode with
//! their own types. After changing anything here run `cargo xtask mcp-golden`
//! and review the snapshot diff.

use omni_mcp_kit::schema::{Lit, Literal, NumberLiterals, nullable, one_of};
use omni_mcp_kit::{Annotations, ApprovalPolicy, Policy, ToolDef, ToolDefinition, ToolInfo};
use schemars::JsonSchema;

pub static LIST_REMINDER_LISTS: ToolDef<ListReminderListsInput, ListReminderListsOutput> =
    ToolDef::new(ToolInfo {
        name: "list_reminder_lists",
        title: "List iCloud Reminder Lists",
        description: "Discover server iCloud Reminders lists and their stable CloudKit IDs. Each count is the number of incomplete, nondeleted reminders in that list; total is the number of lists before pagination. Requires the separately configured server account.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Reads private server iCloud Reminders"],
            cost: "none",
            recommended: ApprovalPolicy::Allow,
        },
    });

pub static GET_REMINDER_LIST: ToolDef<GetReminderListInput, GetReminderListOutput> = ToolDef::new(
    ToolInfo {
        name: "get_reminder_list",
        title: "Get iCloud Reminder List",
        description: "Read one exact list's name, color and current change tag. Does not mutate reminders.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Reads private server iCloud Reminders"],
            cost: "none",
            recommended: ApprovalPolicy::Allow,
        },
    },
);

pub static UPDATE_REMINDER_LIST: ToolDef<UpdateReminderListInput, UpdateReminderListOutput> =
    ToolDef::new(ToolInfo {
        name: "update_reminder_list",
        title: "Rename iCloud Reminder List",
        description: "Rename one exact list using its current changeTag and durable idempotencyKey. Preserves list metadata and reminder contents; verifies the stored name. Creation and deletion of lists are unsupported.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: true,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Mutates the exact owner's iCloud reminder after durable reservation"],
            cost: "none",
            recommended: ApprovalPolicy::RequireApproval,
        },
    });

pub static GET_REMINDER_RECURRENCE: ToolDef<
    GetReminderRecurrenceInput,
    GetReminderRecurrenceOutput,
> = ToolDef::new(ToolInfo {
    name: "get_reminder_recurrence",
    title: "Read iCloud Reminder Recurrence",
    description: "Read exact recurrence rule IDs, change tags, known rule details and writable status for one reminder. Unsupported rule forms remain readable and cannot be edited by these tools.",
    annotations: Annotations {
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: true,
    },
    policy: Policy {
        side_effects: &["Reads private server iCloud Reminders"],
        cost: "none",
        recommended: ApprovalPolicy::Allow,
    },
});

pub static CREATE_REMINDER_RECURRENCE: ToolDef<
    CreateReminderRecurrenceInput,
    CreateReminderRecurrenceOutput,
> = ToolDef::new(ToolInfo {
    name: "create_reminder_recurrence",
    title: "Add iCloud Reminder Recurrence",
    description: "Atomically attach one validated recurrence rule to a reminder without existing recurrence. Requires current reminder changeTag and idempotencyKey. Supports frequency, interval, occurrence count, end date and date selectors. Does not complete the reminder or generate an occurrence.",
    annotations: Annotations {
        read_only_hint: false,
        destructive_hint: true,
        idempotent_hint: true,
        open_world_hint: true,
    },
    policy: Policy {
        side_effects: &["Mutates the exact owner's iCloud reminder after durable reservation"],
        cost: "none",
        recommended: ApprovalPolicy::RequireApproval,
    },
});

pub static UPDATE_REMINDER_RECURRENCE: ToolDef<
    UpdateReminderRecurrenceInput,
    UpdateReminderRecurrenceOutput,
> = ToolDef::new(ToolInfo {
    name: "update_reminder_recurrence",
    title: "Update iCloud Reminder Recurrence",
    description: "Update specified fields on a single supported recurrence rule. Omitted fields are preserved; null clears a selector or end date. Requires both current reminder and rule change tags and idempotencyKey. Unknown rule forms are refused; uncertain writes never automatically replay.",
    annotations: Annotations {
        read_only_hint: false,
        destructive_hint: true,
        idempotent_hint: true,
        open_world_hint: true,
    },
    policy: Policy {
        side_effects: &["Mutates the exact owner's iCloud reminder after durable reservation"],
        cost: "none",
        recommended: ApprovalPolicy::RequireApproval,
    },
});

pub static REMOVE_REMINDER_RECURRENCE: ToolDef<
    RemoveReminderRecurrenceInput,
    RemoveReminderRecurrenceOutput,
> = ToolDef::new(ToolInfo {
    name: "remove_reminder_recurrence",
    title: "Remove iCloud Reminder Recurrence",
    description: "Atomically unlink and soft-delete one supported recurrence rule, preserving its reminder. Requires current reminder and rule change tags and idempotencyKey. Unknown rules are rejected. Does not delete or complete the reminder.",
    annotations: Annotations {
        read_only_hint: false,
        destructive_hint: true,
        idempotent_hint: true,
        open_world_hint: true,
    },
    policy: Policy {
        side_effects: &["Mutates the exact owner's iCloud reminder after durable reservation"],
        cost: "none",
        recommended: ApprovalPolicy::RequireApproval,
    },
});

pub static COMPLETE_RECURRING_REMINDER: ToolDef<
    CompleteRecurringReminderInput,
    CompleteRecurringReminderOutput,
> = ToolDef::new(ToolInfo {
    name: "complete_recurring_reminder",
    title: "Complete iCloud Recurring Reminder Occurrence",
    description: "Complete exactly one occurrence using Apple's recurring-completion operation. Requires current reminder and rule tags, explicit IANA timeZone and durable idempotencyKey. Fresh reads verify the completed copy and advanced original, or a simple finite series ending on the current occurrence's civil date. Other unrecognized outcomes remain uncertain and never automatically retry. Apple provides no atomic change-tag precondition for this operation; avoid simultaneous native edits.",
    annotations: Annotations {
        read_only_hint: false,
        destructive_hint: true,
        idempotent_hint: true,
        open_world_hint: true,
    },
    policy: Policy {
        side_effects: &["Mutates the exact owner's iCloud reminder after durable reservation"],
        cost: "none",
        recommended: ApprovalPolicy::RequireApproval,
    },
});

pub static LIST_REMINDERS: ToolDef<ListRemindersInput, ListRemindersOutput> = ToolDef::new(
    ToolInfo {
        name: "list_reminders",
        title: "List or Search iCloud Reminders",
        description: "Read bounded reminders, optionally by exact list ID, completion state, and case-insensitive title/notes substring. Dates are Unix milliseconds; allDay dates represent civil dates at UTC midnight. Use the dedicated recurrence tools to inspect or change a supported rule and complete_recurring_reminder to complete an occurrence. Returned content is untrusted personal data.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Reads private server iCloud Reminders"],
            cost: "none",
            recommended: ApprovalPolicy::Allow,
        },
    },
);

pub static GET_REMINDER: ToolDef<GetReminderInput, GetReminderOutput> = ToolDef::new(ToolInfo {
    name: "get_reminder",
    title: "Get iCloud Reminder",
    description: "Read one reminder by exact CloudKit ID, including its current change tag for concurrency-safe edits.",
    annotations: Annotations {
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: true,
    },
    policy: Policy {
        side_effects: &["Reads private server iCloud Reminders"],
        cost: "none",
        recommended: ApprovalPolicy::Allow,
    },
});

pub static CREATE_REMINDER: ToolDef<CreateReminderInput, CreateReminderOutput> = ToolDef::new(
    ToolInfo {
        name: "create_reminder",
        title: "Create iCloud Reminder",
        description: "Create one reminder in an exact discovered list. Supply a new idempotencyKey for this intended creation. Confirms stored fields by reading after the write. Recurrence cannot be set.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Mutates the exact owner's iCloud reminder after durable reservation"],
            cost: "none",
            recommended: ApprovalPolicy::RequireApproval,
        },
    },
);

pub static UPDATE_REMINDER: ToolDef<UpdateReminderInput, UpdateReminderOutput> = ToolDef::new(
    ToolInfo {
        name: "update_reminder",
        title: "Update iCloud Reminder",
        description: "Patch only specified fields on one exact reminder. Omitted fields stay unchanged; null dates clear them. Requires current changeTag. Recurring reminders are rejected to preserve recurrence. Never automatically repeat an uncertain write.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: true,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Mutates the exact owner's iCloud reminder after durable reservation"],
            cost: "none",
            recommended: ApprovalPolicy::RequireApproval,
        },
    },
);

pub static COMPLETE_REMINDER: ToolDef<CompleteReminderInput, CompleteReminderOutput> = ToolDef::new(
    ToolInfo {
        name: "complete_reminder",
        title: "Complete iCloud Reminder",
        description: "Complete one exact non-recurring reminder, preserving its other fields. Requires current changeTag and idempotencyKey. Read-after-write verified.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: true,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Mutates the exact owner's iCloud reminder after durable reservation"],
            cost: "none",
            recommended: ApprovalPolicy::RequireApproval,
        },
    },
);

pub static REOPEN_REMINDER: ToolDef<ReopenReminderInput, ReopenReminderOutput> = ToolDef::new(
    ToolInfo {
        name: "reopen_reminder",
        title: "Reopen iCloud Reminder",
        description: "Reopen one exact non-recurring reminder, preserving its other fields. Requires current changeTag and idempotencyKey. Read-after-write verified.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: true,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Mutates the exact owner's iCloud reminder after durable reservation"],
            cost: "none",
            recommended: ApprovalPolicy::RequireApproval,
        },
    },
);

pub static DELETE_REMINDER: ToolDef<DeleteReminderInput, DeleteReminderOutput> = ToolDef::new(
    ToolInfo {
        name: "delete_reminder",
        title: "Delete iCloud Reminder",
        description: "Soft-delete exactly one non-recurring reminder using Apple's Deleted field. Requires current changeTag. Confirms deletion by reading it back; uncertain writes never replay automatically.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: true,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Mutates the exact owner's iCloud reminder after durable reservation"],
            cost: "none",
            recommended: ApprovalPolicy::RequireApproval,
        },
    },
);

/// Every Server Reminders tool, in serving order.
pub static TOOLS: [&dyn ToolDefinition; 15] = [
    &LIST_REMINDER_LISTS,
    &GET_REMINDER_LIST,
    &UPDATE_REMINDER_LIST,
    &GET_REMINDER_RECURRENCE,
    &CREATE_REMINDER_RECURRENCE,
    &UPDATE_REMINDER_RECURRENCE,
    &REMOVE_REMINDER_RECURRENCE,
    &COMPLETE_RECURRING_REMINDER,
    &LIST_REMINDERS,
    &GET_REMINDER,
    &CREATE_REMINDER,
    &UPDATE_REMINDER,
    &COMPLETE_REMINDER,
    &REOPEN_REMINDER,
    &DELETE_REMINDER,
];

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ListReminderListsInput {
    #[schemars(description = "Zero-based result offset", extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 100), extend("default" = 25))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListReminderListsItem {
    #[schemars(length(min = 1, max = 256))]
    pub id: String,
    pub title: String,
    pub color: Option<String>,
    #[schemars(description = "Incomplete, nondeleted reminders in this list.")]
    pub count: u64,
    #[schemars(skip_serializing_if = "Option::is_none", transform = nullable)]
    pub record_change_tag: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListReminderListsOutput {
    #[schemars(length(max = 100))]
    pub items: Vec<ListReminderListsItem>,
    #[schemars(description = "Number of lists before pagination.")]
    pub total: f64,
    pub next_cursor: Option<f64>,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct GetReminderListInput {
    #[schemars(length(min = 1, max = 256))]
    pub id: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct GetReminderListList {
    #[schemars(length(min = 1, max = 256))]
    pub id: String,
    pub title: String,
    pub color: Option<String>,
    pub record_change_tag: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct GetReminderListOutput {
    pub list: Option<GetReminderListList>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateReminderListInput {
    #[schemars(
        description = "Unique key for this exact operation. Reuse only to retrieve its confirmed result; uncertain operations are never replayed.",
        pattern("^[A-Za-z0-9_-]{16,128}$")
    )]
    pub idempotency_key: String,
    #[schemars(length(min = 1, max = 256))]
    pub id: String,
    #[schemars(
        description = "Exact recordChangeTag from a fresh read; conflicts require a new read and decision",
        length(min = 1, max = 1024)
    )]
    pub change_tag: String,
    #[schemars(length(min = 1, max = 4096))]
    pub title: String,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct UpdateReminderListOutput {
    pub list: GetReminderListList,
}

pub type GetReminderRecurrenceInput = GetReminderListInput;

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum Frequency {
    Daily,
    Weekly,
    Monthly,
    Yearly,
    Hourly,
    Minutely,
    Secondly,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct DaysOfWeekItem {
    pub day_of_the_week: i64,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub week_number: Option<i64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecurrenceSupportedYesRule {
    pub frequency: Frequency,
    #[schemars(range(min = 1, max = 2147483647))]
    pub interval: u64,
    #[schemars(skip_serializing_if = "Option::is_none", range(max = 2147483647), transform = nullable)]
    pub occurrence_count: Option<u64>,
    #[schemars(description = "Preserves the server's first-day metadata, including opaque zero. Omit to preserve existing metadata.", skip_serializing_if = "Option::is_none", range(max = 7), transform = nullable)]
    pub first_day_of_week: Option<u64>,
    #[schemars(skip_serializing_if = "Option::is_none", range(max = 253402300799000_i64), transform = nullable)]
    pub end_date: Option<u64>,
    #[schemars(skip_serializing_if = "Option::is_none", transform = nullable)]
    pub days_of_week: Option<Vec<DaysOfWeekItem>>,
    #[schemars(skip_serializing_if = "Option::is_none", transform = nullable)]
    pub days_of_month: Option<Vec<i64>>,
    #[schemars(skip_serializing_if = "Option::is_none", transform = nullable)]
    pub days_of_year: Option<Vec<i64>>,
    #[schemars(skip_serializing_if = "Option::is_none", transform = nullable)]
    pub weeks_of_year: Option<Vec<i64>>,
    #[schemars(skip_serializing_if = "Option::is_none", transform = nullable)]
    pub months_of_year: Option<Vec<i64>>,
    #[schemars(skip_serializing_if = "Option::is_none", transform = nullable)]
    pub set_positions: Option<Vec<i64>>,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RecurrenceSupportedYes {
    #[schemars(transform = Literal(true))]
    pub supported: Lit,
    pub rule: RecurrenceSupportedYesRule,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RecurrenceSupportedNo {
    #[schemars(transform = Literal(false))]
    pub supported: Lit,
    pub reason: String,
    #[schemars(length(max = 32))]
    pub fields: Vec<String>,
}

#[derive(JsonSchema)]
#[schemars(untagged, transform = one_of)]
pub enum Recurrence {
    SupportedYes(RecurrenceSupportedYes),
    SupportedNo(RecurrenceSupportedNo),
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rule {
    #[schemars(length(min = 1, max = 256))]
    pub id: String,
    #[schemars(length(min = 1, max = 256))]
    pub reminder_id: String,
    pub record_change_tag: Option<String>,
    pub writable: bool,
    pub recurrence: Recurrence,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct GetReminderRecurrenceOutput {
    #[schemars(length(min = 1, max = 256))]
    pub reminder_id: String,
    pub reminder_change_tag: String,
    #[schemars(length(max = 100))]
    pub rules: Vec<Rule>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase")]
pub struct CreateReminderRecurrenceRuleDaysOfWeekItem {
    pub day_of_the_week: i64,
    pub week_number: Option<i64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateReminderRecurrenceRule {
    pub frequency: Frequency,
    #[schemars(range(min = 1, max = 2147483647))]
    pub interval: u64,
    #[schemars(range(max = 2147483647), transform = nullable)]
    pub occurrence_count: Option<u64>,
    #[schemars(description = "Preserves the server's first-day metadata, including opaque zero. Omit to preserve existing metadata.", range(max = 7), transform = nullable)]
    pub first_day_of_week: Option<u64>,
    #[schemars(range(max = 253402300799000_i64), transform = nullable)]
    pub end_date: Option<u64>,
    #[schemars(transform = nullable)]
    pub days_of_week: Option<Vec<CreateReminderRecurrenceRuleDaysOfWeekItem>>,
    #[schemars(transform = nullable)]
    pub days_of_month: Option<Vec<i64>>,
    #[schemars(transform = nullable)]
    pub days_of_year: Option<Vec<i64>>,
    #[schemars(transform = nullable)]
    pub weeks_of_year: Option<Vec<i64>>,
    #[schemars(transform = nullable)]
    pub months_of_year: Option<Vec<i64>>,
    #[schemars(transform = nullable)]
    pub set_positions: Option<Vec<i64>>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateReminderRecurrenceInput {
    #[schemars(
        description = "Unique key for this exact operation. Reuse only to retrieve its confirmed result; uncertain operations are never replayed.",
        pattern("^[A-Za-z0-9_-]{16,128}$")
    )]
    pub idempotency_key: String,
    #[schemars(length(min = 1, max = 256))]
    pub id: String,
    #[schemars(
        description = "Exact recordChangeTag from a fresh read; conflicts require a new read and decision",
        length(min = 1, max = 1024)
    )]
    pub change_tag: String,
    pub rule: CreateReminderRecurrenceRule,
}

pub type CreateReminderRecurrenceOutput = GetReminderRecurrenceOutput;

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Patch {
    pub frequency: Option<Frequency>,
    #[schemars(range(min = 1, max = 2147483647))]
    pub interval: Option<u64>,
    #[schemars(range(max = 2147483647), transform = nullable)]
    pub occurrence_count: Option<u64>,
    #[schemars(description = "Preserves the server's first-day metadata, including opaque zero. Omit to preserve existing metadata.", range(max = 7), transform = nullable)]
    pub first_day_of_week: Option<u64>,
    #[schemars(range(max = 253402300799000_i64), transform = nullable)]
    pub end_date: Option<u64>,
    #[schemars(transform = nullable)]
    pub days_of_week: Option<Vec<CreateReminderRecurrenceRuleDaysOfWeekItem>>,
    #[schemars(transform = nullable)]
    pub days_of_month: Option<Vec<i64>>,
    #[schemars(transform = nullable)]
    pub days_of_year: Option<Vec<i64>>,
    #[schemars(transform = nullable)]
    pub weeks_of_year: Option<Vec<i64>>,
    #[schemars(transform = nullable)]
    pub months_of_year: Option<Vec<i64>>,
    #[schemars(transform = nullable)]
    pub set_positions: Option<Vec<i64>>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateReminderRecurrenceInput {
    #[schemars(
        description = "Unique key for this exact operation. Reuse only to retrieve its confirmed result; uncertain operations are never replayed.",
        pattern("^[A-Za-z0-9_-]{16,128}$")
    )]
    pub idempotency_key: String,
    #[schemars(length(min = 1, max = 256))]
    pub id: String,
    #[schemars(
        description = "Exact recordChangeTag from a fresh read; conflicts require a new read and decision",
        length(min = 1, max = 1024)
    )]
    pub change_tag: String,
    #[schemars(length(min = 1, max = 256))]
    pub rule_id: String,
    #[schemars(length(min = 1, max = 256))]
    pub rule_change_tag: String,
    pub patch: Patch,
}

pub type UpdateReminderRecurrenceOutput = GetReminderRecurrenceOutput;

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoveReminderRecurrenceInput {
    #[schemars(
        description = "Unique key for this exact operation. Reuse only to retrieve its confirmed result; uncertain operations are never replayed.",
        pattern("^[A-Za-z0-9_-]{16,128}$")
    )]
    pub idempotency_key: String,
    #[schemars(length(min = 1, max = 256))]
    pub id: String,
    #[schemars(
        description = "Exact recordChangeTag from a fresh read; conflicts require a new read and decision",
        length(min = 1, max = 1024)
    )]
    pub change_tag: String,
    #[schemars(length(min = 1, max = 256))]
    pub rule_id: String,
    #[schemars(length(min = 1, max = 256))]
    pub rule_change_tag: String,
}

pub type RemoveReminderRecurrenceOutput = GetReminderRecurrenceOutput;

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompleteRecurringReminderInput {
    #[schemars(
        description = "Unique key for this exact operation. Reuse only to retrieve its confirmed result; uncertain operations are never replayed.",
        pattern("^[A-Za-z0-9_-]{16,128}$")
    )]
    pub idempotency_key: String,
    #[schemars(length(min = 1, max = 256))]
    pub id: String,
    #[schemars(
        description = "Exact recordChangeTag from a fresh read; conflicts require a new read and decision",
        length(min = 1, max = 1024)
    )]
    pub change_tag: String,
    #[schemars(length(min = 1, max = 256))]
    pub rule_id: String,
    #[schemars(length(min = 1, max = 256))]
    pub rule_change_tag: String,
    #[schemars(length(min = 1, max = 128))]
    pub time_zone: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum State {
    Advanced,
    Ended,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompleteRecurringReminderOutput {
    pub state: State,
    #[schemars(transform = Literal(true))]
    pub verified: Lit,
    #[schemars(length(min = 1, max = 256))]
    pub reminder_id: String,
    pub reminder_change_tag: String,
    #[schemars(length(min = 1, max = 256))]
    pub completed_reminder_id: String,
    pub completed_reminder_change_tag: String,
    #[schemars(length(min = 1, max = 256))]
    pub rule_id: String,
    pub time_zone: String,
    pub previous_due_date: f64,
    pub next_due_date: Option<f64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListRemindersInput {
    #[schemars(description = "Zero-based result offset", extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 100), extend("default" = 25))]
    pub limit: Option<u64>,
    #[schemars(length(min = 1, max = 256))]
    pub list_id: Option<String>,
    #[schemars(length(max = 500))]
    pub query: Option<String>,
    pub completed: Option<bool>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListRemindersItem {
    #[schemars(length(min = 1, max = 256))]
    pub id: String,
    #[schemars(length(min = 1, max = 256))]
    pub list_id: String,
    pub title: String,
    pub description: String,
    pub completed: bool,
    #[schemars(range(max = 253402300799000_i64))]
    pub due_date: Option<u64>,
    #[schemars(range(max = 253402300799000_i64))]
    pub start_date: Option<u64>,
    #[schemars(range(max = 253402300799000_i64))]
    pub completed_date: Option<u64>,
    pub priority: f64,
    pub flagged: bool,
    pub all_day: bool,
    pub deleted: bool,
    #[schemars(range(max = 253402300799000_i64))]
    pub created_date: Option<u64>,
    #[schemars(range(max = 253402300799000_i64))]
    pub last_modified_date: Option<u64>,
    pub record_change_tag: String,
    pub recurring: bool,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListRemindersOutput {
    #[schemars(length(max = 100))]
    pub items: Vec<ListRemindersItem>,
    #[schemars(
        description = "Number of nondeleted reminders matching all supplied filters before pagination."
    )]
    pub total: f64,
    pub next_cursor: Option<f64>,
}

pub type GetReminderInput = GetReminderListInput;

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct GetReminderOutput {
    pub reminder: Option<ListRemindersItem>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateReminderInput {
    #[schemars(
        description = "Unique key for this exact operation. Reuse only to retrieve its confirmed result; uncertain operations are never replayed.",
        pattern("^[A-Za-z0-9_-]{16,128}$")
    )]
    pub idempotency_key: String,
    #[schemars(length(min = 1, max = 256))]
    pub list_id: String,
    #[schemars(length(min = 1, max = 4096))]
    pub title: String,
    #[schemars(length(max = 32000))]
    pub description: Option<String>,
    #[schemars(description = "Unix milliseconds; all-day dates use midnight UTC for the intended calendar date", range(max = 253402300799000_i64), transform = nullable)]
    pub due_date: Option<u64>,
    #[schemars(range(max = 253402300799000_i64), transform = nullable)]
    pub start_date: Option<u64>,
    #[schemars(transform = NumberLiterals(&[0, 1, 5, 9]))]
    pub priority: Option<Lit>,
    pub flagged: Option<bool>,
    pub all_day: Option<bool>,
    pub completed: Option<bool>,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct CreateReminderOutput {
    pub reminder: ListRemindersItem,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateReminderPatch {
    #[schemars(length(min = 1, max = 4096))]
    pub title: Option<String>,
    #[schemars(length(max = 32000))]
    pub description: Option<String>,
    #[schemars(description = "Unix milliseconds; all-day dates use midnight UTC for the intended calendar date", range(max = 253402300799000_i64), transform = nullable)]
    pub due_date: Option<u64>,
    #[schemars(range(max = 253402300799000_i64), transform = nullable)]
    pub start_date: Option<u64>,
    #[schemars(transform = NumberLiterals(&[0, 1, 5, 9]))]
    pub priority: Option<Lit>,
    pub flagged: Option<bool>,
    pub all_day: Option<bool>,
    pub completed: Option<bool>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateReminderInput {
    #[schemars(
        description = "Unique key for this exact operation. Reuse only to retrieve its confirmed result; uncertain operations are never replayed.",
        pattern("^[A-Za-z0-9_-]{16,128}$")
    )]
    pub idempotency_key: String,
    #[schemars(length(min = 1, max = 256))]
    pub id: String,
    #[schemars(
        description = "Exact recordChangeTag from a fresh read; conflicts require a new read and decision",
        length(min = 1, max = 1024)
    )]
    pub change_tag: String,
    pub patch: UpdateReminderPatch,
}

pub type UpdateReminderOutput = CreateReminderOutput;

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompleteReminderInput {
    #[schemars(
        description = "Unique key for this exact operation. Reuse only to retrieve its confirmed result; uncertain operations are never replayed.",
        pattern("^[A-Za-z0-9_-]{16,128}$")
    )]
    pub idempotency_key: String,
    #[schemars(length(min = 1, max = 256))]
    pub id: String,
    #[schemars(
        description = "Exact recordChangeTag from a fresh read; conflicts require a new read and decision",
        length(min = 1, max = 1024)
    )]
    pub change_tag: String,
}

pub type CompleteReminderOutput = CreateReminderOutput;

pub type ReopenReminderInput = CompleteReminderInput;

pub type ReopenReminderOutput = CreateReminderOutput;

pub type DeleteReminderInput = CompleteReminderInput;

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct DeleteReminderOutput {
    #[schemars(length(min = 1, max = 256))]
    pub id: String,
    #[schemars(transform = Literal(true))]
    pub deleted: Lit,
    #[schemars(transform = Literal(true))]
    pub verified: Lit,
}
