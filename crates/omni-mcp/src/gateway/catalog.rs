//! Integration catalogs: what an integration offers and how it is served.
//!
//! A tool entry is served exactly as the integration listed it (title,
//! description, schemas, annotations, icons, `_meta`), with only `name`
//! replaced by the gateway-owned name and `_meta["io.omni/gateway"]` added
//! to record where it came from. Entries without a usable name are skipped
//! with a warning; a duplicate served name keeps the first entry.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::model::CatalogRow;
use super::names::{exposed_event_name, exposed_tool_name};

/// The `_meta` key recording a served entry's origin.
pub const ORIGIN_META_KEY: &str = "io.omni/gateway";
/// The MCP Events extension key (SEP-3415).
pub const EVENTS_EXTENSION: &str = "io.modelcontextprotocol/events";
/// Bounds on what one integration may contribute.
pub const MAX_TOOLS: usize = 2_000;
pub const MAX_RESOURCES: usize = 5_000;
const MAX_NAME: usize = 128;

#[derive(Clone, Debug, PartialEq)]
pub struct CatalogTool {
    pub upstream: String,
    pub exposed: String,
    /// The served `tools/list` entry.
    pub listed: Value,
}

impl CatalogTool {
    fn hint(&self, key: &str) -> Option<bool> {
        self.listed
            .get("annotations")
            .and_then(|a| a.get(key))
            .and_then(Value::as_bool)
    }

    pub fn read_only(&self) -> Option<bool> {
        self.hint("readOnlyHint")
    }

    pub fn destructive(&self) -> Option<bool> {
        self.hint("destructiveHint")
    }

    pub fn open_world(&self) -> Option<bool> {
        self.hint("openWorldHint")
    }

    pub fn idempotent(&self) -> Option<bool> {
        self.hint("idempotentHint")
    }

    pub fn title(&self) -> Option<&str> {
        self.listed
            .get("title")
            .or_else(|| self.listed.get("annotations").and_then(|a| a.get("title")))
            .and_then(Value::as_str)
    }

    pub fn description(&self) -> Option<&str> {
        self.listed.get("description").and_then(Value::as_str)
    }
}

/// An integration event as Omni offers it downstream.
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogEvent {
    pub upstream: String,
    pub exposed: String,
    /// The integration's `events/list` entry.
    pub definition: Value,
    /// Omni can follow it (the integration offers `events/poll` delivery).
    pub pollable: bool,
}

impl CatalogEvent {
    pub fn input_schema(&self) -> Value {
        self.definition
            .get("inputSchema")
            .filter(|s| s.is_object())
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"}))
    }

    /// The downstream `events/list` entry: Omni delivers by webhook.
    pub fn served(&self, integration_name: &str) -> Value {
        let mut entry = self.definition.as_object().cloned().unwrap_or_default();
        entry.insert("name".into(), json!(self.exposed));
        let description = entry
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        entry.insert(
            "description".into(),
            json!(
                format!("[{integration_name}] {description}")
                    .trim_end()
                    .to_owned()
            ),
        );
        entry.insert("delivery".into(), json!(["webhook"]));
        entry.insert("inputSchema".into(), self.input_schema());
        Value::Object(entry)
    }
}

/// A parsed catalog; cheap to share.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IntegrationCatalog {
    pub fetched_at: i64,
    /// The `initialize` result.
    pub server: Value,
    pub tools: Vec<CatalogTool>,
    pub resources: Vec<Value>,
    pub templates: Vec<Value>,
    pub events: Vec<CatalogEvent>,
    pub warnings: Vec<String>,
}

impl IntegrationCatalog {
    pub fn capabilities(&self) -> &Value {
        self.server.get("capabilities").unwrap_or(&Value::Null)
    }

    pub fn server_info(&self) -> Option<&Map<String, Value>> {
        self.server.get("serverInfo").and_then(Value::as_object)
    }

