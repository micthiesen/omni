//! A minimal Deluge 2 daemon RPC client (not the web UI).
//!
//! The daemon speaks TLS on `DELUGE_PORT` with its own self-signed (often
//! expired) certificate, so trust is a pin of that certificate's SHA-256
//! (`DELUGE_CERT_SHA256`), not a CA chain or a date. Each frame is a
//! version byte, a big-endian `u32` length and a zlib-compressed rencode
//! body; a request is a list of `[id, method, args, kwargs]` calls and a
//! reply is `[1, id, value]`, `[2, id, type, message, traceback]` or an event
//! `[3, name, args]`, which is skipped. Every operation opens its own
//! connection, logs in (`daemon.login` with `client_version`) and closes.
//!
//! The account only needs Deluge's normal level (5): add, read and remove
//! torrents and set labels.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use futures::future::BoxFuture;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, SubjectPublicKeyInfoDer, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

const LOG: &str = "Deluge";
const PROTOCOL_VERSION: u8 = 1;
/// Deluge's own version string; the daemon refuses a login without one.
const CLIENT_VERSION: &str = "2.2.0";
pub const DEFAULT_PORT: u16 = 58846;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// One whole operation (connect, login, calls).
const OPERATION_TIMEOUT: Duration = Duration::from_secs(45);
/// A status reply for a few torrents is small; a large file list stays far below.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_DEPTH: usize = 32;
/// Values one reply may decode to, so a small compressed frame cannot
/// expand into gigabytes of `Value`s.
const MAX_VALUES: usize = 1_000_000;

const RPC_RESPONSE: i64 = 1;
const RPC_ERROR: i64 = 2;
const RPC_EVENT: i64 = 3;

/// Status fields read for each torrent.
const STATUS_KEYS: [&str; 15] = [
    "name",
    "state",
    "progress",
    "total_wanted",
    "total_done",
    "eta",
    "num_seeds",
    "total_seeds",
    "download_payload_rate",
    "save_path",
    "move_completed_path",
    "is_finished",
    "message",
    "label",
    "files",
];

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DelugeError {
    #[error("Deluge is not configured: {0}")]
    NotConfigured(&'static str),
    #[error("Deluge could not be reached: {0}")]
    Connect(String),
    #[error("Deluge did not answer within {0} s")]
    Timeout(u64),
    #[error("Deluge refused the login for the configured account")]
    Login,
    /// `message` stays out of the display: libtorrent text can quote
    /// tracker announce URLs, which carry the passkey.
    #[error("Deluge {method} failed: {kind}")]
    Rpc {
        method: String,
        kind: String,
        message: String,
    },
    #[error("Deluge sent an unexpected reply: {0}")]
    Protocol(&'static str),
}

/// A rencode value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    None,
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(Vec<(Value, Value)>),
}

impl Value {
    pub fn str(s: &str) -> Self {
        Self::Bytes(s.as_bytes().to_vec())
    }

    fn as_str(&self) -> Option<String> {
        match self {
            Self::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        }
    }

    fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(n) => Some(*n),
            #[allow(clippy::cast_possible_truncation)]
            Self::Float(f) if f.is_finite() => Some(*f as i64),
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            #[allow(clippy::cast_precision_loss)]
            Self::Int(n) => Some(*n as f64),
            Self::Float(f) => Some(*f),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Self::Dict(pairs) => pairs
                .iter()
                .find(|(k, _)| matches!(k, Self::Bytes(b) if b == key.as_bytes()))
                .map(|(_, v)| v),
            _ => None,
        }
    }
}

// rencode type codes (rencode_orig.py, as bundled with Deluge).
const CHR_LIST: u8 = 59;
const CHR_DICT: u8 = 60;
const CHR_INT: u8 = 61;
const CHR_INT1: u8 = 62;
const CHR_INT2: u8 = 63;
const CHR_INT4: u8 = 64;
const CHR_INT8: u8 = 65;
const CHR_FLOAT32: u8 = 66;
const CHR_FLOAT64: u8 = 44;
const CHR_TRUE: u8 = 67;
const CHR_FALSE: u8 = 68;
const CHR_NONE: u8 = 69;
const CHR_TERM: u8 = 127;
const INT_POS_FIXED_COUNT: u8 = 44;
const INT_NEG_FIXED_START: u8 = 70;
const INT_NEG_FIXED_COUNT: u8 = 32;
const DICT_FIXED_START: u8 = 102;
const DICT_FIXED_COUNT: usize = 25;
const STR_FIXED_START: u8 = 128;
const STR_FIXED_COUNT: usize = 64;
const LIST_FIXED_START: u8 = 192;
const LIST_FIXED_COUNT: usize = 64;

