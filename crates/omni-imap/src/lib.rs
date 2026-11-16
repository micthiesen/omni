//! iCloud IMAP transport, exact archive actions, drafts, Sent copies and
//! the compose/archive/attachment MCP tools.
//!
//! Layout: [`protocol`] (IMAP client seam, raw imap-proto client, TLS
//! connector, Record-mode decorator), [`mime`] (mailparser-compatible
//! parsing), [`ops`] (mailbox workflows), [`transport`] (the connection
//! actor, polling, reads, caches), [`archive_store`] / [`archive_service`]
//! (durable archive receipts and workflow), [`compose`] (durable draft/send
//! receipts), [`mcp_tools`], [`task`] and [`subsystem`] (app wiring).

pub mod archive_service;
pub mod archive_store;
pub mod attachment_images;
pub mod attachments;
pub mod compose;
pub mod compose_attachments;
pub mod cursor;
#[cfg(feature = "testing")]
pub mod fake;
pub mod map_message;
pub mod mcp_tools;
pub mod mime;
pub mod ops;
pub mod protocol;
pub mod read_cache;
pub mod subsystem;
pub mod sync;
pub mod task;
pub mod transport;

pub use map_message::{BodyEnricher, LinkMetadataInput, SharedEnricher};
pub use subsystem::{ImapHandles, ImapSubsystemError, subsystem};
pub use transport::{ImapTransport, TransportOptions};
