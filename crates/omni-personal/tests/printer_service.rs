//! The printer service, plus IPP encoding against a stub printer and the
//! durable `printer-accepted-job` store. The HTTPS-downgrade case drives the
//! real public downloader through a wiremock redirect.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use futures::future::BoxFuture;
use omni_core::clock::{SharedClock, TestClock};
use omni_http::Url;
use omni_http::public::PublicHttpClient;
use omni_personal::printer::ipp::{
    IppPrinterClient, IppPrinterStatus, IppRaw, JobStatus, PrintJob, PrintOptions, PrinterClient,
};
use omni_personal::printer::pipeline::{
    DownloadedFile, PdfDownloader, PrintProcesses, PublicPdfDownloader,
};
use omni_personal::printer::service::{
    AcceptedPrintStore, DurableAcceptedPrintStore, MAX_INLINE_PDF_BYTES, MAX_PRINT_DATA_BYTES,
};
use omni_personal::printer::{
    AcceptedPrintRecord, PrintPaper, PrintPdfInput, PrintSides, PrinterDependencies, PrinterService,
};
use omni_testkit::{TEST_EPOCH_MS, TestStore, mock_http, mock_server, test_clock};
use tokio::sync::Notify;

const PDF: &[u8] = b"%PDF-1.7\nfixture";

#[derive(Default)]
struct FakePrinter {
    raw: Mutex<HashMap<String, IppRaw>>,
    prints: Mutex<Vec<(Bytes, PrintOptions)>>,
    print_failures: Mutex<VecDeque<String>>,
    job_statuses: Mutex<VecDeque<Result<JobStatus, String>>>,
    job_status_calls: Mutex<Vec<String>>,
    block_print: Mutex<Option<Arc<Notify>>>,
    print_started: Notify,
}

impl FakePrinter {
    fn print_count(&self) -> usize {
        self.prints.lock().unwrap().len()
    }
}

fn completed() -> JobStatus {
    JobStatus {
        state: "completed".into(),
        state_reasons: vec!["job-completed-successfully".into()],
        impressions_completed: Some(2.0),
    }
}

impl PrinterClient for FakePrinter {
    fn status(&self) -> BoxFuture<'_, Result<IppPrinterStatus, String>> {
        Box::pin(async move {
            Ok(IppPrinterStatus {
                name: Some("Brother HL-L2370DW".into()),
                uri: "ipp://10.10.1.47:631/ipp/print".into(),
                state: "idle".into(),
                state_reasons: vec![],
                supported_formats: vec!["application/octet-stream".into()],
                supported_media: vec!["na_letter_8.5x11in".into()],
                raw: self.raw.lock().unwrap().clone().into_iter().collect(),
            })
        })
    }

    fn print<'a>(
        &'a self,
        data: Bytes,
        options: &'a PrintOptions,
    ) -> BoxFuture<'a, Result<PrintJob, String>> {
        Box::pin(async move {
            self.prints.lock().unwrap().push((data, options.clone()));
            self.print_started.notify_waiters();
            let gate = self.block_print.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            if let Some(error) = self.print_failures.lock().unwrap().pop_front() {
                return Err(error);
            }
            Ok(PrintJob {
                id: Some(42.0),
                uri: "ipp://10.10.1.47/jobs/42".into(),
                state: "pending".into(),
                name: options.job_name.clone(),
            })
        })
    }

    fn job_status<'a>(
        &'a self,
        job_uri: &'a str,
    ) -> Option<BoxFuture<'a, Result<JobStatus, String>>> {
        Some(Box::pin(async move {
            self.job_status_calls
                .lock()
                .unwrap()
                .push(job_uri.to_owned());
            self.job_statuses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(completed()))
        }))
    }
}

struct FakeDownload {
    reply: Mutex<DownloadedFile>,
    calls: AtomicUsize,
}

impl PdfDownloader for FakeDownload {
    fn download<'a>(&'a self, _url: &'a Url) -> BoxFuture<'a, Result<DownloadedFile, String>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.reply.lock().unwrap().clone())
        })
    }
}

type BinaryCall = (String, Vec<String>, Vec<(String, String)>);

#[derive(Default)]
struct FakeProcesses {
    text_calls: Mutex<Vec<(String, Vec<String>)>>,
    binary_calls: Mutex<Vec<BinaryCall>>,
    pdfinfo: Mutex<Option<String>>,
    pdfinfo_error: Mutex<Option<String>>,
    binary_output: Mutex<Option<Vec<u8>>>,
    observed_pdf: Mutex<Option<Vec<u8>>>,
}