pub fn encode(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Int(n) => encode_int(*n, out),
        Value::Float(f) => {
            out.push(CHR_FLOAT64);
            out.extend_from_slice(&f.to_be_bytes());
        }
        Value::Bool(true) => out.push(CHR_TRUE),
        Value::Bool(false) => out.push(CHR_FALSE),
        Value::None => out.push(CHR_NONE),
        Value::Bytes(bytes) => {
            if bytes.len() < STR_FIXED_COUNT {
                #[allow(clippy::cast_possible_truncation)]
                out.push(STR_FIXED_START + bytes.len() as u8);
            } else {
                out.extend_from_slice(bytes.len().to_string().as_bytes());
                out.push(b':');
            }
            out.extend_from_slice(bytes);
        }
        Value::List(items) => {
            let fixed = items.len() < LIST_FIXED_COUNT;
            #[allow(clippy::cast_possible_truncation)]
            out.push(if fixed {
                LIST_FIXED_START + items.len() as u8
            } else {
                CHR_LIST
            });
            for item in items {
                encode(item, out);
            }
            if !fixed {
                out.push(CHR_TERM);
            }
        }
        Value::Dict(pairs) => {
            let fixed = pairs.len() < DICT_FIXED_COUNT;
            #[allow(clippy::cast_possible_truncation)]
            out.push(if fixed {
                DICT_FIXED_START + pairs.len() as u8
            } else {
                CHR_DICT
            });
            for (k, v) in pairs {
                encode(k, out);
                encode(v, out);
            }
            if !fixed {
                out.push(CHR_TERM);
            }
        }
    }
}

