//! Executor Events adapter.
//!
//! Sits at Executor's public `/mcp` URL. Legacy MCP traffic is proxied unchanged to
//! the Executor container; MCP 2026-07-28 requests are authenticated against
//! Executor's Better Auth session endpoint, served `server/discover`, bridged to
//! Executor's legacy MCP for tools, resources and prompts (with native elicitation
//! continuations), and `events/*` methods are forwarded to Omni with the
//! authenticated owner, except subscriptions to other MCP servers' events
//! (`EVENT_UPSTREAMS`, see `upstreams`), which go to those servers.

pub mod auth;
pub mod config;
pub mod continuations;
pub mod digest;
pub mod legacy;
pub mod rpc;
pub mod server;
pub mod upstreams;

pub use config::{AdapterOptions, ConfigError, PROTOCOL_VERSION, USER_AGENT};
pub use server::{AdapterBuilder, BuildError};
