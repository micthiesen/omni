//! The Omni Slate wire contract, shared by the device and the Omni server.
//!
//! Slate renders nothing. Omni renders every frame in the panel's native four-ink
//! format, publishes it as a numbered version, and the device fetches and streams it into
//! the panel controller row by row. This crate pins down the bytes that cross that boundary:
//! panel geometry and ink codes ([`frame`]), the row codec ([`codec`]), and the CoAP
//! resources and payloads ([`messages`]). The normative description is `docs/protocol.md`;
//! keep the two in step.
//!
//! Status: contract types only. Encoders and decoders land with the firmware bring-up.

#![no_std]

pub mod codec;
pub mod frame;
pub mod messages;