fn encode_int(n: i64, out: &mut Vec<u8>) {
    if (0..i64::from(INT_POS_FIXED_COUNT)).contains(&n) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        out.push(n as u8);
    } else if (-i64::from(INT_NEG_FIXED_COUNT)..0).contains(&n) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        out.push((i64::from(INT_NEG_FIXED_START) - 1 - n) as u8);
    } else if let Ok(b) = i8::try_from(n) {
        out.push(CHR_INT1);
        out.extend_from_slice(&b.to_be_bytes());
    } else if let Ok(h) = i16::try_from(n) {
        out.push(CHR_INT2);
        out.extend_from_slice(&h.to_be_bytes());
    } else if let Ok(l) = i32::try_from(n) {
        out.push(CHR_INT4);
        out.extend_from_slice(&l.to_be_bytes());
    } else {
        out.push(CHR_INT8);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

pub fn decode(bytes: &[u8]) -> Result<Value, DelugeError> {
    let mut budget = MAX_VALUES;
    let (value, end) = decode_at(bytes, 0, 0, &mut budget)?;
    if end != bytes.len() {
        return Err(DelugeError::Protocol("trailing bytes"));
    }
    Ok(value)
}

fn take(bytes: &[u8], at: usize, n: usize) -> Result<&[u8], DelugeError> {
    bytes
        .get(at..at.checked_add(n).ok_or(DelugeError::Protocol("length"))?)
        .ok_or(DelugeError::Protocol("truncated"))
}

fn decode_at(
    bytes: &[u8],
    at: usize,
    depth: usize,
    budget: &mut usize,
) -> Result<(Value, usize), DelugeError> {
    if depth > MAX_DEPTH {
        return Err(DelugeError::Protocol("nested too deeply"));
    }
    *budget = budget
        .checked_sub(1)
        .ok_or(DelugeError::Protocol("reply has too many values"))?;
    let code = *bytes.get(at).ok_or(DelugeError::Protocol("truncated"))?;
    let fixed = |n: usize| -> Result<[u8; 8], DelugeError> {
        let mut buf = [0u8; 8];
        buf[8 - n..].copy_from_slice(take(bytes, at + 1, n)?);
        Ok(buf)
    };
    let signed = |n: usize| -> Result<i64, DelugeError> {
        let raw = i64::from_be_bytes(fixed(n)?);
        let shift = 64 - 8 * u32::try_from(n).map_err(|_| DelugeError::Protocol("int"))?;
        Ok((raw << shift) >> shift)
    };
    Ok(match code {
        c if c < INT_POS_FIXED_COUNT => (Value::Int(i64::from(c)), at + 1),
        c if (INT_NEG_FIXED_START..INT_NEG_FIXED_START + INT_NEG_FIXED_COUNT).contains(&c) => (
            Value::Int(i64::from(INT_NEG_FIXED_START) - 1 - i64::from(c)),
            at + 1,
        ),
        CHR_INT1 => (Value::Int(signed(1)?), at + 2),
        CHR_INT2 => (Value::Int(signed(2)?), at + 3),
        CHR_INT4 => (Value::Int(signed(4)?), at + 5),
        CHR_INT8 => (Value::Int(signed(8)?), at + 9),
        CHR_INT => {
            let end = bytes[at + 1..]
                .iter()
                .position(|b| *b == CHR_TERM)
                .ok_or(DelugeError::Protocol("unterminated int"))?;
            let text = std::str::from_utf8(&bytes[at + 1..at + 1 + end])
                .map_err(|_| DelugeError::Protocol("int"))?;
            // Larger than i64: only ever a size, so saturate.
            let n = text.parse::<i64>().unwrap_or(if text.starts_with('-') {
                i64::MIN
            } else {
                i64::MAX
            });
            (Value::Int(n), at + end + 2)
        }
        CHR_FLOAT32 => {
            let raw = take(bytes, at + 1, 4)?;
            let f = f32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
            (Value::Float(f64::from(f)), at + 5)
        }
        CHR_FLOAT64 => (Value::Float(f64::from_be_bytes(fixed(8)?)), at + 9),
        CHR_TRUE => (Value::Bool(true), at + 1),
        CHR_FALSE => (Value::Bool(false), at + 1),
        CHR_NONE => (Value::None, at + 1),
        b'0'..=b'9' => {
            let colon = bytes[at..]
                .iter()
                .position(|b| *b == b':')
                .ok_or(DelugeError::Protocol("string length"))?;
            let len: usize = std::str::from_utf8(&bytes[at..at + colon])
                .ok()
                .and_then(|t| t.parse().ok())
                .ok_or(DelugeError::Protocol("string length"))?;
            let start = at + colon + 1;
            (Value::Bytes(take(bytes, start, len)?.to_vec()), start + len)
        }
        c if c >= STR_FIXED_START && usize::from(c - STR_FIXED_START) < STR_FIXED_COUNT => {
            let len = usize::from(c - STR_FIXED_START);
            (
                Value::Bytes(take(bytes, at + 1, len)?.to_vec()),
                at + 1 + len,
            )
        }
        c if c >= LIST_FIXED_START => {
            let mut items = Vec::new();
            let mut pos = at + 1;
            for _ in 0..usize::from(c - LIST_FIXED_START) {
                let (item, next) = decode_at(bytes, pos, depth + 1, budget)?;
                items.push(item);
                pos = next;
            }
            (Value::List(items), pos)
        }
        c if c >= DICT_FIXED_START && usize::from(c - DICT_FIXED_START) < DICT_FIXED_COUNT => {
            let mut pairs = Vec::new();
            let mut pos = at + 1;
            for _ in 0..usize::from(c - DICT_FIXED_START) {
                let (k, next) = decode_at(bytes, pos, depth + 1, budget)?;
                let (v, next) = decode_at(bytes, next, depth + 1, budget)?;
                pairs.push((k, v));
                pos = next;
            }
            (Value::Dict(pairs), pos)
        }
        CHR_LIST => {
            let mut items = Vec::new();
            let mut pos = at + 1;
            while *bytes.get(pos).ok_or(DelugeError::Protocol("truncated"))? != CHR_TERM {
                let (item, next) = decode_at(bytes, pos, depth + 1, budget)?;
                items.push(item);
                pos = next;
            }
            (Value::List(items), pos + 1)
        }
        CHR_DICT => {
            let mut pairs = Vec::new();
            let mut pos = at + 1;
            while *bytes.get(pos).ok_or(DelugeError::Protocol("truncated"))? != CHR_TERM {
                let (k, next) = decode_at(bytes, pos, depth + 1, budget)?;
                let (v, next) = decode_at(bytes, next, depth + 1, budget)?;
                pairs.push((k, v));
                pos = next;
            }
            (Value::Dict(pairs), pos + 1)
        }
        _ => return Err(DelugeError::Protocol("unknown type code")),
    })
}

/// One frame: version, length, zlib(rencode(value)).
pub fn frame(value: &Value) -> Result<Vec<u8>, DelugeError> {
    let mut body = Vec::new();
    encode(value, &mut body);
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(&body)
        .map_err(|_| DelugeError::Protocol("compress"))?;
    let compressed = z.finish().map_err(|_| DelugeError::Protocol("compress"))?;
    let len = u32::try_from(compressed.len()).map_err(|_| DelugeError::Protocol("too large"))?;
    let mut out = Vec::with_capacity(compressed.len() + 5);
    out.push(PROTOCOL_VERSION);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&compressed);
    Ok(out)
}

