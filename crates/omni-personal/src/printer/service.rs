//! The monochrome LAN printer: status, and PDF
//! printing through `pdfinfo` → `cupsfilter` → `rastertobrlaser` → IPP.
//!
//! The PDF comes from exactly one source: a public HTTPS URL that Omni
//! downloads, or bounded inline base64 bytes for a private document that has
//! no public URL. Both sources pass the same PDF validation and share one
//! duplicate fingerprint, which covers only the bytes and print settings.
//!
//! Duplicate suppression: an exact document + configuration accepted within
//! five minutes is refused unless `allowDuplicate`, using an in-memory map,
//! an in-flight set and the durable `printer-accepted-job` row written right
//! after IPP acceptance (before polling). When that durable write fails, the
//! result says not to retry. IPP submission is never retried.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use futures::future::BoxFuture;
use omni_core::clock::SharedClock;
use omni_core::js::{json_stringify, utf16_len};
use omni_http::Url;
use omni_store::cbor::Extra;
use omni_store::entity::Entity;
use omni_store::{EntityOps, EntityWrite, Store, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::ipp::{IppPrinterStatus, JobStatus, PrintJob, PrintOptions, PrinterClient};
use super::pipeline::{DownloadedFile, PdfDownloader, PrintProcesses};

pub const PRINTER_DOWNLOAD_USER_AGENT: &str = "OpenAI File Downloader, XaiImageApiFetch/1.0";
pub const MAX_PDF_BYTES: usize = 20 * 1024 * 1024;
/// Decoded inline PDF limit; its base64 form must fit the 4 MiB MCP request.
pub const MAX_INLINE_PDF_BYTES: usize = 3_000_000;
/// Base64 length of `MAX_INLINE_PDF_BYTES`.
pub const MAX_INLINE_PDF_BASE64_CHARS: usize = 4_000_000;
pub const MAX_PRINT_DATA_BYTES: usize = 256 * 1024 * 1024;
const MAX_PAGES: u32 = 25;
const DUPLICATE_WINDOW_MS: i64 = 5 * 60 * 1000;
const JOB_STATUS_POLL: Duration = Duration::from_millis(500);
const JOB_STATUS_MAX_POLLS: u32 = 120;
pub const CUPS_FILTER: &str = "/usr/sbin/cupsfilter";
pub const BRLASER_FILTER: &str = "/usr/lib/cups/filter/rastertobrlaser";
pub const BRLASER_PPD: &str = "/usr/share/omni-printing/brother-hll2370dw.ppd";
const LOG: &str = "Printer";

/// A printer failure; `message` is what the caller sees.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct PrinterError {
    pub operation: String,
    pub message: String,
}

fn failure(operation: &str, message: impl Into<String>) -> PrinterError {
    PrinterError {
        operation: operation.to_owned(),
        message: message.into(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrintPaper {
    #[serde(rename = "letter")]
    Letter,
    #[serde(rename = "a4")]
    A4,
    #[serde(rename = "legal")]
    Legal,
}

impl PrintPaper {
    fn media(self) -> &'static str {
        match self {
            PrintPaper::Letter => "na_letter_8.5x11in",
            PrintPaper::A4 => "iso_a4_210x297mm",
            PrintPaper::Legal => "na_legal_8.5x14in",
        }
    }

    fn cups(self) -> &'static str {
        match self {
            PrintPaper::Letter => "Letter",
            PrintPaper::A4 => "A4",
            PrintPaper::Legal => "Legal",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            PrintPaper::Letter => "letter",
            PrintPaper::A4 => "a4",
            PrintPaper::Legal => "legal",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrintSides {
    #[serde(rename = "one-sided")]
    OneSided,
    #[serde(rename = "two-sided-long-edge")]
    TwoSidedLongEdge,
    #[serde(rename = "two-sided-short-edge")]
    TwoSidedShortEdge,
}

impl PrintSides {
    fn cups(self) -> &'static str {
        match self {
            PrintSides::OneSided => "None",
            PrintSides::TwoSidedLongEdge => "DuplexNoTumble",
            PrintSides::TwoSidedShortEdge => "DuplexTumble",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            PrintSides::OneSided => "one-sided",
            PrintSides::TwoSidedLongEdge => "two-sided-long-edge",
            PrintSides::TwoSidedShortEdge => "two-sided-short-edge",
        }
    }
}

/// A print request (the MCP tool's input after defaults). Exactly one of
/// `url` and `pdf_base64` names the document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrintPdfInput {
    pub url: Option<String>,
    pub pdf_base64: Option<String>,
    pub paper: Option<PrintPaper>,
    pub sides: Option<PrintSides>,
    pub copies: Option<f64>,
    pub job_name: Option<String>,
    pub allow_duplicate: bool,
}

/// `get_printer_status` output.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrinterStatus {
    pub configured: bool,
    pub name: Option<String>,
    pub uri: String,
    pub state: String,
    pub state_reasons: Vec<String>,
    pub ready: bool,
    pub accepting_jobs: Option<bool>,
    pub queued_job_count: Option<f64>,
    pub toner_percent: Option<f64>,
    pub monochrome_only: bool,
    pub default_sides: &'static str,
    pub supported_formats: Vec<String>,
    pub supported_media: Vec<String>,
}

/// `print_document` output.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptedPrintJob {
    pub accepted: bool,
    pub completed: bool,
    pub job_id: Option<f64>,
    pub job_uri: String,
    pub job_state: String,
    pub job_name: String,
    pub pages: u32,
    pub copies: u32,
    pub paper: PrintPaper,
    pub sides: PrintSides,
    pub impressions_completed: Option<f64>,
    pub message: String,
}

