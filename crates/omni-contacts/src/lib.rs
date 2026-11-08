//! iCloud contacts over CardDAV: discovery of every address book of the
//! account, an in-memory index for search, lossless field-targeted vCard
//! edits, contact groups (lists) and their membership, and idempotent,
//! verified writes behind the `contacts_*` MCP tools. See `docs/contacts.md`.

pub mod dav;
pub mod directory;
pub mod edit;
pub mod error;
pub mod labels;
pub mod mcp;
pub mod model;
pub mod service;
pub mod store;
pub mod vcard;

use omni_mcp_kit::ToolMetaError;
use omni_runtime::{AppContext, BootError, BootPhase, BootStep, ManagedEntity, Subsystem};
use omni_store::entity::EntityDescriptor;

pub use error::ContactsError;
pub use service::{Contacts, ContactsDeps, ContactsSettings};

const LOG: &str = "Contacts";

/// The entity descriptors this crate owns.
pub fn entities() -> Vec<EntityDescriptor> {
    vec![
        EntityDescriptor::of::<store::OperationRecord>(),
        EntityDescriptor::of::<store::RecoveryCopy>(),
    ]
}

/// The contacts service configured from the app context.
pub fn service(ctx: &AppContext) -> Contacts {
    Contacts::new(ContactsDeps {
        http: ctx.http.clone(),
        settings: ContactsSettings::from_config(&ctx.config),
        store: ctx.store.clone(),
        clock: ctx.clock.clone(),
        mode: ctx.side_effects,
        tracker: Some(ctx.tracker.clone()),
    })
}

/// The contacts subsystem: MCP tools (registered even without credentials,
/// reporting `not_configured`), the operation entity and the boot step that
/// settles writes interrupted by a restart. No task polls iCloud: contacts
/// are read only when a tool asks.
pub fn subsystem(ctx: &AppContext) -> Result<Subsystem, ToolMetaError> {
    let contacts = service(ctx);
    let configured = contacts.configured();
    let mut subsystem = Subsystem::named("contacts");
    subsystem.mcp_tools = mcp::tools(contacts)?;
    subsystem.entities = entities();
    subsystem.managed_entities = vec![ManagedEntity {
        slug: "contacts-mcp-operation",
        label: "Contacts write operations",
        description: "Idempotent contacts tool writes, keyed by the idempotency key's hash. Ids, states and digests only.",
        warning: Some("Deleting a row lets its idempotency key write again."),
        entity: EntityDescriptor::of::<store::OperationRecord>(),
        primary_key: &["keyHash"],
        can_delete: None,
        after_delete: None,
    }];
    if !configured {
        tracing::info!(target: LOG, "Disabled: no iCloud credentials configured");
        return Ok(subsystem);
    }
    subsystem.boot_steps.push(BootStep {
        phase: BootPhase::Reconcile,
        name: "contacts-reconcile-interrupted-writes",
        run: Box::new(|ctx: AppContext| {
            Box::pin(async move {
                let settled = service::reconcile_interrupted(&ctx.store, ctx.clock.now_ms())
                    .await
                    .map_err(|e| {
                        BootError::new("contacts-reconcile-interrupted-writes", e.to_string())
                    })?;
                if settled > 0 {
                    tracing::warn!(
                        target: LOG,
                        "Marked {settled} interrupted contacts write(s) uncertain or failed"
                    );
                }
                Ok(())
            })
        }),
    });
    Ok(subsystem)
}