pub fn unframe_body(compressed: &[u8]) -> Result<Value, DelugeError> {
    let mut body = Vec::new();
    flate2::read::ZlibDecoder::new(compressed)
        .take(u64::try_from(MAX_FRAME_BYTES * 4).unwrap_or(u64::MAX))
        .read_to_end(&mut body)
        .map_err(|_| DelugeError::Protocol("decompress"))?;
    decode(&body)
}

/// One DER element at `at`: (tag, content start, end).
fn der_element(der: &[u8], at: usize) -> Option<(u8, usize, usize)> {
    let tag = *der.get(at)?;
    let first = *der.get(at + 1)?;
    let (len, header) = if first < 0x80 {
        (usize::from(first), 2)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | usize::from(*der.get(at + 2 + i)?);
        }
        (len, 2 + n)
    };
    let start = at + header;
    let end = start.checked_add(len)?;
    (end <= der.len()).then_some((tag, start, end))
}

/// The DER `SubjectPublicKeyInfo` of an X.509 certificate (v1 or v3).
pub fn subject_public_key_info(cert: &[u8]) -> Option<&[u8]> {
    const SEQUENCE: u8 = 0x30;
    const VERSION: u8 = 0xa0;
    let (tag, cert_start, _) = der_element(cert, 0)?;
    if tag != SEQUENCE {
        return None;
    }
    let (tag, tbs_start, tbs_end) = der_element(cert, cert_start)?;
    if tag != SEQUENCE {
        return None;
    }
    let mut at = tbs_start;
    let (tag, _, end) = der_element(cert, at)?;
    if tag == VERSION {
        at = end;
    }
    // serialNumber, signature, issuer, validity, subject
    for _ in 0..5 {
        at = der_element(cert, at)?.2;
    }
    let (tag, _, end) = der_element(cert, at)?;
    (tag == SEQUENCE && end <= tbs_end).then(|| &cert[at..end])
}

/// Trusts exactly one certificate, by SHA-256, whatever its dates or name.
#[derive(Debug)]
struct PinnedCertificate {
    sha256: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedCertificate {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if omni_core::digest::sha256_hex(end_entity.as_ref()) == self.sha256 {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "Deluge certificate does not match DELUGE_CERT_SHA256".to_owned(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General(
            "Deluge connections use TLS 1.3 only".to_owned(),
        ))
    }

    /// The handshake is checked against the pinned certificate's own key.
    /// Deluge generates X.509 v1 certificates, which webpki will not parse as
    /// certificates, so the key is taken out of the DER directly.
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        if omni_core::digest::sha256_hex(cert.as_ref()) != self.sha256 {
            return Err(rustls::Error::General(
                "certificate changed mid-handshake".to_owned(),
            ));
        }
        let spki = subject_public_key_info(cert.as_ref()).ok_or_else(|| {
            rustls::Error::General("Deluge certificate has no readable public key".to_owned())
        })?;
        rustls::crypto::verify_tls13_signature_with_raw_key(
            message,
            &SubjectPublicKeyInfoDer::from(spki),
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Clone)]
pub struct DelugeConfig {
    pub host: Option<String>,
    pub port: Option<u64>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub cert_sha256: Option<String>,
}

#[derive(Clone)]
struct Settings {
    host: String,
    port: u16,
    username: String,
    password: String,
    tls: TlsConnector,
}

/// A torrent's state as Deluge reports it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TorrentStatus {
    pub name: String,
    /// Downloading, Seeding, Paused, Checking, Queued, Error, Moving, ...
    pub state: String,
    pub progress_percent: f64,
    pub total_wanted: i64,
    pub total_done: i64,
    pub eta_secs: Option<i64>,
    pub seeds: i64,
    pub swarm_seeds: i64,
    pub download_rate: i64,
    pub save_path: String,
    pub move_completed_path: String,
    pub is_finished: bool,
    pub message: String,
    pub label: String,
    /// Paths relative to `save_path`.
    pub files: Vec<String>,
}