/// `printer-accepted-job`: the durable duplicate-suppression record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptedPrintRecord {
    pub fingerprint: String,
    pub accepted_at: f64,
    pub job_id: Option<f64>,
    pub job_uri: String,
    pub job_state: String,
    pub job_name: String,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for AcceptedPrintRecord {
    const NAME: &'static str = "printer-accepted-job";
    type Key = String;
    fn key(&self) -> String {
        self.fingerprint.clone()
    }
}

/// Where accepted fingerprints persist (a seam for tests).
pub trait AcceptedPrintStore: Send + Sync {
    fn get<'a>(
        &'a self,
        fingerprint: &'a str,
    ) -> BoxFuture<'a, Result<Option<AcceptedPrintRecord>, String>>;
    fn upsert(&self, record: AcceptedPrintRecord) -> BoxFuture<'_, Result<(), String>>;
    fn delete_older_than(&self, cutoff: i64) -> BoxFuture<'_, Result<(), String>>;
}

/// The docstore-backed store.
pub struct DurableAcceptedPrintStore {
    store: Store,
}

impl DurableAcceptedPrintStore {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

fn store_message(error: StoreError) -> String {
    error.to_string()
}

impl AcceptedPrintStore for DurableAcceptedPrintStore {
    fn get<'a>(
        &'a self,
        fingerprint: &'a str,
    ) -> BoxFuture<'a, Result<Option<AcceptedPrintRecord>, String>> {
        let key = fingerprint.to_owned();
        Box::pin(async move {
            self.store
                .read(move |docs| docs.get::<AcceptedPrintRecord>(&key))
                .await
                .map_err(store_message)
        })
    }

    fn upsert(&self, record: AcceptedPrintRecord) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.store
                .write(move |tx| tx.upsert(&record, omni_store::entity::UpsertOpts::default()))
                .await
                .map_err(store_message)
        })
    }

    fn delete_older_than(&self, cutoff: i64) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            #[allow(clippy::cast_precision_loss)]
            let cutoff = cutoff as f64;
            self.store
                .write(move |tx| {
                    for record in tx.get_all::<AcceptedPrintRecord>()? {
                        if record.accepted_at <= cutoff {
                            tx.delete::<AcceptedPrintRecord>(&record.fingerprint)?;
                        }
                    }
                    Ok::<_, StoreError>(())
                })
                .await
                .map_err(store_message)
        })
    }
}