impl PrintProcesses for FakeProcesses {
    fn run_text<'a>(
        &'a self,
        executable: &'a str,
        args: &'a [String],
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.text_calls
                .lock()
                .unwrap()
                .push((executable.to_owned(), args.to_vec()));
            assert_eq!(executable, "pdfinfo");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = std::fs::metadata(&args[0]).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600);
            }
            *self.observed_pdf.lock().unwrap() = Some(std::fs::read(&args[0]).unwrap());
            if let Some(error) = self.pdfinfo_error.lock().unwrap().clone() {
                return Err(error);
            }
            Ok(self
                .pdfinfo
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "Pages: 2\nEncrypted: no\n".into()))
        })
    }

    fn run_binary<'a>(
        &'a self,
        executable: &'a str,
        args: &'a [String],
        environment: &'a [(String, String)],
        _max_bytes: usize,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            self.binary_calls.lock().unwrap().push((
                executable.to_owned(),
                args.to_vec(),
                environment.to_vec(),
            ));
            if let Some(out) = self.binary_output.lock().unwrap().clone() {
                return Ok(out);
            }
            Ok(if executable.ends_with("cupsfilter") {
                b"cups-raster-fixture".to_vec()
            } else {
                b"brlaser-fixture".to_vec()
            })
        })
    }
}

#[derive(Default)]
struct MemoryStore {
    records: Mutex<HashMap<String, AcceptedPrintRecord>>,
    fail_upsert: bool,
}

impl AcceptedPrintStore for MemoryStore {
    fn get<'a>(
        &'a self,
        fingerprint: &'a str,
    ) -> BoxFuture<'a, Result<Option<AcceptedPrintRecord>, String>> {
        Box::pin(async move { Ok(self.records.lock().unwrap().get(fingerprint).cloned()) })
    }

    fn upsert(&self, record: AcceptedPrintRecord) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            if self.fail_upsert {
                return Err("database unavailable".into());
            }
            self.records
                .lock()
                .unwrap()
                .insert(record.fingerprint.clone(), record);
            Ok(())
        })
    }

    fn delete_older_than(&self, cutoff: i64) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            #[allow(clippy::cast_precision_loss)]
            let cutoff = cutoff as f64;
            self.records
                .lock()
                .unwrap()
                .retain(|_, record| record.accepted_at > cutoff);
            Ok(())
        })
    }
}

struct Fixture {
    temp: tempfile::TempDir,
    printer: Arc<FakePrinter>,
    download: Arc<FakeDownload>,
    processes: Arc<FakeProcesses>,
    accepted: Arc<dyn AcceptedPrintStore>,
    clock: Arc<TestClock>,
    max_print_data_bytes: usize,
}

impl Fixture {
    fn new() -> Self {
        Self {
            temp: tempfile::tempdir().unwrap(),
            printer: Arc::new(FakePrinter::default()),
            download: Arc::new(FakeDownload {
                reply: Mutex::new(DownloadedFile {
                    body: Bytes::from_static(PDF),
                    content_type: Some("application/pdf; charset=binary".into()),
                }),
                calls: AtomicUsize::new(0),
            }),
            processes: Arc::new(FakeProcesses::default()),
            accepted: Arc::new(MemoryStore::default()),
            clock: test_clock(1_000_000),
            max_print_data_bytes: MAX_PRINT_DATA_BYTES,
        }
    }

    fn service(&self) -> PrinterService {
        let clock: SharedClock = self.clock.clone();
        PrinterService::new(PrinterDependencies {
            printer: Some(self.printer.clone()),
            download: self.download.clone(),
            processes: self.processes.clone(),
            accepted: self.accepted.clone(),
            clock,
            temp_root: self.temp.path().to_path_buf(),
            max_print_data_bytes: self.max_print_data_bytes,
            poll_interval: Duration::from_millis(1),
        })
    }

    fn temp_is_empty(&self) -> bool {
        std::fs::read_dir(self.temp.path())
            .unwrap()
            .next()
            .is_none()
    }
}

fn input() -> PrintPdfInput {
    PrintPdfInput {
        url: Some("https://example.com/file.pdf".into()),
        ..PrintPdfInput::default()
    }
}