    pub fn protocol_version(&self) -> Option<&str> {
        self.server.get("protocolVersion").and_then(Value::as_str)
    }

    pub fn tool(&self, upstream: &str) -> Option<&CatalogTool> {
        self.tools.iter().find(|tool| tool.upstream == upstream)
    }

    pub fn event(&self, exposed: &str) -> Option<&CatalogEvent> {
        self.events.iter().find(|event| event.exposed == exposed)
    }
}

/// Whether `initialize` capabilities offer MCP Events.
pub fn offers_events(capabilities: &Value) -> bool {
    capabilities.get("events").is_some()
        || capabilities
            .get("extensions")
            .and_then(|e| e.get(EVENTS_EXTENSION))
            .is_some()
}

fn usable_name(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|name| {
        !name.is_empty() && name.len() <= MAX_NAME && !name.contains(char::is_control)
    })
}

/// Served tool entries for integration `id`.
pub fn build_tools(id: &str, raw: &[Value], warnings: &mut Vec<String>) -> Vec<CatalogTool> {
    let mut tools = Vec::new();
    let mut seen = HashSet::new();
    for entry in raw.iter().take(MAX_TOOLS) {
        let Some(object) = entry.as_object() else {
            warnings.push("skipped a tool entry that is not an object".into());
            continue;
        };
        let Some(upstream) = usable_name(object.get("name")) else {
            warnings.push("skipped a tool without a usable name".into());
            continue;
        };
        let exposed = exposed_tool_name(id, upstream);
        if !seen.insert(exposed.clone()) {
            warnings.push(format!("skipped duplicate tool {upstream}"));
            continue;
        }
        let mut listed = object.clone();
        listed.insert("name".into(), json!(exposed));
        if !listed.get("inputSchema").is_some_and(Value::is_object) {
            warnings.push(format!(
                "{upstream} has no input schema; serving an empty one"
            ));
            listed.insert("inputSchema".into(), json!({"type": "object"}));
        }
        let mut meta = listed
            .get("_meta")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        meta.insert(
            ORIGIN_META_KEY.into(),
            json!({"integration": id, "tool": upstream}),
        );
        listed.insert("_meta".into(), Value::Object(meta));
        tools.push(CatalogTool {
            upstream: upstream.to_owned(),
            exposed,
            listed: Value::Object(listed),
        });
    }
    if raw.len() > MAX_TOOLS {
        warnings.push(format!("served only the first {MAX_TOOLS} tools"));
    }
    tools
}

/// Served event entries for integration `id`.
pub fn build_events(id: &str, raw: &[Value], warnings: &mut Vec<String>) -> Vec<CatalogEvent> {
    let mut events = Vec::new();
    let mut seen = HashSet::new();
    for entry in raw {
        let Some(upstream) = usable_name(entry.get("name")) else {
            warnings.push("skipped an event without a usable name".into());
            continue;
        };
        if !seen.insert(upstream.to_owned()) {
            continue;
        }
        let pollable = entry
            .get("delivery")
            .and_then(Value::as_array)
            .is_some_and(|modes| modes.iter().any(|m| m.as_str() == Some("poll")));
        events.push(CatalogEvent {
            upstream: upstream.to_owned(),
            exposed: exposed_event_name(id, upstream),
            definition: entry.clone(),
            pollable,
        });
    }
    events
}

fn parse_list(json: &str) -> Vec<Value> {
    match serde_json::from_str::<Value>(json) {
        Ok(Value::Array(items)) => items,
        _ => Vec::new(),
    }
}

/// Rebuilds the parsed catalog from its stored row.
pub fn from_row(id: &str, row: &CatalogRow) -> IntegrationCatalog {
    let mut warnings = Vec::new();
    let tools = build_tools(id, &parse_list(&row.tools_json), &mut warnings);
    let events = build_events(id, &parse_list(&row.events_json), &mut warnings);
    IntegrationCatalog {
        fetched_at: row.fetched_at,
        server: serde_json::from_str(&row.server_json).unwrap_or(Value::Null),
        tools,
        resources: parse_list(&row.resources_json),
        templates: parse_list(&row.templates_json),
        events,
        warnings,
    }
}