/// Injectable parts of the service.
pub struct PrinterDependencies {
    pub printer: Option<Arc<dyn PrinterClient>>,
    pub download: Arc<dyn PdfDownloader>,
    pub processes: Arc<dyn PrintProcesses>,
    pub accepted: Arc<dyn AcceptedPrintStore>,
    pub clock: SharedClock,
    pub temp_root: PathBuf,
    pub max_print_data_bytes: usize,
    pub poll_interval: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NormalizedInput {
    paper: PrintPaper,
    sides: PrintSides,
    copies: u32,
    job_name: String,
}

pub fn assert_https_document_url(value: &str, subject: &str) -> Result<Url, String> {
    if value.is_empty() {
        return Err(format!("{subject} must be a public HTTPS URL"));
    }
    let url = Url::parse(value).map_err(|_| "Invalid URL".to_owned())?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return Err(format!(
            "{subject} must be a public HTTPS URL without credentials"
        ));
    }
    Ok(url)
}

fn normalize_input(input: &PrintPdfInput) -> Result<NormalizedInput, PrinterError> {
    let copies = input.copies.unwrap_or(1.0);
    if copies.fract() != 0.0 || !(1.0..=3.0).contains(&copies) {
        return Err(failure(
            "validate print request",
            "Copies must be an integer from 1 to 3",
        ));
    }
    let trimmed = input
        .job_name
        .as_deref()
        .map(omni_core::js::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("MCP print job");
    if utf16_len(trimmed) > 80 || trimmed.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}') {
        return Err(failure(
            "validate print request",
            "Job name must be at most 80 characters and contain no control characters",
        ));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let copies = copies as u32;
    Ok(NormalizedInput {
        paper: input.paper.unwrap_or(PrintPaper::Letter),
        sides: input.sides.unwrap_or(PrintSides::TwoSidedLongEdge),
        copies,
        job_name: trimmed.to_owned(),
    })
}

/// Where the PDF comes from, resolved from the request's two source fields.
enum PdfSource<'a> {
    Url(Url),
    Inline(&'a str),
}

fn pdf_source(input: &PrintPdfInput) -> Result<PdfSource<'_>, PrinterError> {
    match (input.url.as_deref(), input.pdf_base64.as_deref()) {
        (Some(url), None) => assert_https_document_url(url, "Document URL")
            .map(PdfSource::Url)
            .map_err(|e| failure("validate document URL", e)),
        (None, Some(data)) => Ok(PdfSource::Inline(data)),
        _ => Err(failure(
            "validate print request",
            "Provide exactly one of url and pdfBase64",
        )),
    }
}

/// Decodes and checks inline bytes; errors never echo the input.
fn decode_inline_pdf(data: &str) -> Result<Bytes, String> {
    let too_large =
        || format!("Inline PDF must be between 1 byte and {MAX_INLINE_PDF_BYTES} bytes");
    if data.len() > MAX_INLINE_PDF_BASE64_CHARS {
        return Err(too_large());
    }
    let body = STANDARD
        .decode(data)
        .map_err(|_| "pdfBase64 is not valid standard base64".to_owned())?;
    if body.is_empty() || body.len() > MAX_INLINE_PDF_BYTES {
        return Err(too_large());
    }
    if !body.starts_with(b"%PDF-") {
        return Err("pdfBase64 does not contain a PDF".to_owned());
    }
    Ok(Bytes::from(body))
}

fn validate_pdf(download: &DownloadedFile) -> Result<Bytes, String> {
    let kind = download
        .content_type
        .as_deref()
        .and_then(|t| t.split(';').next())
        .map(|t| omni_core::js::trim(t).to_lowercase());
    if !matches!(
        kind.as_deref(),
        Some("application/pdf" | "application/octet-stream")
    ) {
        return Err("The document URL must return application/pdf".to_owned());
    }
    let body = &download.body;
    if body.is_empty() || body.len() > MAX_PDF_BYTES {
        return Err(format!(
            "PDF must be between 1 byte and {MAX_PDF_BYTES} bytes"
        ));
    }
    if !body.starts_with(b"%PDF-") {
        return Err("The downloaded file is not a PDF".to_owned());
    }
    Ok(body.clone())
}

