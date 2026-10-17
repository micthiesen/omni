//! CoAP resources and payloads.
//!
//! Transport is CoAP (RFC 7252) over UDP on the Thread network, with Block2 (RFC 7959) for
//! frame transfer. Payloads are fixed little-endian binary layouts, not CBOR or JSON, so
//! the device parses them without an allocator. `docs/protocol.md` is normative.

/// UDP port of the Omni CoAP endpoint.
pub const SERVER_PORT: u16 = 5683;

/// `POST` target for the periodic check. The body is a [`CheckRequest`].
pub const CHECK_PATH: &str = "s1/check";
/// `GET` target prefix for frames: `s1/frame/<version>`, fetched with Block2.
pub const FRAME_PATH_PREFIX: &str = "s1/frame";

/// Block2 size exponent: 2^(4 + 5) = 512-byte blocks. Provisional until the transfer
/// energy measurement (`SPIKE-TRANSFER-ENERGY`) compares 256, 512 and 1024.
pub const BLOCK_SZX: u8 = 5;

/// URI query key carrying the provisioned device id: `s1/check?d=<id>`.
pub const DEVICE_QUERY_KEY: &str = "d";

/// Body of the periodic check: what the panel shows now, plus health telemetry. On the
/// wire the two booleans pack into one `flags` byte (bit 0 USB, bit 1 charging); see
/// `docs/protocol.md` for the byte layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckRequest {
    /// Version currently on the panel; 0 when nothing has been shown since provisioning.
    pub shown_version: u32,
    /// Battery voltage at the cell, in millivolts, from the ADC divider.
    pub battery_mv: u16,
    /// USB power present (the device stays awake while it is).
    pub usb_present: bool,
    /// Charger reports an active charge.
    pub charging: bool,
    /// Waveforms run since the last reset.
    pub refreshes_since_boot: u32,
    /// Why the device last reset.
    pub reset_reason: ResetReason,
    /// Firmware build number.
    pub firmware: u16,
    /// RSSI of the last frame from the Thread parent, in dBm.
    pub parent_rssi_dbm: i8,
}

/// Reset cause reported in [`CheckRequest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ResetReason {
    PowerOn = 0,
    Software = 1,
    Watchdog = 2,
    Brownout = 3,
    DeepSleepWake = 4,
    Other = 255,
}

/// The server's answer to a check. The server, not the device, decides how often the
/// device checks and when the panel refreshes; see the refresh policy in
/// `docs/protocol.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckResponse {
    /// Keep the current image. Check again after `next_check_s` seconds.
    NoChange { next_check_s: u16 },
    /// Fetch and show this frame now.
    NewFrame {
        version: u32,
        /// Encoded length in bytes, header included.
        encoded_len: u32,
        /// CRC-32/ISO-HDLC over the decoded rows in stream order.
        crc32: u32,
        waveform: crate::frame::Waveform,
        next_check_s: u16,
    },
}

/// Shortest interval the server may request; the device clamps anything below it.
pub const MIN_CHECK_S: u16 = 30;
/// Default interval when the server is unreachable or answers without one.
pub const DEFAULT_CHECK_S: u16 = 60;