/// Tools added, removed and changed (any served field) between catalogs,
/// by upstream name, in the new catalog's order.
pub fn diff(
    previous: &[CatalogTool],
    next: &[CatalogTool],
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let before: HashMap<&str, &CatalogTool> =
        previous.iter().map(|t| (t.upstream.as_str(), t)).collect();
    let after: HashSet<&str> = next.iter().map(|t| t.upstream.as_str()).collect();
    let mut added = Vec::new();
    let mut changed = Vec::new();
    for tool in next {
        match before.get(tool.upstream.as_str()) {
            None => added.push(tool.upstream.clone()),
            Some(old) if old.listed != tool.listed => changed.push(tool.upstream.clone()),
            Some(_) => {}
        }
    }
    let removed = previous
        .iter()
        .filter(|t| !after.contains(t.upstream.as_str()))
        .map(|t| t.upstream.clone())
        .collect();
    (added, removed, changed)
}

/// The regular expression an RFC 6570 URI template matches (levels 1 to 4,
/// without length modifiers); `None` when the template is not usable.
pub fn template_regex(template: &str) -> Option<regex::Regex> {
    let mut pattern = String::from("^");
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        pattern.push_str(&regex::escape(&rest[..open]));
        let close = rest[open..].find('}')? + open;
        let expression = &rest[open + 1..close];
        let (operator, names) = match expression.chars().next()? {
            op @ ('+' | '#' | '.' | '/' | ';' | '?' | '&') => (op, &expression[1..]),
            _ => (' ', expression),
        };
        if names.is_empty() {
            return None;
        }
        pattern.push_str(match operator {
            '+' | '#' => "[^?#]*",
            '.' => "(?:\\.[^/?#]*)*",
            '/' => "(?:/[^/?#]*)*",
            ';' => "(?:;[^/?#]*)*",
            '?' | '&' => "(?:[?&][^#]*)?",
            _ => "[^/?#]*",
        });
        rest = &rest[close + 1..];
    }
    if rest.contains('}') {
        return None;
    }
    pattern.push_str(&regex::escape(rest));
    pattern.push('$');
    regex::Regex::new(&pattern).ok()
}

/// Every integration's served tools merged in integration-id order, plus the
/// routing table; rebuilt whenever a catalog or the visibility changes.
#[derive(Clone, Debug, Default)]
pub struct ServedCatalog {
    pub tools: Vec<Value>,
    /// served name -> (integration, upstream name)
    pub routes: HashMap<String, (String, String)>,
    pub resources: Vec<Value>,
    pub templates: Vec<Value>,
    /// resource URI -> integration (first listing wins)
    pub resource_routes: HashMap<String, String>,
    /// URI templates -> integration
    pub template_routes: Vec<(regex::Regex, String)>,
    /// Integrations that declared the resources capability.
    pub resource_integrations: Vec<String>,
    /// A digest of the served tool list (notifies clients on change).
    pub tools_digest: String,
    pub resources_digest: String,
}

/// One enabled integration's contribution to [`ServedCatalog::merge`].
pub struct Contribution<'a> {
    pub id: &'a str,
    pub catalog: Arc<IntegrationCatalog>,
    pub hidden: &'a [String],
}

