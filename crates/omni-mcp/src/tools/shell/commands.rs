//! Omni's read-only tools as shell commands.
//!
//! `email_search --from amazon --limit 5` runs `email_search` with
//! `{"from":"amazon","limit":5}` and prints its result as indented JSON.
//! Flags are the input schema's properties, in camelCase or kebab-case;
//! values take the property's type, array flags repeat, and a bare boolean
//! flag means `true`. A single JSON object argument, or a JSON object on
//! stdin with no flags, is passed through as the whole input.

use std::fmt::Write as _;
use std::sync::Arc;

use bashkit::{Builtin, BuiltinContext, ExecResult, async_trait};
use omni_mcp_kit::{McpTool, ToolPhase};
use serde_json::{Map, Value};

use super::run::{Run, Via};

/// One read-only tool the shell can run.
pub(crate) struct Command {
    pub tool: McpTool,
    pub input_schema: Value,
    pub output_schema: Value,
}

impl Command {
    pub fn new(tool: McpTool) -> Self {
        Self {
            input_schema: Value::Object((*tool.meta.input_schema).clone()),
            output_schema: Value::Object((*tool.meta.output_schema).clone()),
            tool,
        }
    }

    pub fn name(&self) -> &str {
        &self.tool.meta.name
    }

    fn properties(&self) -> Option<&Map<String, Value>> {
        self.input_schema
            .get("properties")
            .and_then(Value::as_object)
    }

    fn required(&self) -> Vec<&str> {
        self.input_schema
            .get("required")
            .and_then(Value::as_array)
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }

    /// Whether the tool runs without arguments, and so is also a file.
    /// A preview has nothing to show without a proposed change.
    pub fn runs_bare(&self) -> bool {
        self.required().is_empty()
            && self.input_schema.get("oneOf").is_none()
            && !self.name().ends_with("_preview")
    }

    /// `<tool> --help`.
    pub fn help(&self) -> String {
        let meta = self.tool.meta;
        let mut text = format!("{} - {}\n\n{}\n", meta.name, meta.title, meta.description);
        let required = self.required();
        match self.properties() {
            Some(properties) if !properties.is_empty() => {
                text.push_str("\nFlags:\n");
                for (name, schema) in properties {
                    let _ = write!(text, "  --{}", kebab(name));
                    let kind = describe_kind(schema);
                    if !kind.is_empty() {
                        let _ = write!(text, " <{kind}>");
                    }
                    if required.contains(&name.as_str()) {
                        text.push_str(" (required)");
                    }
                    if let Some(default) = schema.get("default") {
                        let _ = write!(text, " [default {default}]");
                    }
                    if let Some(description) = schema.get("description").and_then(Value::as_str) {
                        let _ = write!(text, "\n      {description}");
                    }
                    text.push('\n');
                }
            }
            _ => {}
        }
        if self.input_schema.get("oneOf").is_some() {
            text.push_str("\nPass the input as one JSON object argument.\n");
        }
        if self.runs_bare() {
            let _ = write!(
                text,
                "\nWith no arguments this is also the file /omni/{}.json.\n",
                meta.name
            );
        }
        text
    }

