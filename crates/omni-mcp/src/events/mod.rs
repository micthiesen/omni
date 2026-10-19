//! MCP Events (`docs/mcp-events.md`): the catalog, the shared durable
//! outbox, webhook signing and delivery, the `events/*` params, the
//! [`EventPublisher`] port implementation and the Claude session and task run
//! watchers. Subscriptions are made through the gateway and owned by gateway
//! principals.
//!
//! [`EventPublisher`]: omni_runtime::ports::EventPublisher

pub mod catalog;
pub mod claude_sessions;
pub mod crypto;
pub mod dynamic;
pub mod persistence;
pub mod publisher;
pub mod rpc;
pub mod service;
pub mod task_runs;
pub mod webhook;