fn inline(pdf: &[u8]) -> PrintPdfInput {
    PrintPdfInput {
        pdf_base64: Some(STANDARD.encode(pdf)),
        ..PrintPdfInput::default()
    }
}

#[tokio::test]
async fn normalizes_printer_readiness_and_consumable_status() {
    let f = Fixture::new();
    f.printer.raw.lock().unwrap().extend([
        ("printer-is-accepting-jobs".to_owned(), IppRaw::Bool(false)),
        ("queued-job-count".to_owned(), IppRaw::Number(3.0)),
        (
            "marker-levels".to_owned(),
            IppRaw::Array(vec![IppRaw::Number(61.0), IppRaw::Number(100.0)]),
        ),
    ]);
    let status = f.service().status().await.unwrap();
    assert!(status.configured);
    assert_eq!(status.state, "idle");
    assert!(!status.ready);
    assert_eq!(status.accepting_jobs, Some(false));
    assert_eq!(status.queued_job_count, Some(3.0));
    assert_eq!(status.toner_percent, Some(61.0));
    assert!(status.monochrome_only);
    assert_eq!(status.default_sides, "two-sided-long-edge");
}

#[tokio::test]
async fn converts_submits_and_confirms_a_monochrome_duplex_brlaser_job() {
    let f = Fixture::new();
    let result = f
        .service()
        .print_pdf(&PrintPdfInput {
            paper: Some(PrintPaper::A4),
            copies: Some(2.0),
            job_name: Some("Board packet".into()),
            ..input()
        })
        .await
        .unwrap();
    assert!(result.accepted && result.completed);
    assert_eq!(result.job_id, Some(42.0));
    assert_eq!(result.pages, 2);
    assert_eq!(result.copies, 2);
    assert_eq!(result.paper, PrintPaper::A4);
    assert_eq!(result.sides, PrintSides::TwoSidedLongEdge);
    assert_eq!(result.impressions_completed, Some(2.0));
    assert!(result.message.contains("completed the job successfully"));
    assert_eq!(f.processes.text_calls.lock().unwrap()[0].0, "pdfinfo");
    let binary = f.processes.binary_calls.lock().unwrap().clone();
    assert_eq!(binary[0].0, "/usr/sbin/cupsfilter");
    for option in ["PageSize=A4", "Duplex=DuplexNoTumble", "print-scaling=fit"] {
        assert!(binary[0].1.contains(&option.to_owned()));
    }
    assert_eq!(binary[1].0, "/usr/lib/cups/filter/rastertobrlaser");
    assert_eq!(
        binary[1].2,
        vec![(
            "PPD".to_owned(),
            "/usr/share/omni-printing/brother-hll2370dw.ppd".to_owned()
        )]
    );
    assert!(
        binary[1]
            .1
            .contains(&"PageSize=A4 Duplex=DuplexNoTumble print-scaling=fit".to_owned())
    );
    let prints = f.printer.prints.lock().unwrap().clone();
    let (data, options) = &prints[0];
    assert_eq!(data.as_ref(), b"brlaser-fixture");
    assert_eq!(options.copies, 2);
    assert_eq!(options.media, "iso_a4_210x297mm");
    assert_eq!(options.sides, "two-sided-long-edge");
    assert_eq!(options.color_mode, "monochrome");
    assert_eq!(options.document_format, "application/octet-stream");
    assert_eq!(options.job_name, "Board packet");
    assert_eq!(
        f.printer.job_status_calls.lock().unwrap().clone(),
        vec!["ipp://10.10.1.47/jobs/42".to_owned()]
    );
    assert!(f.temp_is_empty());
}

#[tokio::test]
async fn passes_sides_through_both_cups_filters() {
    for (sides, duplex) in [
        (PrintSides::OneSided, "Duplex=None"),
        (PrintSides::TwoSidedShortEdge, "Duplex=DuplexTumble"),
    ] {
        let f = Fixture::new();
        f.service()
            .print_pdf(&PrintPdfInput {
                sides: Some(sides),
                ..input()
            })
            .await
            .unwrap();
        let binary = f.processes.binary_calls.lock().unwrap().clone();
        assert!(binary[0].1.contains(&duplex.to_owned()));
        assert!(
            binary[1]
                .1
                .contains(&format!("PageSize=Letter {duplex} print-scaling=fit"))
        );
    }
}