impl TorrentStatus {
    fn of(value: &Value) -> Self {
        let text = |k: &str| value.get(k).and_then(Value::as_str).unwrap_or_default();
        let int = |k: &str| value.get(k).and_then(Value::as_i64).unwrap_or_default();
        Self {
            name: text("name"),
            state: text("state"),
            progress_percent: value
                .get("progress")
                .and_then(Value::as_f64)
                .unwrap_or_default(),
            total_wanted: int("total_wanted"),
            total_done: int("total_done"),
            eta_secs: value
                .get("eta")
                .and_then(Value::as_i64)
                .filter(|eta| *eta > 0),
            seeds: int("num_seeds"),
            swarm_seeds: int("total_seeds"),
            download_rate: int("download_payload_rate"),
            save_path: text("save_path"),
            move_completed_path: text("move_completed_path"),
            is_finished: value
                .get("is_finished")
                .and_then(Value::as_bool)
                .unwrap_or_default(),
            message: text("message"),
            label: text("label"),
            files: match value.get("files") {
                Some(Value::List(files)) => files
                    .iter()
                    .filter_map(|f| f.get("path").and_then(Value::as_str))
                    .collect(),
                _ => Vec::new(),
            },
        }
    }

    /// Where a file lives once the torrent is at `save_path`.
    pub fn file_paths(&self) -> Vec<String> {
        let base = self.save_path.trim_end_matches('/');
        self.files.iter().map(|f| format!("{base}/{f}")).collect()
    }
}

/// Why an add did not succeed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AddError {
    /// Failed before the torrent was sent (connection, login).
    #[error("{0}")]
    NotSent(DelugeError),
    /// Deluge answered with an error.
    #[error("{0}")]
    Refused(DelugeError),
    /// The torrent was sent but no answer arrived.
    #[error("{0}")]
    Unknown(DelugeError),
}

/// What adding a torrent did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Added {
    Added,
    /// Deluge already had the torrent.
    AlreadyPresent,
}

/// The torrent operations the audiobook flow needs (tests substitute a fake).
pub trait TorrentClient: Send + Sync {
    fn is_configured(&self) -> bool;
    /// The folder `label` moves finished torrents to, if it moves them.
    fn label_completed_path<'a>(
        &'a self,
        label: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, DelugeError>>;
    fn add<'a>(
        &'a self,
        file_name: &'a str,
        torrent: &'a [u8],
        hash: &'a str,
        label: &'a str,
        completed_path: &'a str,
    ) -> BoxFuture<'a, Result<Added, AddError>>;
    fn statuses<'a>(
        &'a self,
        hashes: &'a [String],
    ) -> BoxFuture<'a, Result<BTreeMap<String, TorrentStatus>, DelugeError>>;
    /// Removes a torrent and its downloaded data.
    fn remove<'a>(&'a self, hash: &'a str) -> BoxFuture<'a, Result<(), DelugeError>>;
}

impl TorrentClient for Deluge {
    fn is_configured(&self) -> bool {
        self.settings.is_ok()
    }

    fn label_completed_path<'a>(
        &'a self,
        label: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, DelugeError>> {
        Box::pin(self.label_completed_path_inner(label))
    }

    fn add<'a>(
        &'a self,
        file_name: &'a str,
        torrent: &'a [u8],
        hash: &'a str,
        label: &'a str,
        completed_path: &'a str,
    ) -> BoxFuture<'a, Result<Added, AddError>> {
        Box::pin(self.add_inner(file_name, torrent, hash, label, completed_path))
    }

    fn statuses<'a>(
        &'a self,
        hashes: &'a [String],
    ) -> BoxFuture<'a, Result<BTreeMap<String, TorrentStatus>, DelugeError>> {
        Box::pin(self.statuses_inner(hashes))
    }

    fn remove<'a>(&'a self, hash: &'a str) -> BoxFuture<'a, Result<(), DelugeError>> {
        Box::pin(self.remove_inner(hash))
    }
}

/// The configured Deluge daemon, or why there is none.
#[derive(Clone)]
pub struct Deluge {
    settings: Result<Settings, &'static str>,
}

impl Deluge {
    pub fn new(config: DelugeConfig) -> Self {
        Self {
            settings: settings(config),
        }
    }

