//! The MCP gateway: one downstream endpoint (`/gateway/mcp`) serving Omni's
//! own tools and every configured upstream MCP integration's, with Omni's
//! events and the integrations' events (see `docs/gateway.md`).

pub mod access;
pub mod bundled;
pub mod catalog;
pub mod client;
pub mod connect;
pub mod credentials;
pub mod endpoint;
pub mod error;
pub mod events;
pub mod model;
pub mod names;
pub mod oauth;
pub mod public;
pub mod routes;
pub mod runtime;
pub mod secrets;
pub mod service;

pub use service::{Gateway, GatewayParts};
