//! MCP tool registration.
//!
//! Each tool's public contract (name, title, description, annotations, policy and
//! input/output schemas) is a [`ToolDef`] declared next to its handler; the schemas
//! are derived from `schemars::JsonSchema` types ([`schema`]). [`typed_tool`]
//! validates inputs and outputs against those schemas; [`registry::ToolRegistry`]
//! formats results and serves the tools through
//! rmcp ([`registry::RegistryServer`]). `golden/tools-list.json` and
//! `golden/mcp-policy.json` are snapshots generated from the definitions by
//! `cargo xtask mcp-golden`.

use std::future::Future;
use std::marker::PhantomData;
use std::sync::{Arc, OnceLock};

use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

pub mod golden;
pub mod registry;
pub mod schema;

/// A tool's recommended approval policy (`docs/mcp-policy.json`), for the
/// connected client to apply before calling it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    Allow,
    RequireApproval,
    Block,
}

/// MCP tool annotations (all four hints are always present).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Annotations {
    pub read_only_hint: bool,
    pub destructive_hint: bool,
    pub idempotent_hint: bool,
    pub open_world_hint: bool,
}

/// `ToolPolicy`: side effects, cost and the recommended approval policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolPolicy {
    pub side_effects: Vec<String>,
    pub cost: String,
    pub recommended_policy: ApprovalPolicy,
}

/// The metadata of one tool, as served by `tools/list` plus its policy.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolMeta {
    pub name: String,
    pub title: String,
    pub description: String,
    pub input_schema: Arc<Map<String, Value>>,
    pub output_schema: Arc<Map<String, Value>>,
    pub annotations: Annotations,
    pub policy: ToolPolicy,
}

impl ToolMeta {
    /// The `tools/list` entry: name, title, description, inputSchema, annotations,
    /// outputSchema, in that order.
    pub fn listed(&self) -> Map<String, Value> {
        let mut tool = Map::new();
        tool.insert("name".into(), Value::String(self.name.clone()));
        tool.insert("title".into(), Value::String(self.title.clone()));
        tool.insert(
            "description".into(),
            Value::String(self.description.clone()),
        );
        tool.insert(
            "inputSchema".into(),
            Value::Object((*self.input_schema).clone()),
        );
        tool.insert(
            "annotations".into(),
            serde_json::to_value(self.annotations).unwrap_or(Value::Null),
        );
        tool.insert(
            "outputSchema".into(),
            Value::Object((*self.output_schema).clone()),
        );
        tool
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolMetaError {
    #[error("invalid schema for MCP tool {tool:?}: {reason}")]
    InvalidSchema { tool: String, reason: String },
    #[error("invalid golden MCP handshake: {0}")]
    Handshake(String),
}

/// A tool's recommended policy: side effects, cost and approval policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub side_effects: &'static [&'static str],
    pub cost: &'static str,
    pub recommended: ApprovalPolicy,
}

/// The hand-written part of a tool's contract; [`ToolDef`] adds the schemas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolInfo {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub annotations: Annotations,
    pub policy: Policy,
}

/// A tool's complete public contract: [`ToolInfo`] plus the input schema derived
/// from `I` and the output schema derived from `O`. Declared as a `static` next to
/// the handler; the metadata is built once on first use.
pub struct ToolDef<I, O> {
    info: ToolInfo,
    meta: OnceLock<Result<ToolMeta, String>>,
    schemas: PhantomData<fn() -> (I, O)>,
}

impl<I, O> ToolDef<I, O> {
    pub const fn new(info: ToolInfo) -> Self {
        Self {
            info,
            meta: OnceLock::new(),
            schemas: PhantomData,
        }
    }

    pub const fn info(&self) -> &ToolInfo {
        &self.info
    }
}

/// A [`ToolDef`] with its schema types erased, for listing a package's tools.
pub trait ToolDefinition: Sync {
    fn name(&self) -> &'static str;
    /// The metadata with derived schemas; fails when a schema leaves the served
    /// dialect or does not compile as a validator.
    fn meta(&self) -> Result<&ToolMeta, ToolMetaError>;
}