    /// The completed-download folder the label moves finished torrents to,
    /// or `None` when the label is missing or does not move them.
    async fn label_completed_path_inner(&self, label: &str) -> Result<Option<String>, DelugeError> {
        let label = label.to_owned();
        self.with_session(move |mut s| async move {
            let labels = s.call("label.get_labels", vec![], vec![]).await?;
            let known = matches!(&labels, Value::List(items)
                if items.iter().any(|l| l.as_str().as_deref() == Some(label.as_str())));
            if !known {
                return Ok(None);
            }
            let options = s
                .call("label.get_options", vec![Value::str(&label)], vec![])
                .await?;
            let moves = options
                .get("move_completed")
                .and_then(Value::as_bool)
                .unwrap_or_default();
            Ok(options
                .get("move_completed_path")
                .and_then(Value::as_str)
                .filter(|p| moves && !p.is_empty()))
        })
        .await
    }

    /// Adds a `.torrent` that moves to `completed_path` when finished and
    /// carries `label`.
    async fn add_inner(
        &self,
        file_name: &str,
        torrent: &[u8],
        hash: &str,
        label: &str,
        completed_path: &str,
    ) -> Result<Added, AddError> {
        let settings = self
            .settings
            .as_ref()
            .map_err(|r| AddError::NotSent(DelugeError::NotConfigured(r)))?;
        let dump = base64::engine::general_purpose::STANDARD.encode(torrent);
        tokio::time::timeout(OPERATION_TIMEOUT, async {
            let mut s = Session::open(settings).await.map_err(AddError::NotSent)?;
            let options = Value::Dict(vec![
                (Value::str("move_completed"), Value::Bool(true)),
                (
                    Value::str("move_completed_path"),
                    Value::str(completed_path),
                ),
            ]);
            let added = match s
                .call(
                    "core.add_torrent_file",
                    vec![Value::str(file_name), Value::str(&dump), options],
                    vec![],
                )
                .await
            {
                Ok(_) => Added::Added,
                Err(DelugeError::Rpc { kind, message, .. })
                    if kind.contains("AddTorrentError") && message.contains("already") =>
                {
                    Added::AlreadyPresent
                }
                Err(error @ DelugeError::Rpc { .. }) => return Err(AddError::Refused(error)),
                Err(error) => return Err(AddError::Unknown(error)),
            };
            // The add's own move options already put it in the right folder;
            // the label keeps it grouped with the other books in Deluge.
            if let Err(error) = s
                .call(
                    "label.set_torrent",
                    vec![Value::str(hash), Value::str(label)],
                    vec![],
                )
                .await
            {
                tracing::warn!(target: LOG, "Deluge label {label} not set: {error}");
            }
            Ok(added)
        })
        .await
        .unwrap_or(Err(AddError::Unknown(DelugeError::Timeout(
            OPERATION_TIMEOUT.as_secs(),
        ))))
    }

    /// The status of each of `hashes` Deluge has; absent hashes are missing.
    async fn statuses_inner(
        &self,
        hashes: &[String],
    ) -> Result<BTreeMap<String, TorrentStatus>, DelugeError> {
        if hashes.is_empty() {
            return Ok(BTreeMap::new());
        }
        let ids = Value::List(hashes.iter().map(|h| Value::str(h)).collect());
        self.with_session(move |mut s| async move {
            let filter = Value::Dict(vec![(Value::str("id"), ids)]);
            let keys = Value::List(STATUS_KEYS.iter().map(|k| Value::str(k)).collect());
            let reply = s
                .call("core.get_torrents_status", vec![filter, keys], vec![])
                .await?;
            let Value::Dict(pairs) = reply else {
                return Err(DelugeError::Protocol("status is not a dict"));
            };
            Ok(pairs
                .iter()
                .filter_map(|(id, status)| Some((id.as_str()?, TorrentStatus::of(status))))
                .collect())
        })
        .await
    }

    /// Removes a torrent and its downloaded data.
    async fn remove_inner(&self, hash: &str) -> Result<(), DelugeError> {
        let hash = hash.to_owned();
        self.with_session(move |mut s| async move {
            s.call(
                "core.remove_torrent",
                vec![Value::str(&hash), Value::Bool(true)],
                vec![],
            )
            .await
            .map(|_| ())
        })
        .await
    }

