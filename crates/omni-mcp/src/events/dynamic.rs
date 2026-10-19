//! Events beyond the fixed catalog, and owners beyond Executor's.
//!
//! The gateway offers its integrations' events (`<integration>__<event>`)
//! through the same outbox, and its subscriptions are owned by gateway
//! principals (`gateway:client:<id>`, `gateway:grant:<id>`). Both are
//! registered once with [`super::service::McpEventService::extend`]; without
//! them the service behaves exactly as before.

use futures::future::BoxFuture;
use serde_json::{Map, Value};

use super::catalog::{EventArguments, EventDefinition, event_definition};

/// Integration events the outbox can carry.
pub trait DynamicEvents: Send + Sync {
    /// Whether `name` is such an event now.
    fn knows(&self, name: &str) -> bool;
    /// Canonical arguments, or `None` when they are invalid for `name`.
    fn parse_arguments(&self, name: &str, raw: &Value) -> Option<EventArguments>;
}

/// Whether a gateway owner still has access (for delivery-time checks).
pub trait OwnerCheck: Send + Sync {
    fn is_active<'a>(&'a self, owner: &'a str) -> BoxFuture<'a, bool>;
}

/// What the service registers.
pub struct EventExtensions {
    pub dynamic: std::sync::Arc<dyn DynamicEvents>,
    pub owners: std::sync::Arc<dyn OwnerCheck>,
}

/// One event the outbox knows.
#[derive(Clone, Copy)]
pub enum Definition<'a> {
    Static(&'static EventDefinition),
    Dynamic(&'a str, &'a dyn DynamicEvents),
}

impl Definition<'_> {
    pub fn resolve<'a>(
        name: &'a str,
        dynamic: Option<&'a dyn DynamicEvents>,
    ) -> Option<Definition<'a>> {
        if let Some(definition) = event_definition(name) {
            return Some(Definition::Static(definition));
        }
        let dynamic = dynamic?;
        dynamic
            .knows(name)
            .then_some(Definition::Dynamic(name, dynamic))
    }

    pub fn name(&self) -> &str {
        match self {
            Definition::Static(definition) => definition.name,
            Definition::Dynamic(name, _) => name,
        }
    }

    pub fn parse_arguments(&self, raw: &Value) -> Option<EventArguments> {
        match self {
            Definition::Static(definition) => definition.parse_arguments(raw),
            Definition::Dynamic(name, dynamic) => dynamic.parse_arguments(name, raw),
        }
    }

    /// Whether a payload reaches a subscription with `args`. Integration
    /// events are published for one argument set (`publish_targeted`), and a
    /// subscription's id fixes its arguments, so they always match here.
    pub fn matches(&self, args: &EventArguments, data: &Map<String, Value>) -> bool {
        match self {
            Definition::Static(definition) => definition.matches(args, data),
            Definition::Dynamic(..) => true,
        }
    }
}

/// Integration event arguments as canonical strings, without checking an
/// event's schema (`null` is no arguments; values are strings up to 500
/// bytes).
pub fn string_arguments(raw: &Value) -> Option<EventArguments> {
    let object = match raw {
        Value::Null => return Some(EventArguments::new()),
        Value::Object(object) => object,
        _ => return None,
    };
    let mut args = EventArguments::new();
    for (key, value) in object {
        match value {
            Value::String(text) if text.len() <= 500 => {
                args.insert(key.clone(), text.clone());
            }
            Value::Null => {}
            _ => return None,
        }
    }
    args.sort_keys();
    Some(args)
}

/// Whether `owner` is a gateway principal.
pub fn is_gateway_owner(owner: &str) -> bool {
    owner.starts_with(crate::gateway::access::OWNER_PREFIX)
}