/// Refuses encrypted PDFs and more than 25 pages.
pub fn parse_pdf_info(stdout: &str) -> Result<u32, String> {
    let lines = || stdout.split('\n').map(|line| line.trim_end_matches('\r'));
    let field = |line: &str, name: &str| -> Option<String> {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.to_owned())
    };
    if lines().any(|line| {
        field(line, "Encrypted").is_some_and(|value| {
            let value = value.trim_start_matches(omni_core::js::is_js_whitespace);
            value
                .get(..3)
                .is_some_and(|head| head.eq_ignore_ascii_case("yes"))
        })
    }) {
        return Err("Encrypted PDFs cannot be printed".to_owned());
    }
    let pages = lines()
        .filter_map(|line| field(line, "Pages"))
        .find_map(|value| {
            let digits = omni_core::js::trim(&value);
            (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
                .then(|| digits.parse::<u64>().ok())
                .flatten()
        });
    let pages = match pages {
        Some(pages) if pages >= 1 => pages,
        _ => return Err("Could not determine the PDF page count".to_owned()),
    };
    if pages > u64::from(MAX_PAGES) {
        return Err(format!("PDF has {pages} pages; the maximum is {MAX_PAGES}"));
    }
    u32::try_from(pages).map_err(|e| e.to_string())
}

/// `sha256(pdf || JSON.stringify(options))`.
fn duplicate_key(pdf: &[u8], options: &NormalizedInput) -> String {
    let json = json_stringify(&json!({
        "paper": options.paper.as_str(),
        "sides": options.sides.as_str(),
        "copies": options.copies,
        "jobName": options.job_name,
    }));
    let mut hasher = Sha256::new();
    hasher.update(pdf);
    hasher.update(json.as_bytes());
    hex::encode(hasher.finalize())
}

struct InFlightGuard<'a> {
    set: &'a Mutex<HashSet<String>>,
    key: Option<String>,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.set
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&key);
        }
    }
}

/// The printer service.
pub struct PrinterService {
    deps: PrinterDependencies,
    volatile: Mutex<HashMap<String, i64>>,
    in_flight: Mutex<HashSet<String>>,
}