    async fn with_session<T, F, Fut>(&self, f: F) -> Result<T, DelugeError>
    where
        F: FnOnce(Session) -> Fut,
        Fut: std::future::Future<Output = Result<T, DelugeError>>,
    {
        let settings = self
            .settings
            .as_ref()
            .map_err(|r| DelugeError::NotConfigured(r))?;
        tokio::time::timeout(OPERATION_TIMEOUT, async {
            let session = Session::open(settings).await?;
            f(session).await
        })
        .await
        .unwrap_or(Err(DelugeError::Timeout(OPERATION_TIMEOUT.as_secs())))
    }
}

fn settings(config: DelugeConfig) -> Result<Settings, &'static str> {
    let present = |v: Option<String>| v.filter(|v| !v.is_empty());
    let host = present(config.host).ok_or("DELUGE_HOST is not set")?;
    let username = present(config.username).ok_or("DELUGE_USERNAME is not set")?;
    let password = present(config.password).ok_or("DELUGE_PASSWORD is not set")?;
    let pin = present(config.cert_sha256)
        .map(|p| p.replace(':', "").to_ascii_lowercase())
        .filter(|p| p.len() == 64 && p.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("DELUGE_CERT_SHA256 is not a SHA-256 hex digest")?;
    let port = match config.port {
        None => DEFAULT_PORT,
        Some(port) => u16::try_from(port).map_err(|_| "DELUGE_PORT is out of range")?,
    };
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let tls = ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| "TLS setup failed")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertificate {
            sha256: pin,
            provider,
        }))
        .with_no_client_auth();
    Ok(Settings {
        host,
        port,
        username,
        password,
        tls: TlsConnector::from(Arc::new(tls)),
    })
}

struct Session {
    stream: TlsStream<TcpStream>,
    next_id: i64,
}

impl Session {
    async fn open(settings: &Settings) -> Result<Self, DelugeError> {
        let tcp = tokio::time::timeout(
            CONNECT_TIMEOUT,
            TcpStream::connect((settings.host.as_str(), settings.port)),
        )
        .await
        .map_err(|_| DelugeError::Timeout(CONNECT_TIMEOUT.as_secs()))?
        .map_err(|e| DelugeError::Connect(e.kind().to_string()))?;
        // The verifier ignores the name; rustls still needs a syntactically valid one.
        let name = ServerName::try_from("deluge").map_err(|_| DelugeError::Protocol("name"))?;
        let stream = settings.tls.connect(name, tcp).await.map_err(|e| {
            tracing::debug!(target: LOG, "Deluge TLS failed: {e}");
            DelugeError::Connect(format!("TLS: {e}"))
        })?;
        let mut session = Self { stream, next_id: 0 };
        let ok = session
            .call(
                "daemon.login",
                vec![
                    Value::str(&settings.username),
                    Value::str(&settings.password),
                ],
                vec![("client_version", Value::str(CLIENT_VERSION))],
            )
            .await;
        match ok {
            Ok(Value::Int(level)) if level > 0 => Ok(session),
            Ok(_) => Err(DelugeError::Login),
            Err(DelugeError::Rpc { kind, .. }) if kind.contains("BadLogin") => {
                Err(DelugeError::Login)
            }
            Err(error) => Err(error),
        }
    }