#[tokio::test]
async fn reports_a_printer_aborted_job_as_a_failure() {
    let f = Fixture::new();
    f.printer
        .job_statuses
        .lock()
        .unwrap()
        .push_back(Ok(JobStatus {
            state: "aborted".into(),
            state_reasons: vec!["document-format-error".into()],
            impressions_completed: Some(0.0),
        }));
    let error = f.service().print_pdf(&input()).await.unwrap_err();
    assert!(
        error
            .message
            .contains("Printer aborted the job: document-format-error")
    );
}

#[tokio::test]
async fn rejects_an_invalid_download() {
    let mut oversized = b"%PDF-".to_vec();
    oversized.extend(vec![0u8; 20 * 1024 * 1024]);
    for (content_type, body, message) in [
        ("text/html", PDF.to_vec(), "must return application/pdf"),
        ("application/pdf", b"not a pdf".to_vec(), "not a PDF"),
        ("application/pdf", oversized, "PDF must be between"),
    ] {
        let f = Fixture::new();
        *f.download.reply.lock().unwrap() = DownloadedFile {
            body: Bytes::from(body),
            content_type: Some(content_type.into()),
        };
        let error = f.service().print_pdf(&input()).await.unwrap_err();
        assert!(error.message.contains(message), "{}", error.message);
        assert_eq!(f.printer.print_count(), 0);
        assert!(f.processes.text_calls.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn rejects_encrypted_and_oversized_page_pdfs_and_cleans_temporary_files() {
    let f = Fixture::new();
    *f.processes.pdfinfo.lock().unwrap() = Some("Pages: 26\nEncrypted: yes\n".into());
    let error = f.service().print_pdf(&input()).await.unwrap_err();
    assert!(error.message.contains("Encrypted PDFs cannot be printed"));
    assert_eq!(f.printer.print_count(), 0);
    assert!(f.temp_is_empty());

    *f.processes.pdfinfo.lock().unwrap() = Some("Pages: 26\nEncrypted: no\n".into());
    let error = f.service().print_pdf(&input()).await.unwrap_err();
    assert!(error.message.contains("maximum is 25"));
    assert!(f.temp_is_empty());
}

#[tokio::test]
async fn rejects_oversized_converted_print_data_before_submission() {
    let mut f = Fixture::new();
    f.max_print_data_bytes = 10;
    *f.processes.binary_output.lock().unwrap() = Some(vec![0u8; 11]);
    let error = f.service().print_pdf(&input()).await.unwrap_err();
    assert!(error.message.contains("Converted print data exceeds"));
    assert_eq!(f.printer.print_count(), 0);
    assert!(f.temp_is_empty());
}

#[tokio::test]
async fn suppresses_only_accepted_exact_duplicates_for_five_minutes() {
    let f = Fixture::new();
    f.clock.set(10_000);
    let service = f.service();
    service.print_pdf(&input()).await.unwrap();
    let error = service.print_pdf(&input()).await.unwrap_err();
    assert!(error.message.contains("allowDuplicate"));
    service
        .print_pdf(&PrintPdfInput {
            allow_duplicate: true,
            ..input()
        })
        .await
        .unwrap();
    // The test clock follows real elapsed time; leave a margin past the window.
    f.clock.set(10_000 + 5 * 60 * 1000 + 1_000);
    service.print_pdf(&input()).await.unwrap();
    assert_eq!(f.printer.print_count(), 3);
}

#[tokio::test]
async fn suppresses_an_accepted_duplicate_after_the_service_restarts() {
    let f = Fixture::new();
    f.service().print_pdf(&input()).await.unwrap();
    let error = f.service().print_pdf(&input()).await.unwrap_err();
    assert!(error.message.contains("allowDuplicate"));
    assert_eq!(f.printer.print_count(), 1);
}

#[tokio::test]
async fn reports_acceptance_without_inviting_a_retry_when_durable_suppression_fails() {
    let mut f = Fixture::new();
    f.accepted = Arc::new(MemoryStore {
        fail_upsert: true,
        ..MemoryStore::default()
    });
    let service = f.service();
    let result = service.print_pdf(&input()).await.unwrap();
    assert!(result.accepted);
    assert_eq!(
        result.message,
        "The printer accepted the job, but durable duplicate suppression failed; do not retry it automatically"
    );
    let error = service.print_pdf(&input()).await.unwrap_err();
    assert!(error.message.contains("allowDuplicate"));
    assert_eq!(f.printer.print_count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn prevents_concurrent_duplicate_submissions_unless_explicitly_allowed() {
    let f = Fixture::new();
    let gate = Arc::new(Notify::new());
    *f.printer.block_print.lock().unwrap() = Some(gate.clone());
    let service = Arc::new(f.service());
    let started = f.printer.print_started.notified();
    tokio::pin!(started);
    started.as_mut().enable();
    let first = {
        let service = service.clone();
        tokio::spawn(async move { service.print_pdf(&input()).await })
    };
    started.await;
    let error = service.print_pdf(&input()).await.unwrap_err();
    assert!(error.message.contains("already being submitted"));
    gate.notify_one();
    assert!(first.await.unwrap().unwrap().accepted);
    assert_eq!(f.printer.print_count(), 1);
}

#[tokio::test]
async fn does_not_suppress_a_retry_when_ipp_submission_fails() {
    let f = Fixture::new();
    f.printer
        .print_failures
        .lock()
        .unwrap()
        .push_back("printer connection closed".into());
    let service = f.service();
    let error = service.print_pdf(&input()).await.unwrap_err();
    assert!(error.message.contains("connection closed"));
    assert!(service.print_pdf(&input()).await.unwrap().accepted);
    assert_eq!(f.printer.print_count(), 2);
    assert!(f.temp_is_empty());
}

#[tokio::test]
async fn suppresses_duplicates_immediately_after_ipp_acceptance_when_status_polling_fails() {
    let f = Fixture::new();
    f.printer
        .job_statuses
        .lock()
        .unwrap()
        .push_back(Err("Get-Job-Attributes timed out".into()));
    let service = f.service();
    let result = service.print_pdf(&input()).await.unwrap();
    assert!(result.accepted && !result.completed);
    assert_eq!(
        result.message,
        "The printer accepted the job; physical completion is not confirmed"
    );
    assert!(
        service
            .print_pdf(&input())
            .await
            .unwrap_err()
            .message
            .contains("allowDuplicate")
    );
    assert_eq!(f.printer.print_count(), 1);
    assert!(f.temp_is_empty());
}

#[tokio::test]
async fn validates_the_public_input_shape_before_downloading() {
    let f = Fixture::new();
    let service = f.service();
    let error = service
        .print_pdf(&PrintPdfInput {
            url: Some("http://example.com/file.pdf".into()),
            ..input()
        })
        .await
        .unwrap_err();
    assert!(error.message.contains("public HTTPS URL"));
    let error = service
        .print_pdf(&PrintPdfInput {
            copies: Some(4.0),
            ..input()
        })
        .await
        .unwrap_err();
    assert!(
        error
            .message
            .contains("Copies must be an integer from 1 to 3")
    );
    assert_eq!(f.download.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn prints_inline_pdf_bytes_without_downloading() {
    let f = Fixture::new();
    let job = f
        .service()
        .print_pdf(&PrintPdfInput {
            job_name: Some("Return sheet".into()),
            ..inline(PDF)
        })
        .await
        .unwrap();
    assert!(job.accepted && job.completed);
    assert_eq!(job.job_name, "Return sheet");
    assert_eq!(f.download.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.processes.observed_pdf.lock().unwrap().as_deref(),
        Some(PDF)
    );
    assert_eq!(f.printer.print_count(), 1);
    assert!(f.temp_is_empty());
}

#[tokio::test]
async fn rejects_invalid_and_oversized_inline_pdfs_before_converting() {
    let mut largest = b"%PDF-".to_vec();
    largest.resize(MAX_INLINE_PDF_BYTES + 1, 0);
    for (data, message) in [
        ("%%%not base64".to_owned(), "not valid standard base64"),
        (String::new(), "between 1 byte and 3000000 bytes"),
        (
            STANDARD.encode(b"<html>not a pdf</html>"),
            "does not contain a PDF",
        ),
        (
            STANDARD.encode(&largest),
            "between 1 byte and 3000000 bytes",
        ),
        ("A".repeat(4_000_004), "between 1 byte and 3000000 bytes"),
    ] {
        let f = Fixture::new();
        let error = f
            .service()
            .print_pdf(&PrintPdfInput {
                pdf_base64: Some(data),
                ..PrintPdfInput::default()
            })
            .await
            .unwrap_err();
        assert!(error.message.contains(message), "{}", error.message);
        assert!(!error.message.contains("not a pdf"), "{}", error.message);
        assert!(f.processes.text_calls.lock().unwrap().is_empty());
        assert_eq!(f.printer.print_count(), 0);
    }
}

#[tokio::test]
async fn requires_exactly_one_document_source() {
    let f = Fixture::new();
    let service = f.service();
    for request in [
        PrintPdfInput {
            pdf_base64: Some(STANDARD.encode(PDF)),
            ..input()
        },
        PrintPdfInput::default(),
    ] {
        let error = service.print_pdf(&request).await.unwrap_err();
        assert_eq!(error.message, "Provide exactly one of url and pdfBase64");
    }
    assert_eq!(f.download.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.printer.print_count(), 0);
}

#[tokio::test]
async fn suppresses_the_same_pdf_across_url_and_inline_sources() {
    let f = Fixture::new();
    let service = f.service();
    service.print_pdf(&inline(PDF)).await.unwrap();
    let error = service.print_pdf(&inline(PDF)).await.unwrap_err();
    assert!(error.message.contains("allowDuplicate"));
    let error = service.print_pdf(&input()).await.unwrap_err();
    assert!(error.message.contains("allowDuplicate"));
    service
        .print_pdf(&PrintPdfInput {
            allow_duplicate: true,
            ..inline(PDF)
        })
        .await
        .unwrap();
    service
        .print_pdf(&inline(b"%PDF-1.7\nanother document"))
        .await
        .unwrap();
    assert_eq!(f.printer.print_count(), 3);
}

#[tokio::test]
async fn rejects_an_https_redirect_that_downgrades_to_http() {
    let server = mock_server().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(
            wiremock::ResponseTemplate::new(302)
                .insert_header("location", "http://example.com/file.pdf"),
        )
        .mount(&server)
        .await;
    let downloader = PublicPdfDownloader::new(
        PublicHttpClient::new(&mock_http(&server, &["https://example.com"]))
            .allow_loopback_for_tests(),
        Duration::from_secs(1),
    );
    let url = Url::parse("https://example.com/file.pdf").unwrap();
    let error = downloader.download(&url).await.unwrap_err();
    assert_eq!(
        error,
        "Document redirects must be a public HTTPS URL without credentials"
    );
}

#[tokio::test]
async fn writes_the_downloaded_bytes_unchanged_before_inspection() {
    let f = Fixture::new();
    *f.processes.pdfinfo_error.lock().unwrap() = Some("inspection stopped".into());
    let error = f.service().print_pdf(&input()).await.unwrap_err();
    assert!(error.message.contains("inspection stopped"));
    assert_eq!(
        f.processes.observed_pdf.lock().unwrap().as_deref(),
        Some(PDF)
    );
    assert!(f.temp_is_empty());
}

#[tokio::test]
async fn durable_store_prunes_records_at_the_window() {
    let clock: SharedClock = test_clock(TEST_EPOCH_MS);
    let store = TestStore::new(clock).await;
    let durable = DurableAcceptedPrintStore::new(store.store.clone());
    for (fingerprint, accepted_at) in [("old", 100.0), ("new", 200.0)] {
        durable
            .upsert(AcceptedPrintRecord {
                fingerprint: fingerprint.into(),
                accepted_at,
                job_id: None,
                job_uri: String::new(),
                job_state: "pending".into(),
                job_name: "MCP print job".into(),
                extra: Default::default(),
            })
            .await
            .unwrap();
    }
    durable.delete_older_than(100).await.unwrap();
    assert!(durable.get("old").await.unwrap().is_none());
    assert_eq!(durable.get("new").await.unwrap().unwrap().job_id, None);
}

/// The durable `printer-accepted-job` key matches the stored keys:
/// `sha256(pdf || JSON.stringify({paper, sides, copies, jobName}))`, pinned
/// for the fixture PDF and the default options.
#[tokio::test]
async fn durable_fingerprint_matches_stored_keys() {
    let mut f = Fixture::new();
    let memory = Arc::new(MemoryStore::default());
    f.accepted = memory.clone();
    f.service().print_pdf(&input()).await.unwrap();
    let keys: Vec<String> = memory.records.lock().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        vec!["5895ac37eb1a40beb8359889a64e76f5eac8534fd5ff1b37b227b7cd9fcfffa8".to_owned()]
    );
}

// --- IPP wire encoding against a stub printer ---

fn ipp_response(status: u16, request_id: i32, job: &[(&str, ipp::value::IppValue)]) -> Vec<u8> {
    use ipp::attribute::IppAttribute;
    use ipp::model::DelimiterTag;
    let mut out = vec![2, 0];
    out.extend_from_slice(&status.to_be_bytes());
    out.extend_from_slice(&request_id.to_be_bytes());
    let mut attrs = ipp::attribute::IppAttributes::new();
    for (name, value) in job {
        attrs.add(
            DelimiterTag::JobAttributes,
            IppAttribute::with_name(name, value.clone()).unwrap(),
        );
    }
    out.extend_from_slice(&attrs.to_bytes());
    out
}

#[tokio::test]
async fn ipp_client_encodes_print_job_and_reads_the_accepted_job() {
    use ipp::value::IppValue;
    let server = mock_server().await;
    // The request id is seeded at 7, so Print-Job uses 7.
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::header(
            "content-type",
            "application/ipp",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "application/ipp")
                .set_body_bytes(ipp_response(
                    0,
                    7,
                    &[
                        ("job-id", IppValue::Integer(42)),
                        (
                            "job-uri",
                            IppValue::Uri("ipp://printer/jobs/42".try_into().unwrap()),
                        ),
                        ("job-state", IppValue::Enum(3)),
                    ],
                )),
        )
        .mount(&server)
        .await;
    let client = IppPrinterClient::new(
        omni_testkit::no_network(),
        &format!(
            "ipp://{}:{}/ipp/print",
            server.address().ip(),
            server.address().port()
        ),
        Duration::from_secs(5),
        omni_http::SideEffectMode::Live,
        7,
    )
    .unwrap();
    let options = PrintOptions {
        copies: 2,
        media: "na_letter_8.5x11in".into(),
        sides: "two-sided-long-edge".into(),
        color_mode: "monochrome".into(),
        document_format: "application/octet-stream".into(),
        job_name: "Board packet".into(),
        fit_to_page: true,
    };
    let job = client
        .print(Bytes::from_static(b"DATA"), &options)
        .await
        .unwrap();
    assert_eq!(job.id, Some(42.0));
    assert_eq!(job.uri, "ipp://printer/jobs/42");
    assert_eq!(job.state, "pending");
    let request = &server.received_requests().await.unwrap()[0];
    assert_eq!(request.url.path(), "/ipp/print");
    let body = &request.body;
    assert_eq!(&body[..4], &[2, 0, 0, 2]); // IPP/2.0 Print-Job
    assert!(body.ends_with(b"DATA"));
    let text = String::from_utf8_lossy(body);
    for needle in [
        "printer-uri",
        "requesting-user-name",
        "job-name",
        "Board packet",
        "document-format",
        "application/octet-stream",
        "copies",
        "na_letter_8.5x11in",
        "two-sided-long-edge",
        "print-color-mode",
        "monochrome",
        "print-scaling",
    ] {
        assert!(text.contains(needle), "{needle}");
    }
}

#[tokio::test]
async fn ipp_client_records_print_jobs_in_record_mode() {
    let server = mock_server().await;
    let client = IppPrinterClient::new(
        omni_testkit::no_network(),
        &format!("ipp://{}/ipp/print", server.address()),
        Duration::from_secs(5),
        omni_http::SideEffectMode::Record,
        1,
    )
    .unwrap();
    let options = PrintOptions {
        copies: 1,
        media: "na_letter_8.5x11in".into(),
        sides: "one-sided".into(),
        color_mode: "monochrome".into(),
        document_format: "application/octet-stream".into(),
        job_name: "x".into(),
        fit_to_page: true,
    };
    assert!(
        client
            .print(Bytes::from_static(b"DATA"), &options)
            .await
            .is_err()
    );
    assert_eq!(client.recorded().len(), 1);
    assert!(server.received_requests().await.unwrap().is_empty());
}

fn ipp_operation_response(
    status: u16,
    request_id: i32,
    operation: &[(&str, ipp::value::IppValue)],
) -> Vec<u8> {
    use ipp::attribute::IppAttribute;
    use ipp::model::DelimiterTag;
    let mut out = vec![2, 0];
    out.extend_from_slice(&status.to_be_bytes());
    out.extend_from_slice(&request_id.to_be_bytes());
    let mut attrs = ipp::attribute::IppAttributes::new();
    for (name, value) in operation {
        attrs.add(
            DelimiterTag::OperationAttributes,
            IppAttribute::with_name(name, value.clone()).unwrap(),
        );
    }
    out.extend_from_slice(&attrs.to_bytes());
    out
}

fn letter_options() -> PrintOptions {
    PrintOptions {
        copies: 1,
        media: "na_letter_8.5x11in".into(),
        sides: "one-sided".into(),
        color_mode: "monochrome".into(),
        document_format: "application/octet-stream".into(),
        job_name: "x".into(),
        fit_to_page: true,
    }
}

/// URL credentials become Basic auth (never part of
/// the request URL), job fields fall back to any group, and an operation error
/// without `status-message` names the status code.
#[tokio::test]
async fn ipp_client_edges() {
    use ipp::value::IppValue;
    let server = mock_server().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "application/ipp")
                .set_body_bytes(ipp_operation_response(
                    0,
                    3,
                    &[
                        ("job-id", IppValue::Integer(9)),
                        (
                            "job-uri",
                            IppValue::Uri("ipp://printer/jobs/9".try_into().unwrap()),
                        ),
                    ],
                )),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "application/ipp")
                .set_body_bytes(ipp_operation_response(0x0507, 4, &[])),
        )
        .mount(&server)
        .await;
    let client = IppPrinterClient::new(
        omni_testkit::no_network(),
        &format!(
            "ipp://us%40er:p%2Bss@{}:{}/ipp/print",
            server.address().ip(),
            server.address().port()
        ),
        Duration::from_secs(5),
        omni_http::SideEffectMode::Live,
        3,
    )
    .unwrap();
    let job = client
        .print(Bytes::from_static(b"DATA"), &letter_options())
        .await
        .unwrap();
    assert_eq!(job.id, Some(9.0));
    assert_eq!(job.uri, "ipp://printer/jobs/9");
    assert_eq!(job.state, "unknown");
    let busy = client
        .print(Bytes::from_static(b"DATA"), &letter_options())
        .await
        .unwrap_err();
    assert_eq!(busy, "IPP operation error (0x507 - busy)");
    let request = &server.received_requests().await.unwrap()[0];
    assert_eq!(
        request.headers.get("authorization").unwrap(),
        // base64("us@er:p+ss")
        "Basic dXNAZXI6cCtzcw=="
    );
    assert!(request.url.username().is_empty());
    assert!(request.url.password().is_none());
}

#[test]
fn maps_ipp_urls_to_http() {
    use omni_personal::printer::ipp::ipp_http_url;
    assert_eq!(
        ipp_http_url("ipp://10.10.1.47/ipp/print").unwrap().as_str(),
        "http://10.10.1.47:631/ipp/print"
    );
    assert_eq!(
        ipp_http_url("ipps://printer:8443/x").unwrap().as_str(),
        "https://printer:8443/x"
    );
}

#[tokio::test]
async fn mcp_tools_return_outputs_that_match_the_golden_schemas() {
    use omni_mcp_kit::{ToolContext, ToolOutput};
    let f = Fixture::new();
    let tools = omni_personal::mcp::printer::tools(Some(Arc::new(f.service()))).unwrap();
    let call = |name: &'static str, input: serde_json::Value| {
        let tool = tools.iter().find(|t| t.meta.name == name).unwrap().clone();
        async move {
            let cx = ToolContext {
                call_id: "1".into(),
                cancel: tokio_util::sync::CancellationToken::new(),
            };
            match tool.handler.call(input, cx).await.unwrap() {
                ToolOutput::Structured(map)
                | ToolOutput::Custom {
                    structured: map, ..
                } => serde_json::Value::Object(map),
            }
        }
    };
    let status = call("get_printer_status", serde_json::json!({})).await;
    assert_eq!(status["ready"], serde_json::json!(true));
    assert_eq!(status["acceptingJobs"], serde_json::Value::Null);
    let job = call(
        "print_document",
        serde_json::json!({"url": "https://example.com/file.pdf", "jobName": "  Packet  "}),
    )
    .await;
    assert_eq!(job["jobId"], serde_json::json!(42));
    assert_eq!(job["jobName"], serde_json::json!("Packet"));
    assert_eq!(job["paper"], serde_json::json!("letter"));
}