impl<I: JsonSchema, O: JsonSchema> ToolDefinition for ToolDef<I, O> {
    fn name(&self) -> &'static str {
        self.info.name
    }

    fn meta(&self) -> Result<&ToolMeta, ToolMetaError> {
        let name = self.info.name;
        self.meta
            .get_or_init(|| {
                build_meta(
                    &self.info,
                    schema::input_schema::<I>,
                    schema::output_schema::<O>,
                )
            })
            .as_ref()
            .map_err(|reason| ToolMetaError::InvalidSchema {
                tool: name.to_owned(),
                reason: reason.clone(),
            })
    }
}

type SchemaFn = fn() -> Result<Map<String, Value>, schema::SchemaError>;

fn build_meta(info: &ToolInfo, input: SchemaFn, output: SchemaFn) -> Result<ToolMeta, String> {
    let build = |which: &str, f: SchemaFn| -> Result<Map<String, Value>, String> {
        let schema = f().map_err(|e| format!("{which} schema {e}"))?;
        schema::check_dialect(&schema).map_err(|e| format!("{which} schema {e}"))?;
        SchemaValidator::new(info.name, &schema).map_err(|e| e.to_string())?;
        Ok(schema)
    };
    Ok(ToolMeta {
        name: info.name.to_owned(),
        title: info.title.to_owned(),
        description: info.description.to_owned(),
        input_schema: Arc::new(build("input", input)?),
        output_schema: Arc::new(build("output", output)?),
        annotations: info.annotations,
        policy: ToolPolicy {
            side_effects: info
                .policy
                .side_effects
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            cost: info.policy.cost.to_owned(),
            recommended_policy: info.policy.recommended,
        },
    })
}

/// A compiled JSON-schema validator (draft 2020-12, formats asserted).
///
/// `pattern` keywords follow ECMAScript regex semantics, as clients read them: `\d` and `\w`
/// are ASCII-only and `\s` is JS whitespace (U+FEFF but not U+0085). `integer`
/// accepts integral floats such as `2.0` (JS has one number type).
pub struct SchemaValidator {
    validator: jsonschema::Validator,
}

impl SchemaValidator {
    pub fn new(tool: &str, schema: &Map<String, Value>) -> Result<Self, ToolMetaError> {
        let validator = jsonschema::options()
            .should_validate_formats(true)
            .build(&Value::Object(schema.clone()))
            .map_err(|e| ToolMetaError::InvalidSchema {
                tool: tool.to_owned(),
                reason: e.to_string(),
            })?;
        Ok(Self { validator })
    }

    /// `Ok` or every violation as `"<path>: <message>"`, joined by `; `.
    pub fn check(&self, value: &Value) -> Result<(), String> {
        let errors: Vec<String> = self
            .validator
            .iter_errors(value)
            .map(|e| {
                let path = e.instance_path().to_string();
                if path.is_empty() {
                    e.to_string()
                } else {
                    format!("{path}: {e}")
                }
            })
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

/// Per-call context.
#[derive(Clone, Debug)]
pub struct ToolContext {
    pub call_id: String,
    pub cancel: CancellationToken,
}

/// An MCP content block, serialized verbatim (`{"type":"text",...}`, resources, images).
pub type Content = Map<String, Value>;

/// A tool result: structured content, optionally with a custom content list.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolOutput {
    Structured(Map<String, Value>),
    Custom {
        structured: Map<String, Value>,
        content: Vec<Content>,
    },
}

/// Where a tool call failed (`McpToolError.phase`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolPhase {
    Input,
    Execute,
    Output,
}

/// A failed tool call; `message` is the innermost cause (`toolErrorMessage`).
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
#[error("{message}")]
pub struct ToolError {
    pub phase: ToolPhase,
    pub message: String,
}

impl ToolError {
    pub fn input(message: impl Into<String>) -> Self {
        Self {
            phase: ToolPhase::Input,
            message: message.into(),
        }
    }

    pub fn execute(message: impl Into<String>) -> Self {
        Self {
            phase: ToolPhase::Execute,
            message: message.into(),
        }
    }

    /// An execute-phase error from the innermost cause of `error`.
    pub fn execute_from(error: &dyn std::error::Error) -> Self {
        Self::execute(omni_core::error::chain_message(error))
    }

    pub fn output(message: impl Into<String>) -> Self {
        Self {
            phase: ToolPhase::Output,
            message: message.into(),
        }
    }
}

pub trait ToolHandler: Send + Sync {
    fn call<'a>(
        &'a self,
        input: Value,
        cx: ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>>;
}

