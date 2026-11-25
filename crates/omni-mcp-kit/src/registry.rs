//! The registered tool set: ordered listing, formatted call results, and an
//! rmcp [`ServerHandler`] over them.
//!
//! `omni-mcp` owns the production MCP endpoint (bearer auth, activity recording, the
//! events pre-router); it builds a [`ToolRegistry`] from every subsystem's tools in
//! its serving order and calls [`ToolRegistry::call`] from its own handler.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ListToolsResult,
    PaginatedRequestParams, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::{McpTool, ToolContext, ToolError, ToolMetaError, ToolOutput};

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum RegistryError {
    #[error("Duplicate MCP tool name: {0}")]
    Duplicate(String),
    #[error(
        "MCP tools differ from the serving order: missing {missing:?}, unexpected {unexpected:?}"
    )]
    OrderMismatch {
        missing: Vec<String>,
        unexpected: Vec<String>,
    },
    #[error("invalid MCP metadata: {0}")]
    Metadata(String),
}

impl From<ToolMetaError> for RegistryError {
    fn from(e: ToolMetaError) -> Self {
        RegistryError::Metadata(e.to_string())
    }
}

/// Every registered tool, listed in serving order.
#[derive(Clone)]
pub struct ToolRegistry {
    tools: Arc<Vec<McpTool>>,
    by_name: Arc<HashMap<String, usize>>,
    listed: Arc<Vec<Tool>>,
}

impl ToolRegistry {
    /// Rejects duplicate names and lists the tools
    /// in `order`, which must name exactly the registered tools.
    pub fn new(tools: Vec<McpTool>, order: &[&str]) -> Result<Self, RegistryError> {
        let mut seen = HashMap::new();
        for (i, tool) in tools.iter().enumerate() {
            if seen.insert(tool.meta.name.clone(), i).is_some() {
                return Err(RegistryError::Duplicate(tool.meta.name.clone()));
            }
        }
        let missing: Vec<String> = order
            .iter()
            .filter(|name| !seen.contains_key(**name))
            .map(|name| (*name).to_owned())
            .collect();
        let unexpected: Vec<String> = tools
            .iter()
            .filter(|tool| !order.contains(&tool.meta.name.as_str()))
            .map(|tool| tool.meta.name.clone())
            .collect();
        if !missing.is_empty() || !unexpected.is_empty() {
            return Err(RegistryError::OrderMismatch {
                missing,
                unexpected,
            });
        }
        let mut slots: Vec<Option<McpTool>> = tools.into_iter().map(Some).collect();
        let mut ordered = Vec::with_capacity(slots.len());
        let mut listed = Vec::with_capacity(slots.len());
        for name in order {
            if let Some(tool) = seen.get(*name).and_then(|&i| slots[i].take()) {
                listed.push(rmcp_tool(&tool.meta.listed())?);
                ordered.push(tool);
            }
        }
        let by_name = ordered
            .iter()
            .enumerate()
            .map(|(i, tool)| (tool.meta.name.clone(), i))
            .collect();
        Ok(Self {
            tools: Arc::new(ordered),
            by_name: Arc::new(by_name),
            listed: Arc::new(listed),
        })
    }

    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    pub fn get(&self, name: &str) -> Option<&McpTool> {
        self.by_name.get(name).map(|&i| &self.tools[i])
    }

    /// The `tools/list` entries.
    pub fn listed(&self) -> &[Tool] {
        &self.listed
    }

    /// Runs a tool. `None` when no such tool is registered.
    pub async fn call(
        &self,
        name: &str,
        arguments: Option<Map<String, Value>>,
        cx: ToolContext,
    ) -> Option<Result<ToolOutput, ToolError>> {
        let tool = self.get(name)?;
        let input = Value::Object(arguments.unwrap_or_default());
        Some(tool.handler.call(input, cx).await)
    }
}

fn rmcp_tool(raw: &Map<String, Value>) -> Result<Tool, RegistryError> {
    serde_json::from_value(Value::Object(raw.clone()))
        .map_err(|e| RegistryError::Metadata(format!("tool is not a valid rmcp Tool: {e}")))
}

/// The MCP result for a tool outcome.
pub fn call_tool_result(result: Result<ToolOutput, ToolError>) -> CallToolResult {
    let value = match result {
        Ok(ToolOutput::Structured(structured)) => {
            let text = omni_core::js::json_stringify(&Value::Object(structured.clone()));
            serde_json::json!({
                "content": [{"type": "text", "text": text}],
                "structuredContent": structured,
            })
        }
        Ok(ToolOutput::Custom {
            structured,
            content,
        }) => serde_json::json!({
            "content": content,
            "structuredContent": structured,
        }),
        Err(error) => failed(&error),
    };
    serde_json::from_value(value)
        .unwrap_or_else(|e| fallback_failure(&format!("invalid tool result content: {e}")))
}

fn failed(error: &crate::ToolError) -> Value {
    serde_json::json!({
        "isError": true,
        "content": [{"type": "text", "text": error.text()}],
        "_meta": {crate::ERROR_META: error.meta()},
    })
}

fn fallback_failure(message: &str) -> CallToolResult {
    let mut result = CallToolResult::error(vec![rmcp::model::ContentBlock::text(message)]);
    result.is_error = Some(true);
    result
}

/// An rmcp server over a [`ToolRegistry`] with the golden server info and instructions.
#[derive(Clone)]
pub struct RegistryServer {
    registry: ToolRegistry,
    info: ServerConfig,
}

impl RegistryServer {
    /// Uses the golden `initialize` result (server info, capabilities, instructions).
    pub fn new(registry: ToolRegistry) -> Result<Self, RegistryError> {
        let handshake = crate::golden::handshake()?;
        let initialize = handshake
            .pointer("/legacy/initialize")
            .cloned()
            .ok_or_else(|| RegistryError::Metadata("handshake has no initialize".to_owned()))?;
        let info: ServerConfig = serde_json::from_value(initialize)
            .map_err(|e| RegistryError::Metadata(format!("invalid initialize result: {e}")))?;
        Ok(Self { registry, info })
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }
}

impl ServerHandler for RegistryServer {
    fn get_info(&self) -> ServerConfig {
        self.info.clone()
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(
            self.registry.listed().to_vec(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let cx = ToolContext {
            call_id: context.id.to_string(),
            cancel: context.ct.clone(),
        };
        match self
            .registry
            .call(&request.name, request.arguments, cx)
            .await
        {
            Some(result) => Ok(call_tool_result(result).into()),
            None => Err(McpError::invalid_params(
                format!("Tool {} not found", request.name),
                None,
            )),
        }
    }
}

/// A fresh per-call context (tests and non-rmcp callers).
pub fn standalone_context(call_id: impl Into<String>) -> ToolContext {
    ToolContext {
        call_id: call_id.into(),
        cancel: CancellationToken::new(),
    }
}