impl PrinterService {
    pub fn new(deps: PrinterDependencies) -> Self {
        Self {
            deps,
            volatile: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    fn require_printer(&self) -> Result<&Arc<dyn PrinterClient>, PrinterError> {
        self.deps
            .printer
            .as_ref()
            .ok_or_else(|| failure("configure printer", "Printer is not configured"))
    }

    fn now(&self) -> i64 {
        self.deps.clock.now_ms()
    }

    /// Readiness, queue depth, toner and capabilities.
    pub async fn status(&self) -> Result<PrinterStatus, PrinterError> {
        let printer = self.require_printer()?;
        let status: IppPrinterStatus = printer
            .status()
            .await
            .map_err(|e| failure("read printer status", e))?;
        let accepting_jobs = status
            .raw
            .get("printer-is-accepting-jobs")
            .and_then(super::ipp::IppRaw::boolean);
        let queued_job_count = status
            .raw
            .get("queued-job-count")
            .and_then(super::ipp::IppRaw::number)
            .filter(|n| *n >= 0.0);
        let toner_percent = status
            .raw
            .get("marker-levels")
            .and_then(super::ipp::IppRaw::number)
            .filter(|n| (0.0..=100.0).contains(n));
        Ok(PrinterStatus {
            configured: true,
            name: status.name,
            uri: status.uri,
            ready: accepting_jobs != Some(false)
                && (status.state == "idle" || status.state == "processing"),
            state: status.state,
            state_reasons: status.state_reasons,
            accepting_jobs,
            queued_job_count,
            toner_percent,
            monochrome_only: true,
            default_sides: "two-sided-long-edge",
            supported_formats: status.supported_formats,
            supported_media: status.supported_media,
        })
    }

    async fn prune_duplicates(&self) -> Result<(), PrinterError> {
        let cutoff = self.now() - DUPLICATE_WINDOW_MS;
        self.deps
            .accepted
            .delete_older_than(cutoff)
            .await
            .map_err(|e| failure("prune accepted print jobs", e))?;
        self.volatile
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|_, accepted_at| *accepted_at > cutoff);
        Ok(())
    }

    /// Downloads or decodes, validates, converts and submits one PDF.
    pub async fn print_pdf(&self, input: &PrintPdfInput) -> Result<AcceptedPrintJob, PrinterError> {
        let printer = self.require_printer()?.clone();
        let options = normalize_input(input)?;
        let pdf = match pdf_source(input)? {
            PdfSource::Url(url) => {
                let download = self
                    .deps
                    .download
                    .download(&url)
                    .await
                    .map_err(|e| failure("download PDF", e))?;
                validate_pdf(&download).map_err(|e| failure("validate PDF", e))?
            }
            PdfSource::Inline(data) => {
                decode_inline_pdf(data).map_err(|e| failure("validate PDF", e))?
            }
        };
        let key = duplicate_key(&pdf, &options);
        self.prune_duplicates().await?;
        if !input.allow_duplicate {
            let durable = self
                .deps
                .accepted
                .get(&key)
                .await
                .map_err(|e| failure("read accepted print jobs", e))?;
            let volatile = self
                .volatile
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(&key);
            if durable.is_some() || volatile {
                return Err(failure(
                    "suppress duplicate",
                    "This exact document and print configuration was accepted within the last 5 minutes; set allowDuplicate to print it again",
                ));
            }
        }
        let _guard = {
            let mut in_flight = self
                .in_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !input.allow_duplicate && in_flight.contains(&key) {
                return Err(failure(
                    "reserve print job",
                    "This exact document and print configuration is already being submitted; wait for it to finish or set allowDuplicate to print another copy",
                ));
            }
            let reserved = !input.allow_duplicate;
            if reserved {
                in_flight.insert(key.clone());
            }
            InFlightGuard {
                set: &self.in_flight,
                key: reserved.then(|| key.clone()),
            }
        };
        let workspace = tempfile::Builder::new()
            .prefix("omni-printer-")
            .tempdir_in(&self.deps.temp_root)
            .map_err(|e| failure("create print workspace", e.to_string()))?;
        let result = self
            .submit(printer.as_ref(), workspace.path(), pdf, &options, &key)
            .await;
        // Cleanup must not replace the printer's authoritative result.
        if let Err(error) = workspace.close() {
            tracing::warn!(target: LOG, "Removing the print workspace failed: {error}");
        }
        result
    }

    async fn write_private_file(path: &Path, data: &[u8]) -> Result<(), PrinterError> {
        let path = path.to_owned();
        let data = data.to_vec();
        tokio::task::spawn_blocking(move || {
            use std::io::Write as _;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options.open(&path)?;
            file.write_all(&data)?;
            file.sync_all()
        })
        .await
        .map_err(|e| failure("write print workspace file", e.to_string()))?
        .map_err(|e| failure("write print workspace file", e.to_string()))
    }

    async fn submit(
        &self,
        printer: &dyn PrinterClient,
        directory: &Path,
        pdf: Bytes,
        options: &NormalizedInput,
        key: &str,
    ) -> Result<AcceptedPrintJob, PrinterError> {
        let input_path = directory.join("input.pdf");
        let raster_path = directory.join("output.raster");
        Self::write_private_file(&input_path, &pdf).await?;
        let input_arg = input_path.to_string_lossy().into_owned();
        let info = self
            .deps
            .processes
            .run_text("pdfinfo", std::slice::from_ref(&input_arg))
            .await
            .map_err(|e| failure("run pdfinfo", e))?;
        let pages = parse_pdf_info(&info).map_err(|e| failure("inspect PDF", e))?;
        let cups_options = [
            format!("PageSize={}", options.paper.cups()),
            format!("Duplex={}", options.sides.cups()),
            "print-scaling=fit".to_owned(),
        ];
        let mut cups_args: Vec<String> = [
            "-p",
            BRLASER_PPD,
            "-m",
            "application/vnd.cups-raster",
            "-i",
            "application/pdf",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        for option in &cups_options {
            cups_args.push("-o".to_owned());
            cups_args.push(option.clone());
        }
        cups_args.push(input_arg);
        let raster = self
            .deps
            .processes
            .run_binary(CUPS_FILTER, &cups_args, &[], MAX_PRINT_DATA_BYTES)
            .await
            .map_err(|e| failure(&format!("run {CUPS_FILTER}"), e))?;
        Self::write_private_file(&raster_path, &raster).await?;
        let brlaser_args = vec![
            "1".to_owned(),
            "omni".to_owned(),
            options.job_name.clone(),
            "1".to_owned(),
            cups_options.join(" "),
            raster_path.to_string_lossy().into_owned(),
        ];
        let print_data = self
            .deps
            .processes
            .run_binary(
                BRLASER_FILTER,
                &brlaser_args,
                &[("PPD".to_owned(), BRLASER_PPD.to_owned())],
                MAX_PRINT_DATA_BYTES,
            )
            .await
            .map_err(|e| failure(&format!("run {BRLASER_FILTER}"), e))?;
        if print_data.is_empty() || print_data.len() > self.deps.max_print_data_bytes {
            return Err(failure(
                "convert PDF",
                format!(
                    "Converted print data exceeds the {}-byte limit",
                    self.deps.max_print_data_bytes
                ),
            ));
        }
        // Never retry submission: a transport failure can happen after acceptance.
        let print_options = PrintOptions {
            copies: options.copies,
            media: options.paper.media().to_owned(),
            sides: options.sides.as_str().to_owned(),
            color_mode: "monochrome".to_owned(),
            document_format: "application/octet-stream".to_owned(),
            job_name: options.job_name.clone(),
            fit_to_page: true,
        };
        let job: PrintJob = printer
            .print(Bytes::from(print_data), &print_options)
            .await
            .map_err(|e| failure("submit IPP job", e))?;
        // Acceptance is irreversible. Persist suppression before fallible polling
        // so a process restart cannot turn an uncertain retry into a second print.
        let accepted_at = self.now();
        self.volatile
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key.to_owned(), accepted_at);
        #[allow(clippy::cast_precision_loss)]
        let record = AcceptedPrintRecord {
            fingerprint: key.to_owned(),
            accepted_at: accepted_at as f64,
            job_id: job.id,
            job_uri: job.uri.clone(),
            job_state: job.state.clone(),
            job_name: job.name.clone(),
            extra: Extra::default(),
        };
        let suppression_persisted = match self.deps.accepted.upsert(record).await {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(target: LOG, "Persisting print duplicate suppression failed: {error}");
                false
            }
        };
        let final_status = self.wait_for_final_job_status(printer, &job).await?;
        let completed = final_status
            .as_ref()
            .is_some_and(|s| s.state == "completed");
        let message = if !suppression_persisted {
            "The printer accepted the job, but durable duplicate suppression failed; do not retry it automatically"
        } else if completed {
            "The printer completed the job successfully"
        } else {
            "The printer accepted the job; physical completion is not confirmed"
        };
        Ok(AcceptedPrintJob {
            accepted: true,
            completed,
            job_id: job.id,
            job_uri: job.uri,
            job_state: final_status
                .as_ref()
                .map_or(job.state, |status| status.state.clone()),
            job_name: job.name,
            pages,
            copies: options.copies,
            paper: options.paper,
            sides: options.sides,
            impressions_completed: final_status.and_then(|s| s.impressions_completed),
            message: message.to_owned(),
        })
    }

