//! MCP Events (`docs/mcp-events.md`): the catalog, the
//! shared durable outbox, webhook signing and delivery, Executor delegated
//! authorization, the `events/*` methods, the [`EventPublisher`] port
//! implementation and the Claude session and task run watchers.
//!
//! [`EventPublisher`]: omni_runtime::ports::EventPublisher

pub mod catalog;
pub mod claude_sessions;
pub mod crypto;
pub mod dynamic;
pub mod executor_auth;
pub mod persistence;
pub mod publisher;
pub mod rpc;
pub mod service;
pub mod task_runs;
pub mod webhook;