/// A registered tool: its metadata plus its handler.
#[derive(Clone)]
pub struct McpTool {
    pub meta: &'static ToolMeta,
    pub handler: Arc<dyn ToolHandler>,
}

struct TypedHandler<I, O, F> {
    f: F,
    input: Option<SchemaValidator>,
    output: Option<SchemaValidator>,
    _types: std::marker::PhantomData<fn(I) -> O>,
}

impl<I, O, F, Fut> ToolHandler for TypedHandler<I, O, F>
where
    I: DeserializeOwned + Send + 'static,
    O: Serialize + Send + 'static,
    F: Fn(I, ToolContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<O, ToolError>> + Send + 'static,
{
    fn call<'a>(
        &'a self,
        input: Value,
        cx: ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        // JS has one number type: `2.0` decodes wherever an integer is expected.
        let input = omni_core::js::normalize_numbers(input);
        let checked = match &self.input {
            Some(validator) => validator.check(&input).map_err(ToolError::input),
            None => Ok(()),
        };
        let decoded = checked.and_then(|()| {
            serde_json::from_value::<I>(input).map_err(|e| ToolError::input(e.to_string()))
        });
        let fut = decoded.map(|input| (self.f)(input, cx));
        Box::pin(async move {
            let output = fut?.await?;
            let value =
                serde_json::to_value(output).map_err(|e| ToolError::output(e.to_string()))?;
            // Whole-number doubles serialize as JS prints them (`12`, not `12.0`).
            let value = omni_core::js::normalize_numbers(value);
            if let Some(validator) = &self.output {
                validator.check(&value).map_err(ToolError::output)?;
            }
            match value {
                Value::Object(map) => Ok(ToolOutput::Structured(map)),
                _ => Err(ToolError::output("tool output must be a JSON object")),
            }
        })
    }
}

/// Builds a tool from its definition: input is validated against the derived
/// input schema, then decoded into the handler's `D` with serde (explicit defaults
/// apply the advertised defaults); the handler's result must serialize to a JSON object
/// that satisfies the derived output schema. Integral floats in
/// the input decode into integer fields, and whole-number `f64` output fields
/// serialize without a fraction, as in JS.
pub fn typed_tool<I, O, D, R, F, Fut>(
    def: &'static ToolDef<I, O>,
    f: F,
) -> Result<McpTool, ToolMetaError>
where
    I: JsonSchema,
    O: JsonSchema,
    D: DeserializeOwned + Send + 'static,
    R: Serialize + Send + 'static,
    F: Fn(D, ToolContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<R, ToolError>> + Send + 'static,
{
    let meta = def.meta()?;
    Ok(McpTool {
        meta,
        handler: Arc::new(TypedHandler {
            f,
            input: Some(SchemaValidator::new(&meta.name, &meta.input_schema)?),
            output: Some(SchemaValidator::new(&meta.name, &meta.output_schema)?),
            _types: std::marker::PhantomData,
        }),
    })
}

/// A tool with a hand-written handler (custom content via [`ToolOutput::Custom`]);
/// the handler validates its own input.
pub fn raw_tool(
    def: &'static dyn ToolDefinition,
    handler: Arc<dyn ToolHandler>,
) -> Result<McpTool, ToolMetaError> {
    Ok(McpTool {
        meta: def.meta()?,
        handler,
    })
}

/// The metadata of every definition in `defs`, in order.
pub fn metas(
    defs: &[&'static dyn ToolDefinition],
) -> Result<Vec<&'static ToolMeta>, ToolMetaError> {
    defs.iter().map(|def| def.meta()).collect()
}

/// `{items, nextCursor | null, total}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<usize>,
    pub total: usize,
}

/// `paginate()`: `items[cursor..cursor+limit]`, `nextCursor` when more remain.
pub fn paginate<T: Serialize>(items: Vec<T>, cursor: usize, limit: usize) -> Page<T> {
    let total = items.len();
    let page: Vec<T> = items.into_iter().skip(cursor).take(limit).collect();
    let next = cursor.saturating_add(page.len());
    Page {
        items: page,
        next_cursor: (next < total).then_some(next),
        total,
    }
}