impl ServedCatalog {
    pub fn merge(contributions: &[Contribution<'_>]) -> Self {
        let mut served = ServedCatalog::default();
        for contribution in contributions {
            let catalog = &contribution.catalog;
            for tool in &catalog.tools {
                if contribution.hidden.contains(&tool.upstream)
                    || served.routes.contains_key(&tool.exposed)
                {
                    continue;
                }
                served.routes.insert(
                    tool.exposed.clone(),
                    (contribution.id.to_owned(), tool.upstream.clone()),
                );
                served.tools.push(tool.listed.clone());
            }
            if catalog.capabilities().get("resources").is_some() {
                served
                    .resource_integrations
                    .push(contribution.id.to_owned());
            }
            for resource in catalog.resources.iter().take(MAX_RESOURCES) {
                if let Some(uri) = resource.get("uri").and_then(Value::as_str) {
                    served
                        .resource_routes
                        .entry(uri.to_owned())
                        .or_insert_with(|| contribution.id.to_owned());
                }
                served.resources.push(resource.clone());
            }
            for template in &catalog.templates {
                if let Some(matcher) = template
                    .get("uriTemplate")
                    .and_then(Value::as_str)
                    .and_then(template_regex)
                {
                    served
                        .template_routes
                        .push((matcher, contribution.id.to_owned()));
                }
                served.templates.push(template.clone());
            }
        }
        served.tools_digest =
            omni_core::digest::sha256_hex(serde_json::to_vec(&served.tools).unwrap_or_default());
        served.resources_digest = omni_core::digest::sha256_hex(
            serde_json::to_vec(&(&served.resources, &served.templates)).unwrap_or_default(),
        );
        served
    }

    /// The integration that listed `uri`.
    pub fn listed_owner(&self, uri: &str) -> Option<&str> {
        self.resource_routes.get(uri).map(String::as_str)
    }

    /// The integration whose templates match `uri`, when exactly one does.
    /// An ambiguous URI routes nowhere, so one integration's broad template
    /// never captures another integration's reads.
    pub fn template_owner(&self, uri: &str) -> Option<&str> {
        let mut owners = self
            .template_routes
            .iter()
            .filter(|(matcher, _)| matcher.is_match(uri))
            .map(|(_, owner)| owner.as_str());
        let first = owners.next()?;
        owners.all(|other| other == first).then_some(first)
    }

    /// The only integration offering resources, if exactly one does.
    pub fn sole_resource_integration(&self) -> Option<&str> {
        match self.resource_integrations.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, description: &str) -> Value {
        json!({"name": name, "description": description, "inputSchema": {"type": "object"}, "annotations": {"readOnlyHint": true}})
    }

    #[test]
    fn tools_keep_every_field_but_the_name() {
        let mut warnings = Vec::new();
        let raw = vec![
            json!({"name": "get_issue", "title": "Get issue", "description": "d", "inputSchema": {"type": "object", "properties": {"n": {"type": "integer"}}}, "outputSchema": {"type": "object"}, "annotations": {"readOnlyHint": true}, "icons": [{"src": "https://x/i.png"}], "_meta": {"x": 1}}),
            json!({"name": "get_issue", "inputSchema": {}}),
            json!({"description": "nameless"}),
            json!({"name": "no_schema"}),
        ];
        let tools = build_tools("github", &raw, &mut warnings);
        assert_eq!(tools.len(), 2);
        let first = &tools[0].listed;
        assert_eq!(first["name"], "github__get_issue");
        assert_eq!(first["title"], "Get issue");
        assert_eq!(first["outputSchema"], json!({"type": "object"}));
        assert_eq!(first["icons"][0]["src"], "https://x/i.png");
        assert_eq!(first["_meta"]["x"], 1);
        assert_eq!(first["_meta"][ORIGIN_META_KEY]["tool"], "get_issue");
        assert_eq!(tools[0].read_only(), Some(true));
        assert_eq!(tools[1].listed["inputSchema"], json!({"type": "object"}));
        assert_eq!(warnings.len(), 3, "{warnings:?}");
    }

    #[test]
    fn diff_reports_added_removed_and_changed() {
        let mut w = Vec::new();
        let before = build_tools("a", &[tool("x", "1"), tool("y", "1")], &mut w);
        let after = build_tools("a", &[tool("y", "2"), tool("z", "1")], &mut w);
        assert_eq!(
            diff(&before, &after),
            (vec!["z".into()], vec!["x".into()], vec!["y".into()])
        );
    }

    #[test]
    fn merge_orders_hides_and_routes() {
        let mut w = Vec::new();
        let a = Arc::new(IntegrationCatalog {
            tools: build_tools("a", &[tool("one", ""), tool("two", "")], &mut w),
            server: json!({"capabilities": {"resources": {}}}),
            resources: vec![json!({"uri": "repo://x/readme", "name": "readme"})],
            templates: vec![json!({"uriTemplate": "repo://{owner}/{name}"})],
            ..Default::default()
        });
        let b = Arc::new(IntegrationCatalog {
            tools: build_tools("b", &[tool("one", "")], &mut w),
            ..Default::default()
        });
        let hidden = vec!["two".to_owned()];
        let served = ServedCatalog::merge(&[
            Contribution {
                id: "a",
                catalog: a,
                hidden: &hidden,
            },
            Contribution {
                id: "b",
                catalog: b,
                hidden: &[],
            },
        ]);
        let names: Vec<&str> = served
            .tools
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert_eq!(names, ["a__one", "b__one"]);
        assert_eq!(served.routes["b__one"], ("b".to_owned(), "one".to_owned()));
        assert_eq!(served.listed_owner("repo://x/readme"), Some("a"));
        assert_eq!(served.template_owner("repo://other/thing"), Some("a"));
        assert_eq!(served.template_owner("file:///etc"), None);
        assert_eq!(served.sole_resource_integration(), Some("a"));
    }

    #[test]
    fn uri_templates_match_like_rfc_6570() {
        let simple = template_regex("repo://{owner}/{repo}/contents{/path*}").unwrap();
        assert!(simple.is_match("repo://octo/hello/contents"));
        assert!(simple.is_match("repo://octo/hello/contents/src/main.rs"));
        assert!(!simple.is_match("repo://octo/contents"));
        assert!(!simple.is_match("other://octo/hello/contents"));
        let reserved = template_regex("https://docs.example/{+path}{?q}").unwrap();
        assert!(reserved.is_match("https://docs.example/a/b/c?q=1"));
        assert!(template_regex("broken{").is_none());
        assert!(template_regex("x{}").is_none());
    }

    #[test]
    fn ambiguous_templates_route_nowhere() {
        let catalog = |templates: Vec<Value>| {
            Arc::new(IntegrationCatalog {
                server: json!({"capabilities": {"resources": {}}}),
                templates,
                ..Default::default()
            })
        };
        let served = ServedCatalog::merge(&[
            Contribution {
                id: "gh",
                catalog: catalog(vec![json!({"uriTemplate": "repo://{owner}/{repo}"})]),
                hidden: &[],
            },
            Contribution {
                id: "evil",
                catalog: catalog(vec![json!({"uriTemplate": "{+anything}"})]),
                hidden: &[],
            },
            Contribution {
                id: "docs",
                catalog: catalog(vec![json!({"uriTemplate": "docs://{page}"})]),
                hidden: &[],
            },
        ]);
        assert_eq!(
            served.template_owner("repo://octo/hello"),
            None,
            "two integrations match"
        );
        assert_eq!(served.template_owner("mem://x"), Some("evil"));
        assert_eq!(served.sole_resource_integration(), None);
    }

    #[test]
    fn events_note_poll_support() {
        let mut w = Vec::new();
        let events = build_events(
            "artifacts",
            &[
                json!({"name": "comment.created", "delivery": ["poll", "webhook"], "inputSchema": {"type": "object"}}),
                json!({"name": "push.only", "delivery": ["push"]}),
            ],
            &mut w,
        );
        assert_eq!(events[0].exposed, "artifacts__comment.created");
        assert!(events[0].pollable);
        assert!(!events[1].pollable);
        let served = events[0].served("Artifacts");
        assert_eq!(served["delivery"], json!(["webhook"]));
        assert!(
            served["description"]
                .as_str()
                .unwrap()
                .starts_with("[Artifacts]")
        );
    }
}