    /// The tool input from the command's arguments and stdin.
    pub fn input(&self, args: &[String], stdin: Option<&str>) -> Result<Value, String> {
        if let [only] = args
            && only.trim_start().starts_with('{')
        {
            return parse_object(only);
        }
        if args.is_empty() {
            if let Some(text) = stdin.map(str::trim).filter(|t| t.starts_with('{')) {
                return parse_object(text);
            }
            return Ok(Value::Object(Map::new()));
        }
        let mut input = Map::new();
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            let Some(flag) = arg.strip_prefix("--") else {
                return Err(format!(
                    "unexpected argument {arg:?}; pass --flag value pairs or one JSON object"
                ));
            };
            let (flag, inline) = match flag.split_once('=') {
                Some((flag, value)) => (flag, Some(value.to_owned())),
                None => (flag, None),
            };
            let (name, schema) = self.property(flag)?;
            let kind = Kind::of(schema);
            let raw = match inline {
                Some(value) => value,
                None if kind == Kind::Boolean => {
                    // A bare boolean flag is `true` unless a value follows.
                    match rest.clone().next().map(String::as_str) {
                        Some("true" | "false") => rest.next().cloned().unwrap_or_default(),
                        _ => "true".to_owned(),
                    }
                }
                None => rest
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("--{flag} needs a value"))?,
            };
            let value = kind
                .parse(&raw, schema)
                .map_err(|e| format!("--{flag}: {e}"))?;
            match (kind, input.get_mut(&name)) {
                (Kind::Array, Some(Value::Array(items))) => match value {
                    Value::Array(more) => items.extend(more),
                    other => items.push(other),
                },
                _ => {
                    input.insert(name, value);
                }
            }
        }
        Ok(Value::Object(input))
    }

    /// The property a flag names: exact, or ignoring case, `-` and `_`.
    fn property(&self, flag: &str) -> Result<(String, &Value), String> {
        let properties = self.properties();
        let wanted = fold(flag);
        properties
            .and_then(|properties| {
                properties
                    .get(flag)
                    .map(|schema| (flag.to_owned(), schema))
                    .or_else(|| {
                        properties
                            .iter()
                            .find(|(name, _)| fold(name) == wanted)
                            .map(|(name, schema)| (name.clone(), schema))
                    })
            })
            .ok_or_else(|| {
                let known: Vec<String> = properties
                    .map(|p| p.keys().map(|k| format!("--{}", kebab(k))).collect())
                    .unwrap_or_default();
                if known.is_empty() {
                    format!("unknown flag --{flag}; this tool takes no flags")
                } else {
                    format!("unknown flag --{flag}; flags: {}", known.join(" "))
                }
            })
    }
}

fn parse_object(text: &str) -> Result<Value, String> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Ok(Value::Object(map)),
        Ok(_) => Err("the JSON input must be an object".to_owned()),
        Err(e) => Err(format!("invalid JSON input: {e}")),
    }
}