/// `truncate()`: first `max` UTF-16 units and whether anything was cut.
pub fn truncate_utf16(s: &str, max: usize) -> (String, bool) {
    (
        omni_core::js::utf16_slice(s, 0, max).into_owned(),
        omni_core::js::utf16_len(s) > max,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagination_cases() {
        let page = paginate(vec![1, 2, 3, 4, 5], 1, 2);
        assert_eq!(
            page,
            Page {
                items: vec![2, 3],
                next_cursor: Some(3),
                total: 5
            }
        );
        let last = paginate(vec![1, 2, 3], 2, 5);
        assert_eq!(
            last,
            Page {
                items: vec![3],
                next_cursor: None,
                total: 3
            }
        );
        let beyond = paginate(vec![1], 9, 5);
        assert_eq!(
            beyond,
            Page {
                items: vec![],
                next_cursor: None,
                total: 1
            }
        );
    }

    #[test]
    fn truncation_counts_utf16() {
        assert_eq!(truncate_utf16("héllo", 3), ("hél".to_owned(), true));
        assert_eq!(truncate_utf16("hi", 3), ("hi".to_owned(), false));
    }

    #[tokio::test]
    async fn typed_handler_decodes_and_encodes() {
        #[derive(Deserialize)]
        struct In {
            n: u32,
        }
        #[derive(Serialize)]
        struct Out {
            doubled: u32,
        }
        let handler = TypedHandler {
            f: |input: In, _cx: ToolContext| async move {
                Ok::<_, ToolError>(Out {
                    doubled: input.n * 2,
                })
            },
            input: None,
            output: None,
            _types: std::marker::PhantomData,
        };
        let cx = || ToolContext {
            call_id: "1".to_owned(),
            cancel: CancellationToken::new(),
        };
        let out = handler.call(serde_json::json!({"n": 2}), cx()).await;
        assert_eq!(
            out,
            Ok(ToolOutput::Structured(
                serde_json::json!({"doubled": 4})
                    .as_object()
                    .cloned()
                    .unwrap_or_default()
            ))
        );
        let bad = handler.call(serde_json::json!({"n": "x"}), cx()).await;
        assert!(matches!(
            bad,
            Err(ToolError {
                phase: ToolPhase::Input,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn numbers_cross_the_handler_as_js_numbers() {
        #[derive(Deserialize)]
        struct In {
            n: u32,
        }
        #[derive(Serialize)]
        struct Out {
            half: f64,
            whole: f64,
        }
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"n": {"type": "integer", "minimum": 0}},
            "required": ["n"],
        });
        let handler = TypedHandler {
            f: |input: In, _cx: ToolContext| async move {
                Ok::<_, ToolError>(Out {
                    half: f64::from(input.n) / 2.0,
                    whole: f64::from(input.n),
                })
            },
            input: SchemaValidator::new(
                "t",
                schema.as_object().cloned().as_ref().unwrap_or(&Map::new()),
            )
            .ok(),
            output: None,
            _types: std::marker::PhantomData,
        };
        let cx = ToolContext {
            call_id: "1".to_owned(),
            cancel: CancellationToken::new(),
        };
        let out = handler.call(serde_json::json!({"n": 3.0}), cx).await;
        let Ok(ToolOutput::Structured(map)) = out else {
            panic!("unexpected {out:?}");
        };
        assert_eq!(
            serde_json::to_string(&map).unwrap_or_default(),
            r#"{"half":1.5,"whole":3}"#
        );
    }

    #[test]
    fn patterns_use_ecmascript_classes() {
        let schema = serde_json::json!({"type": "object", "properties": {
            "d": {"type": "string", "pattern": "^\\d+$"},
            "s": {"type": "string", "pattern": "^\\s+$"},
            "set": {"type": "string", "pattern": "^[\\d\\s]+$"},
        }});
        let validator = SchemaValidator::new("t", schema.as_object().unwrap_or(&Map::new()));
        let Ok(validator) = validator else {
            panic!("schema compiles");
        };
        let check = |key: &str, value: &str| validator.check(&serde_json::json!({key: value}));
        assert!(check("d", "12").is_ok());
        assert!(check("d", "\u{0661}\u{0662}").is_err());
        assert!(check("s", "\u{FEFF}\u{A0}").is_ok());
        assert!(check("s", "\u{85}").is_err());
        assert!(check("set", "1 2").is_ok());
        assert!(check("set", "\u{0663}").is_err());
    }
}