    async fn call(
        &mut self,
        method: &str,
        args: Vec<Value>,
        kwargs: Vec<(&str, Value)>,
    ) -> Result<Value, DelugeError> {
        self.next_id += 1;
        let id = self.next_id;
        let request = Value::List(vec![Value::List(vec![
            Value::Int(id),
            Value::str(method),
            Value::List(args),
            Value::Dict(
                kwargs
                    .into_iter()
                    .map(|(k, v)| (Value::str(k), v))
                    .collect(),
            ),
        ])]);
        let bytes = frame(&request)?;
        self.stream
            .write_all(&bytes)
            .await
            .map_err(|e| DelugeError::Connect(e.kind().to_string()))?;
        loop {
            let reply = self.read().await?;
            let Value::List(items) = reply else {
                return Err(DelugeError::Protocol("reply is not a list"));
            };
            match items.first().and_then(Value::as_i64) {
                Some(RPC_EVENT) => continue,
                Some(RPC_RESPONSE) if items.get(1).and_then(Value::as_i64) == Some(id) => {
                    return Ok(items.get(2).cloned().unwrap_or(Value::None));
                }
                Some(RPC_ERROR) if items.get(1).and_then(Value::as_i64) == Some(id) => {
                    // [2, id, exception name, exception args, kwargs, traceback]
                    let message = match items.get(3) {
                        Some(Value::List(args)) => args
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" "),
                        Some(other) => other.as_str().unwrap_or_default(),
                        None => String::new(),
                    };
                    return Err(DelugeError::Rpc {
                        method: method.to_owned(),
                        kind: items.get(2).and_then(Value::as_str).unwrap_or_default(),
                        message,
                    });
                }
                Some(RPC_RESPONSE | RPC_ERROR) => continue,
                _ => return Err(DelugeError::Protocol("unknown reply type")),
            }
        }
    }

    async fn read(&mut self) -> Result<Value, DelugeError> {
        let mut header = [0u8; 5];
        self.stream
            .read_exact(&mut header)
            .await
            .map_err(|e| DelugeError::Connect(e.kind().to_string()))?;
        if header[0] != PROTOCOL_VERSION {
            return Err(DelugeError::Protocol("protocol version"));
        }
        let len = usize::try_from(u32::from_be_bytes([
            header[1], header[2], header[3], header[4],
        ]))
        .map_err(|_| DelugeError::Protocol("length"))?;
        if len > MAX_FRAME_BYTES {
            return Err(DelugeError::Protocol("reply too large"));
        }
        let mut body = vec![0u8; len];
        self.stream
            .read_exact(&mut body)
            .await
            .map_err(|e| DelugeError::Connect(e.kind().to_string()))?;
        unframe_body(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::{Value, decode, encode};

    /// `rencode.dumps((1, ["a", {"k": True}], None, -5, 300, 1.5, "x" * 70))`
    /// from Deluge 2.2's bundled rencode (its floats are 32-bit).
    #[test]
    fn decodes_what_deluge_encodes() {
        let hex = format!(
            "c701c2816167816b43454a3f012c423fc0000037303a{}",
            "78".repeat(70)
        );
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
            .collect();
        let value = decode(&bytes).expect("decode");
        assert_eq!(
            value,
            Value::List(vec![
                Value::Int(1),
                Value::List(vec![
                    Value::str("a"),
                    Value::Dict(vec![(Value::str("k"), Value::Bool(true))]),
                ]),
                Value::None,
                Value::Int(-5),
                Value::Int(300),
                Value::Float(1.5),
                Value::Bytes(vec![b'x'; 70]),
            ])
        );
    }

    /// DER `SEQUENCE` (tag 0x30) or any `tag` around `content`.
    fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        if content.len() < 0x80 {
            out.push(u8::try_from(content.len()).expect("len"));
        } else {
            out.push(0x82);
            out.extend_from_slice(&u16::try_from(content.len()).expect("len").to_be_bytes());
        }
        out.extend_from_slice(content);
        out
    }

    #[test]
    fn finds_the_public_key_in_v1_and_v3_certificates() {
        let spki = tlv(0x30, &[0x05; 200]);
        let fields = |version: bool| {
            let mut tbs = Vec::new();
            if version {
                tbs.extend(tlv(0xa0, &tlv(0x02, &[2])));
            }
            tbs.extend(tlv(0x02, &[0]));
            for _ in 0..4 {
                tbs.extend(tlv(0x30, b"x"));
            }
            tbs.extend(&spki);
            tbs.extend(tlv(0xa3, b"ext"));
            let mut cert = tlv(0x30, &tbs);
            cert.extend(tlv(0x30, b"alg"));
            cert.extend(tlv(0x03, b"sig"));
            tlv(0x30, &cert)
        };
        for version in [false, true] {
            let cert = fields(version);
            assert_eq!(super::subject_public_key_info(&cert), Some(spki.as_slice()));
        }
        assert_eq!(super::subject_public_key_info(b"\x30\x05ab"), None);
    }

    #[test]
    fn round_trips_large_and_negative_values() {
        let value = Value::List(vec![
            Value::Int(-40),
            Value::Int(-200),
            Value::Int(70_000),
            Value::Int(9_000_000_000),
            Value::List((0..70).map(Value::Int).collect()),
            Value::Dict(
                (0..30)
                    .map(|i| (Value::Int(i), Value::Bool(false)))
                    .collect(),
            ),
        ]);
        let mut bytes = Vec::new();
        encode(&value, &mut bytes);
        assert_eq!(decode(&bytes).expect("decode"), value);
    }
}
