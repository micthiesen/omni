//! OAuth for the gateway.
//!
//! - [`pkce`]: RFC 7636 S256 verifiers and challenges.
//! - [`client`]: the upstream client that connects OAuth-protected
//!   integrations (RFC 9728 and RFC 8414 discovery, RFC 7591 registration,
//!   authorization code with PKCE, refresh).
//! - [`server`]: the downstream authorization server ChatGPT connects
//!   through (CIMD or DCR clients, owner approval on the LAN UI, rotating
//!   refresh tokens).

pub mod client;
pub mod pkce;
pub mod server;