fn fold(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '-' && *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// `bodyChars` -> `body-chars`.
fn kebab(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            out.push('-');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn describe_kind(schema: &Value) -> String {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return values
            .iter()
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
            .collect::<Vec<_>>()
            .join("|");
    }
    match Kind::of(schema) {
        Kind::String => "text",
        Kind::Integer => "integer",
        Kind::Number => "number",
        Kind::Boolean => "",
        Kind::Array => "value, repeatable",
        Kind::Object => "json",
        Kind::Any => "value",
    }
    .to_owned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    String,
    Integer,
    Number,
    Boolean,
    Array,
    Object,
    Any,
}

impl Kind {
    fn of(schema: &Value) -> Self {
        let named = |name: &str| match name {
            "string" => Some(Self::String),
            "integer" => Some(Self::Integer),
            "number" => Some(Self::Number),
            "boolean" => Some(Self::Boolean),
            "array" => Some(Self::Array),
            "object" => Some(Self::Object),
            _ => None,
        };
        match schema.get("type") {
            Some(Value::String(name)) => named(name).unwrap_or(Self::Any),
            Some(Value::Array(names)) => names
                .iter()
                .filter_map(Value::as_str)
                .find_map(named)
                .unwrap_or(Self::Any),
            _ => schema
                .get("anyOf")
                .or_else(|| schema.get("oneOf"))
                .and_then(Value::as_array)
                .and_then(|branches| {
                    branches
                        .iter()
                        .map(Self::of)
                        .find(|kind| *kind != Self::Any)
                })
                .unwrap_or(Self::Any),
        }
    }

    fn parse(self, raw: &str, schema: &Value) -> Result<Value, String> {
        match self {
            Self::String => Ok(Value::String(raw.to_owned())),
            Self::Integer => raw
                .parse::<i64>()
                .map(Value::from)
                .map_err(|_| format!("{raw:?} is not an integer")),
            Self::Number => raw
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .ok_or_else(|| format!("{raw:?} is not a number")),
            Self::Boolean => match raw {
                "true" => Ok(Value::Bool(true)),
                "false" => Ok(Value::Bool(false)),
                _ => Err(format!("{raw:?} is not true or false")),
            },
            Self::Array => {
                if raw.trim_start().starts_with('[')
                    && let Ok(items @ Value::Array(_)) = serde_json::from_str::<Value>(raw)
                {
                    return Ok(items);
                }
                let item = schema.get("items").unwrap_or(&Value::Null);
                Self::of(item).parse(raw, item)
            }
            Self::Object => {
                serde_json::from_str::<Value>(raw).map_err(|e| format!("invalid JSON object: {e}"))
            }
            Self::Any => Ok(serde_json::from_str::<Value>(raw)
                .unwrap_or_else(|_| Value::String(raw.to_owned()))),
        }
    }
}

/// The run of the current `exec`, handed to builtins as an extension.
pub(crate) fn current_run(ctx: &BuiltinContext<'_>) -> Option<Arc<Run>> {
    ctx.execution_extension::<Arc<Run>>()?
        .try_with(Arc::clone)
        .ok()
}

/// A builtin that runs one tool.
pub(crate) struct ToolCommand(pub Arc<Command>);

#[async_trait]
impl Builtin for ToolCommand {
    async fn execute(&self, ctx: BuiltinContext<'_>) -> bashkit::Result<ExecResult> {
        let command = &self.0;
        let name = command.name();
        if matches!(ctx.args.first().map(String::as_str), Some("--help" | "-h")) {
            return Ok(ExecResult::ok(command.help()));
        }
        let Some(run) = current_run(&ctx) else {
            return Ok(ExecResult::err(format!("{name}: not available here\n"), 1));
        };
        let stdin = ctx.stdin.and_then(|s| s.text().ok());
        let input = match command.input(ctx.args, stdin) {
            Ok(input) => input,
            Err(message) => {
                return Ok(ExecResult::err(
                    format!("{name}: {message}\nRun `{name} --help` for its flags.\n"),
                    2,
                ));
            }
        };
        match run.call(command, input, Via::Command).await {
            Ok(json) => Ok(ExecResult::ok(json.to_string())),
            Err(error) => Ok(ExecResult::err(
                format!("{name}: {}\n", error.message),
                if error.phase == ToolPhase::Input {
                    2
                } else {
                    1
                },
            )),
        }
    }
}

/// `tools`: every command with its title.
pub(crate) struct ToolsCommand(pub Arc<Vec<Arc<Command>>>);

#[async_trait]
impl Builtin for ToolsCommand {
    async fn execute(&self, _ctx: BuiltinContext<'_>) -> bashkit::Result<ExecResult> {
        let width = self.0.iter().map(|c| c.name().len()).max().unwrap_or(0);
        let mut text = String::new();
        for command in self.0.iter() {
            let marker = if command.runs_bare() { "*" } else { " " };
            let _ = writeln!(
                text,
                "{marker} {:width$}  {}",
                command.name(),
                command.tool.meta.title
            );
        }
        text.push_str("\n* also a file: /omni/<tool>.json. Run `<tool> --help` for flags.\n");
        Ok(ExecResult::ok(text))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn kinds(schema: Value) -> Kind {
        Kind::of(&schema)
    }

    #[test]
    fn kinds_follow_the_schema_type() {
        assert_eq!(kinds(json!({"type": "integer"})), Kind::Integer);
        assert_eq!(kinds(json!({"type": ["string", "null"]})), Kind::String);
        assert_eq!(
            kinds(json!({"anyOf": [{"type": "null"}, {"type": "boolean"}]})),
            Kind::Boolean
        );
        assert_eq!(kinds(json!({"enum": ["a"]})), Kind::Any);
    }

    #[test]
    fn values_parse_by_kind() {
        let array = json!({"type": "array", "items": {"type": "integer"}});
        assert_eq!(Kind::Array.parse("3", &array), Ok(json!(3)));
        assert_eq!(Kind::Array.parse("[1,2]", &array), Ok(json!([1, 2])));
        assert_eq!(Kind::Any.parse("movie", &json!({})), Ok(json!("movie")));
        assert_eq!(Kind::Any.parse("7", &json!({})), Ok(json!(7)));
        assert!(Kind::Integer.parse("seven", &json!({})).is_err());
    }

    #[test]
    fn names_fold_across_cases() {
        assert_eq!(kebab("bodyChars"), "body-chars");
        assert_eq!(fold("body-chars"), fold("bodyChars"));
        assert_eq!(fold("body_chars"), fold("bodyChars"));
    }
}