    async fn wait_for_final_job_status(
        &self,
        printer: &dyn PrinterClient,
        job: &PrintJob,
    ) -> Result<Option<JobStatus>, PrinterError> {
        if job.id.is_none() {
            return Ok(None);
        }
        let mut latest: Option<JobStatus> = None;
        for attempt in 0..JOB_STATUS_MAX_POLLS {
            if attempt > 0 {
                tokio::time::sleep(self.deps.poll_interval).await;
            }
            let Some(poll) = printer.job_status(&job.uri) else {
                return Ok(None);
            };
            let Ok(status) = poll.await else {
                // A status read failure ends polling; acceptance stands.
                break;
            };
            let terminal = matches!(status.state.as_str(), "completed" | "aborted" | "canceled");
            latest = Some(status);
            if terminal {
                break;
            }
        }
        let Some(status) = latest else {
            return Ok(None);
        };
        if status.state == "aborted" || status.state == "canceled" {
            let reasons = if status.state_reasons.is_empty() {
                "no reason reported".to_owned()
            } else {
                status.state_reasons.join(", ")
            };
            return Err(failure(
                "complete printer job",
                format!("Printer {} the job: {reasons}", status.state),
            ));
        }
        Ok((status.state == "completed").then_some(status))
    }
}

/// The poll interval used in production.
pub const DEFAULT_POLL_INTERVAL: Duration = JOB_STATUS_POLL;
