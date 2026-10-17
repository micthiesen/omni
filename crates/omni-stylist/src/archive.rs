//! Portable archives: export, verification, restore, listing, fixture purge.
//!
//! An archive is one JSON document (`omni-stylist-export`, format version 1)
//! holding every record revision, event, evidence row (optionally with its
//! bytes in base64), idempotency receipt and derived relationship of one
//! namespace, the runbooks, and SHA-256 checksums of each section. The
//! `content` checksum excludes the namespace name, timestamps and bytes, so a
//! restore into another namespace re-exports to the same value; restore checks
//! that inside its transaction and rolls back on any mismatch.
//!
//! Archives are written to `<private>/archives/<id>.json` with a small
//! `<id>.manifest.json` beside it, both mode 0600.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::api::{
    ArchiveCounts, ArchiveManifest, ArchiveSource, ArchivesListOutput, ExportOutput, PurgeOutput,
    RestoreOutput,
};
use crate::error::{StylistError, invalid, io, reject};
use crate::evidence::{PrivateDir, sha256_hex};
use crate::ledger::{self, EventRow, EvidenceRow, IdempotencyRow, RecordRow};
use crate::runbook;
use crate::service::{Stylist, digest_json, iso};
use crate::state::LedgerState;
use crate::types::Namespace;

pub const FORMAT: &str = "omni-stylist-export";
pub const FORMAT_VERSION: u64 = 1;
/// The largest archive returned inline by `stylist_export`.
pub const MAX_INLINE_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
/// Scheduled backups kept.
pub const KEEP_BACKUPS: usize = 72;
/// Exports of real kept.
pub const KEEP_REAL_EXPORTS: usize = 20;
/// Fixture archives (exports and pre-purge) kept, across all fixtures.
pub const KEEP_FIXTURE: usize = 50;

const LOG: &str = "Stylist";

/// Rows deleted per table.
type RowsRemoved = Vec<(String, u64)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Export,
    Backup,
    PrePurge,
}

impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Reason::Export => "export",
            Reason::Backup => "backup",
            Reason::PrePurge => "pre_purge",
        }
    }

    fn id_part(self) -> &'static str {
        match self {
            Reason::Export => "export",
            Reason::Backup => "backup",
            Reason::PrePurge => "prepurge",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveRunbook {
    pub version: String,
    pub sha256: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveRecord {
    pub id: String,
    pub revision: u64,
    pub kind: String,
    pub ledger_revision: u64,
    pub recorded_at: i64,
    pub supersedes: Option<String>,
    pub correction_reason: Option<String>,
    pub payload: Value,
    pub derived: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveEvent {
    pub id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub ledger_revision: u64,
    pub occurred_on: String,
    pub recorded_at: i64,
    pub supersedes: Option<String>,
    pub payload: Value,
    pub derived: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveEvidence {
    pub id: String,
    pub sha256: String,
    pub mime_type: String,
    pub size: u64,
    pub ledger_revision: u64,
    pub recorded_at: i64,
    pub info: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveIdempotency {
    pub key: String,
    pub tool: String,
    pub fingerprint: String,
    pub ledger_revision: u64,
    pub recorded_at: i64,
    pub result: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ArchiveRelationship {
    pub from: String,
    pub rel: String,
    pub to: String,
}

/// The archive document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveDoc {
    pub format: String,
    pub format_version: u64,
    pub archive_id: String,
    pub namespace: String,
    pub ledger_revision: u64,
    pub exported_at: String,
    pub reason: String,
    pub includes_bytes: bool,
    pub runbooks: Vec<ArchiveRunbook>,
    pub records: Vec<ArchiveRecord>,
    pub events: Vec<ArchiveEvent>,
    pub evidence: Vec<ArchiveEvidence>,
    pub idempotency: Vec<ArchiveIdempotency>,
    pub relationships: Vec<ArchiveRelationship>,
    pub checksums: BTreeMap<String, String>,
}

fn parse_json(what: &str, text: &str) -> Result<Value, StylistError> {
    serde_json::from_str(text).map_err(|e| StylistError::Corrupt(format!("{what}: {e}")))
}

fn to_value<T: Serialize>(value: &T) -> Result<Value, StylistError> {
    serde_json::to_value(value).map_err(|e| StylistError::Corrupt(e.to_string()))
}

/// The namespace's rows, read in one store job.
pub struct Rows {
    pub revision: u64,
    pub records: Vec<RecordRow>,
    pub events: Vec<EventRow>,
    pub evidence: Vec<EvidenceRow>,
    pub idempotency: Vec<IdempotencyRow>,
}

pub fn read_rows(conn: &Connection, namespace: &Namespace) -> Result<Rows, StylistError> {
    let revision = ledger::namespace_revision(conn, namespace.as_str())?;
    Ok(Rows {
        revision,
        records: ledger::record_rows(conn, namespace.as_str(), revision)?,
        events: ledger::event_rows(conn, namespace.as_str(), revision)?,
        evidence: ledger::evidence_rows(conn, namespace.as_str(), revision)?,
        idempotency: ledger::idempotency_rows(conn, namespace.as_str())?,
    })
}

/// The document sections for `rows` (evidence without bytes).
fn sections(namespace: &Namespace, rows: &Rows) -> Result<ArchiveDoc, StylistError> {
    let state = LedgerState::from_rows(
        namespace.clone(),
        rows.revision,
        rows.revision,
        rows.records.clone(),
        rows.events.clone(),
        rows.evidence.clone(),
    )?;
    let mut relationships = Vec::new();
    for record in state.records.values() {
        for link in state.record_links(record) {
            relationships.push(ArchiveRelationship {
                from: record.id.as_str().to_owned(),
                rel: link.rel,
                to: link.id,
            });
        }
    }
    for event in &state.events {
        for link in state.event_links(event) {
            relationships.push(ArchiveRelationship {
                from: event.id.as_str().to_owned(),
                rel: link.rel,
                to: link.id,
            });
        }
    }
    for evidence in state.evidence.values() {
        for link in LedgerState::evidence_links(evidence) {
            relationships.push(ArchiveRelationship {
                from: evidence.id.as_str().to_owned(),
                rel: link.rel,
                to: link.id,
            });
        }
    }
    relationships.sort();
    let records = rows
        .records
        .iter()
        .map(|r| {
            Ok(ArchiveRecord {
                id: r.id.clone(),
                revision: r.revision,
                kind: r.kind.clone(),
                ledger_revision: r.ledger_revision,
                recorded_at: r.recorded_at,
                supersedes: r.supersedes.clone(),
                correction_reason: r.correction_reason.clone(),
                payload: parse_json(&r.id, &r.payload)?,
                derived: r
                    .derived
                    .as_deref()
                    .map(|d| parse_json(&r.id, d))
                    .transpose()?,
            })
        })
        .collect::<Result<Vec<_>, StylistError>>()?;
    let events = rows
        .events
        .iter()
        .map(|e| {
            Ok(ArchiveEvent {
                id: e.id.clone(),
                event_type: e.event_type.clone(),
                ledger_revision: e.ledger_revision,
                occurred_on: e.occurred_on.clone(),
                recorded_at: e.recorded_at,
                supersedes: e.supersedes.clone(),
                payload: parse_json(&e.id, &e.payload)?,
                derived: e
                    .derived
                    .as_deref()
                    .map(|d| parse_json(&e.id, d))
                    .transpose()?,
            })
        })
        .collect::<Result<Vec<_>, StylistError>>()?;
    let evidence = rows
        .evidence
        .iter()
        .map(|e| {
            Ok(ArchiveEvidence {
                id: e.id.clone(),
                sha256: e.sha256.clone(),
                mime_type: e.mime_type.clone(),
                size: e.size,
                ledger_revision: e.ledger_revision,
                recorded_at: e.recorded_at,
                info: parse_json(&e.id, &e.payload)?,
                data_base64: None,
            })
        })
        .collect::<Result<Vec<_>, StylistError>>()?;
    let idempotency = rows
        .idempotency
        .iter()
        .map(|i| {
            Ok(ArchiveIdempotency {
                key: i.key.clone(),
                tool: i.tool.clone(),
                fingerprint: i.fingerprint.clone(),
                ledger_revision: i.ledger_revision,
                recorded_at: i.recorded_at,
                result: parse_json(&i.key, &i.result)?,
            })
        })
        .collect::<Result<Vec<_>, StylistError>>()?;
    let mut doc = ArchiveDoc {
        format: FORMAT.to_owned(),
        format_version: FORMAT_VERSION,
        archive_id: String::new(),
        namespace: namespace.as_str().to_owned(),
        ledger_revision: rows.revision,
        exported_at: String::new(),
        reason: String::new(),
        includes_bytes: false,
        runbooks: runbook::RUNBOOKS
            .iter()
            .map(|r| ArchiveRunbook {
                version: r.version.to_owned(),
                sha256: r.sha256().to_owned(),
                text: r.text.to_owned(),
            })
            .collect(),
        records,
        events,
        evidence,
        idempotency,
        relationships,
        checksums: BTreeMap::new(),
    };
    doc.checksums = checksums(&doc)?;
    Ok(doc)
}

/// Section checksums and the namespace-independent content checksum.
pub fn checksums(doc: &ArchiveDoc) -> Result<BTreeMap<String, String>, StylistError> {
    let evidence: Vec<ArchiveEvidence> = doc
        .evidence
        .iter()
        .map(|e| ArchiveEvidence {
            data_base64: None,
            ..e.clone()
        })
        .collect();
    let mut sums = BTreeMap::new();
    sums.insert("records".to_owned(), digest_json(&to_value(&doc.records)?));
    sums.insert("events".to_owned(), digest_json(&to_value(&doc.events)?));
    sums.insert("evidence".to_owned(), digest_json(&to_value(&evidence)?));
    sums.insert(
        "idempotency".to_owned(),
        digest_json(&to_value(&doc.idempotency)?),
    );
    sums.insert(
        "relationships".to_owned(),
        digest_json(&to_value(&doc.relationships)?),
    );
    sums.insert(
        "runbooks".to_owned(),
        digest_json(&to_value(&doc.runbooks)?),
    );
    sums.insert(
        "content".to_owned(),
        digest_json(&json!({
            "ledgerRevision": doc.ledger_revision,
            "records": doc.records,
            "events": doc.events,
            "evidence": evidence,
            "idempotency": doc.idempotency,
            "relationships": doc.relationships,
        })),
    );
    Ok(sums)
}

fn counts(doc: &ArchiveDoc) -> ArchiveCounts {
    let n = |len: usize| u64::try_from(len).unwrap_or(u64::MAX);
    let records: HashSet<&str> = doc.records.iter().map(|r| r.id.as_str()).collect();
    ArchiveCounts {
        records: n(records.len()),
        record_revisions: n(doc.records.len()),
        events: n(doc.events.len()),
        evidence: n(doc.evidence.len()),
        evidence_bytes: doc.evidence.iter().map(|e| e.size).sum(),
        idempotency_keys: n(doc.idempotency.len()),
        relationships: n(doc.relationships.len()),
    }
}

/// What verification found.
pub struct Verified {
    pub evidence_verified: u64,
}

/// Checks the format, every section checksum and every evidence digest
/// (inline bytes, or files in `private` when the archive has none).
pub fn verify(doc: &ArchiveDoc, private: Option<&PrivateDir>) -> Result<Verified, StylistError> {
    if doc.format != FORMAT || doc.format_version != FORMAT_VERSION {
        return Err(reject(
            "archive",
            format!(
                "unsupported archive format {} version {}",
                doc.format, doc.format_version
            ),
        ));
    }
    let recomputed = checksums(doc)?;
    for (section, digest) in &recomputed {
        if doc.checksums.get(section) != Some(digest) {
            return Err(reject(
                "integrity",
                format!("archive section {section} does not match its checksum"),
            ));
        }
    }
    Namespace::parse(&doc.namespace).map_err(|e| reject("archive", e))?;
    for runbook in &doc.runbooks {
        if sha256_hex(runbook.text.as_bytes()) != runbook.sha256 {
            return Err(reject(
                "integrity",
                format!("runbook {} does not match its digest", runbook.version),
            ));
        }
    }
    check_continuity(doc)?;
    let mut verified = 0;
    for evidence in &doc.evidence {
        if !crate::evidence::is_digest(&evidence.sha256) {
            return Err(reject(
                "integrity",
                format!("evidence {} has a malformed digest", evidence.id),
            ));
        }
        let bytes = match &evidence.data_base64 {
            Some(data) => STANDARD.decode(data).map_err(|_| {
                reject(
                    "integrity",
                    format!("evidence {} has invalid base64", evidence.id),
                )
            })?,
            None => match private {
                Some(private) => private.get(&evidence.sha256)?,
                None => {
                    return Err(reject(
                        "archive",
                        format!(
                            "evidence {} bytes are not in the archive; provide the evidence directory",
                            evidence.id
                        ),
                    ));
                }
            },
        };
        if sha256_hex(&bytes) != evidence.sha256
            || u64::try_from(bytes.len()).unwrap_or(u64::MAX) != evidence.size
        {
            return Err(reject(
                "integrity",
                format!(
                    "evidence {} bytes do not match its digest or size",
                    evidence.id
                ),
            ));
        }
        verified += 1;
    }
    Ok(Verified {
        evidence_verified: verified,
    })
}

/// Every committed write adds exactly one record, event or evidence row at
/// the next ledger revision, so an intact archive holds revisions 1..=N once
/// each, with well-formed ids, and no receipt beyond N.
fn check_continuity(doc: &ArchiveDoc) -> Result<(), StylistError> {
    let mut revisions: Vec<u64> = Vec::new();
    for (id, revision) in doc
        .records
        .iter()
        .map(|r| (&r.id, r.ledger_revision))
        .chain(doc.events.iter().map(|e| (&e.id, e.ledger_revision)))
        .chain(doc.evidence.iter().map(|e| (&e.id, e.ledger_revision)))
    {
        crate::types::LedgerId::parse(id).map_err(|e| reject("integrity", e))?;
        revisions.push(revision);
    }
    revisions.sort_unstable();
    let expected: Vec<u64> = (1..=doc.ledger_revision).collect();
    if revisions != expected {
        return Err(reject(
            "integrity",
            format!(
                "the archive's rows do not cover ledger revisions 1 to {} exactly once; rows are missing or duplicated",
                doc.ledger_revision
            ),
        ));
    }
    if doc
        .idempotency
        .iter()
        .any(|i| i.ledger_revision > doc.ledger_revision)
    {
        return Err(reject(
            "integrity",
            "an idempotency receipt points beyond the archive's ledger revision",
        ));
    }
    Ok(())
}

fn archive_id(namespace: &Namespace, revision: u64, now_ms: i64, reason: Reason) -> String {
    let stamp = jiff::Timestamp::from_millisecond(now_ms)
        .map(|t| t.strftime("%Y%m%dt%H%M%Sz").to_string())
        .unwrap_or_else(|_| now_ms.to_string());
    let nonce = &uuid::Uuid::new_v4().simple().to_string()[..6];
    format!(
        "{}-r{revision}-{stamp}-{}-{nonce}",
        namespace.as_str(),
        reason.id_part()
    )
}

fn manifest(doc: &ArchiveDoc, file_sha256: String, file_bytes: u64) -> ArchiveManifest {
    let mut sums = Map::new();
    for (k, v) in &doc.checksums {
        sums.insert(k.clone(), json!(v));
    }
    ArchiveManifest {
        archive_id: doc.archive_id.clone(),
        format: doc.format.clone(),
        format_version: doc.format_version,
        namespace: doc.namespace.clone(),
        ledger_revision: doc.ledger_revision,
        exported_at: doc.exported_at.clone(),
        reason: doc.reason.clone(),
        includes_bytes: doc.includes_bytes,
        counts: counts(doc),
        checksums: sums,
        file_sha256,
        file_bytes,
    }
}

/// Reads and parses an archive file.
pub fn read_doc(path: &Path) -> Result<(ArchiveDoc, Vec<u8>), StylistError> {
    let bytes = std::fs::read(path)
        .map_err(|e| reject("archive", format!("cannot read {}: {e}", path.display())))?;
    let doc: ArchiveDoc = serde_json::from_slice(&bytes).map_err(|e| {
        reject(
            "archive",
            format!("{} is not a stylist archive: {e}", path.display()),
        )
    })?;
    Ok((doc, bytes))
}

/// Reads and verifies the archive at `path`, resolving evidence without
/// inline bytes from `private`. Returns the document and the evidence count.
pub fn verify_file(path: &Path, private: &PrivateDir) -> Result<(ArchiveDoc, u64), StylistError> {
    let (doc, raw) = read_doc(path)?;
    // A stored archive's manifest pins the whole file.
    let manifest_path = path
        .to_str()
        .and_then(|p| p.strip_suffix(".json"))
        .map(|stem| PathBuf::from(format!("{stem}.manifest.json")));
    if let Some(manifest_path) = manifest_path
        && let Ok(bytes) = std::fs::read(&manifest_path)
    {
        let manifest: ArchiveManifest = serde_json::from_slice(&bytes)
            .map_err(|e| reject("archive", format!("unreadable archive manifest: {e}")))?;
        if manifest.file_sha256 != sha256_hex(&raw) {
            return Err(reject(
                "integrity",
                "the archive file does not match the SHA-256 in its manifest",
            ));
        }
    }
    let verified = verify(&doc, Some(private))?;
    Ok((doc, verified.evidence_verified))
}

/// A finished export.
pub struct Exported {
    pub output: ExportOutput,
    pub path: PathBuf,
    pub document: Option<Vec<u8>>,
}

impl Stylist {
    fn archive_path(&self, id: &str) -> PathBuf {
        self.private.archives().join(format!("{id}.json"))
    }

    fn manifest_path(&self, id: &str) -> PathBuf {
        self.private.archives().join(format!("{id}.manifest.json"))
    }

    /// Builds the archive document for `namespace` from one consistent read.
    pub async fn build_archive(
        &self,
        namespace: &Namespace,
        include_bytes: bool,
        reason: Reason,
    ) -> Result<ArchiveDoc, StylistError> {
        self.ready().await?;
        let ns = namespace.clone();
        let rows = self
            .store
            .read(move |docs| Ok(read_rows(docs.connection(), &ns)))
            .await??;
        let mut doc = sections(namespace, &rows)?;
        let now = self.clock.now_ms();
        doc.archive_id = archive_id(namespace, rows.revision, now, reason);
        doc.exported_at = iso(now);
        doc.reason = reason.as_str().to_owned();
        doc.includes_bytes = include_bytes;
        if include_bytes {
            let private = self.private.clone();
            let digests: Vec<String> = doc.evidence.iter().map(|e| e.sha256.clone()).collect();
            let blobs = tokio::task::spawn_blocking(move || {
                digests
                    .iter()
                    .map(|d| private.get(d).map(|b| STANDARD.encode(b)))
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(io)??;
            for (evidence, blob) in doc.evidence.iter_mut().zip(blobs) {
                evidence.data_base64 = Some(blob);
            }
        }
        Ok(doc)
    }

    /// Writes `doc` (archive and manifest) and verifies the written file.
    async fn write_archive(
        &self,
        doc: ArchiveDoc,
        path: Option<PathBuf>,
    ) -> Result<Exported, StylistError> {
        let bytes = serde_json::to_vec(&doc).map_err(|e| StylistError::Corrupt(e.to_string()))?;
        let file_sha = sha256_hex(&bytes);
        let file_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let manifest = manifest(&doc, file_sha.clone(), file_bytes);
        let archive_path = path
            .clone()
            .unwrap_or_else(|| self.archive_path(&doc.archive_id));
        let manifest_path = path.is_none().then(|| self.manifest_path(&doc.archive_id));
        let private = self.private.clone();
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| StylistError::Corrupt(e.to_string()))?;
        let check_cas = !doc.includes_bytes;
        let target = archive_path.clone();
        let (verified, bytes) =
            tokio::task::spawn_blocking(move || -> Result<(u64, Vec<u8>), StylistError> {
                private.ensure()?;
                if let Some(parent) = target.parent()
                    && path.is_some()
                {
                    std::fs::create_dir_all(parent).map_err(io)?;
                }
                let checked = (|| {
                    private.write_atomic(&target, &bytes)?;
                    if let Some(manifest_path) = &manifest_path {
                        private.write_atomic(manifest_path, &manifest_bytes)?;
                    }
                    let (reread, raw) = read_doc(&target)?;
                    if sha256_hex(&raw) != file_sha {
                        return Err(reject(
                            "integrity",
                            "the written archive differs from what was exported",
                        ));
                    }
                    verify(&reread, check_cas.then_some(&private)).map(|v| v.evidence_verified)
                })();
                match checked {
                    Ok(verified) => Ok((verified, bytes)),
                    Err(error) => {
                        // An archive that failed verification is not kept.
                        let _ = std::fs::remove_file(&target);
                        if let Some(manifest_path) = &manifest_path {
                            let _ = std::fs::remove_file(manifest_path);
                        }
                        Err(error)
                    }
                }
            })
            .await
            .map_err(io)??;
        tracing::info!(
            target: LOG,
            "Wrote stylist archive {} ({} bytes, {} evidence verified)",
            doc.archive_id,
            file_bytes,
            verified
        );
        Ok(Exported {
            output: ExportOutput {
                manifest,
                verified: true,
                document_attached: false,
            },
            path: archive_path,
            document: Some(bytes),
        })
    }

    /// Exports `namespace` into the private archive directory.
    pub async fn export(
        &self,
        namespace: &Namespace,
        include_bytes: bool,
        reason: Reason,
    ) -> Result<Exported, StylistError> {
        let doc = self.build_archive(namespace, include_bytes, reason).await?;
        let exported = self.write_archive(doc, None).await?;
        if let Err(error) = self.prune_archives().await {
            tracing::warn!(target: LOG, "Pruning stylist archives failed: {}", error.tool_text());
        }
        Ok(exported)
    }

    /// Exports `namespace` to an operator-chosen file.
    pub async fn export_to_path(
        &self,
        namespace: &Namespace,
        include_bytes: bool,
        path: PathBuf,
    ) -> Result<Exported, StylistError> {
        let doc = self
            .build_archive(namespace, include_bytes, Reason::Export)
            .await?;
        self.write_archive(doc, Some(path)).await
    }

    /// Every archive manifest, newest first.
    pub async fn list_archives(
        &self,
        namespace: Option<&Namespace>,
        limit: usize,
    ) -> Result<ArchivesListOutput, StylistError> {
        self.ready().await?;
        let dir = self.private.archives();
        let mut manifests = tokio::task::spawn_blocking(move || read_manifests(&dir))
            .await
            .map_err(io)??;
        if let Some(namespace) = namespace {
            manifests.retain(|m| m.namespace == namespace.as_str());
        }
        let total = u64::try_from(manifests.len()).unwrap_or(u64::MAX);
        manifests.truncate(limit);
        Ok(ArchivesListOutput {
            archives: manifests,
            total,
        })
    }

    /// Keeps the newest [`KEEP_BACKUPS`] backups of real, [`KEEP_REAL_EXPORTS`]
    /// other archives of real, and [`KEEP_FIXTURE`] fixture archives, so test
    /// churn never evicts an archive of the owner's ledger.
    pub async fn prune_archives(&self) -> Result<u64, StylistError> {
        let dir = self.private.archives();
        tokio::task::spawn_blocking(move || -> Result<u64, StylistError> {
            let manifests = read_manifests(&dir)?;
            let mut removed = 0;
            let (real, fixtures): (Vec<_>, Vec<_>) =
                manifests.into_iter().partition(|m| m.namespace == "real");
            let (backups, exports): (Vec<_>, Vec<_>) =
                real.into_iter().partition(|m| m.reason == "backup");
            for stale in backups
                .into_iter()
                .skip(KEEP_BACKUPS)
                .chain(exports.into_iter().skip(KEEP_REAL_EXPORTS))
                .chain(fixtures.into_iter().skip(KEEP_FIXTURE))
            {
                for suffix in [".json", ".manifest.json"] {
                    let path = dir.join(format!("{}{suffix}", stale.archive_id));
                    if path.exists() {
                        std::fs::remove_file(&path).map_err(io)?;
                    }
                }
                removed += 1;
            }
            Ok(removed)
        })
        .await
        .map_err(io)?
    }

    fn source_path(&self, source: &ArchiveSource) -> PathBuf {
        match source {
            ArchiveSource::Id(id) => self.archive_path(id.as_str()),
            ArchiveSource::Path(path) => path.clone(),
        }
    }

    /// Verifies an archive without restoring it.
    pub async fn verify_archive(
        &self,
        source: ArchiveSource,
    ) -> Result<(ArchiveDoc, u64), StylistError> {
        let path = self.source_path(&source);
        let private = self.private.clone();
        tokio::task::spawn_blocking(move || verify_file(&path, &private))
            .await
            .map_err(io)?
    }

    /// Restores an archive into an empty namespace. Fixture namespaces only,
    /// unless `allow_real` (the operator command) is set.
    pub async fn restore(
        &self,
        source: ArchiveSource,
        target: &Namespace,
        allow_real: bool,
    ) -> Result<RestoreOutput, StylistError> {
        if !target.is_fixture() && !allow_real {
            return Err(reject(
                "namespace",
                "restoring into real is an operator action (omni stylist-restore --allow-real); restore into a fixture namespace",
            ));
        }
        self.ready().await?;
        let path = self.source_path(&source);
        let private = self.private.clone();
        let _guard = self.evidence_lock.lock().await;
        let target_is_fixture = target.is_fixture();
        let (doc, evidence_verified, created) = tokio::task::spawn_blocking(
            move || -> Result<(ArchiveDoc, u64, Vec<String>), StylistError> {
                let (doc, verified) = verify_file(&path, &private)?;
                // Real and fixture data never cross: a fixture archive cannot
                // become the owner's ledger, nor real data a fixture.
                if doc.namespace.starts_with("fixture-") != target_is_fixture {
                    return Err(reject(
                        "namespace",
                        format!(
                            "archive {} holds namespace {}; restore it only into {}",
                            doc.archive_id,
                            doc.namespace,
                            if target_is_fixture {
                                "real"
                            } else {
                                "a fixture namespace"
                            }
                        ),
                    ));
                }
                let mut created = Vec::new();
                for evidence in &doc.evidence {
                    if let Some(data) = &evidence.data_base64 {
                        let bytes = STANDARD.decode(data).map_err(|_| {
                            reject(
                                "integrity",
                                format!("evidence {} has invalid base64", evidence.id),
                            )
                        })?;
                        let stored = private.put(&bytes)?;
                        if stored.created {
                            created.push(stored.digest);
                        }
                    }
                }
                Ok((doc, verified, created))
            },
        )
        .await
        .map_err(io)??;
        let target_ns = target.clone();
        let now = self.clock.now_ms();
        let expected = doc
            .checksums
            .get("content")
            .cloned()
            .ok_or_else(|| reject("archive", "the archive has no content checksum"))?;
        let doc_for_write = doc.clone();
        let written = self
            .store
            .write(move |tx| -> Result<String, StylistError> {
                let conn = tx.connection();
                let ns = target_ns.as_str();
                if !ledger::namespace_is_empty(conn, ns)? {
                    return Err(reject(
                        "namespace",
                        format!("namespace {ns} is not empty; restore only into an empty namespace"),
                    ));
                }
                insert_doc(conn, ns, &doc_for_write)?;
                ledger::set_revision(conn, ns, doc_for_write.ledger_revision, now)?;
                let rows = read_rows(conn, &target_ns)?;
                let restored = sections(&target_ns, &rows)?;
                let content = restored.checksums.get("content").cloned().unwrap_or_default();
                if content != expected {
                    return Err(reject(
                        "integrity",
                        "the restored namespace does not reproduce the archive's content checksum; nothing was restored",
                    ));
                }
                Ok(content)
            })
            .await;
        let content = match written {
            Ok(content) => content,
            Err(error) => {
                self.remove_created(created).await;
                return Err(error);
            }
        };
        tracing::info!(
            target: LOG,
            "Restored stylist archive {} into {target} ({} records, {} events, {} evidence)",
            doc.archive_id,
            doc.records.len(),
            doc.events.len(),
            doc.evidence.len()
        );
        Ok(RestoreOutput {
            namespace: target.as_str().to_owned(),
            archive_id: doc.archive_id.clone(),
            source_namespace: doc.namespace.clone(),
            ledger_revision: doc.ledger_revision,
            counts: counts(&doc),
            evidence_verified,
            verified: true,
            content_sha256: content,
        })
    }

    /// Archives (with bytes) and deletes a fixture namespace, then removes
    /// evidence files no namespace references.
    pub async fn purge_fixture(
        &self,
        namespace: &Namespace,
        confirm: &str,
    ) -> Result<PurgeOutput, StylistError> {
        if !namespace.is_fixture() {
            return Err(reject("namespace", "only fixture namespaces can be purged"));
        }
        if confirm != namespace.as_str() {
            return Err(invalid("confirmNamespace must repeat namespace exactly"));
        }
        let doc = self
            .build_archive(namespace, true, Reason::PrePurge)
            .await?;
        let archived_revision = doc.ledger_revision;
        let candidates: HashSet<String> = doc.evidence.iter().map(|e| e.sha256.clone()).collect();
        let exported = self.write_archive(doc, None).await?;
        if let Err(error) = self.prune_archives().await {
            tracing::warn!(target: LOG, "Pruning stylist archives failed: {}", error.tool_text());
        }
        let _guard = self.evidence_lock.lock().await;
        let ns = namespace.clone();
        let (removed, referenced) = self
            .store
            .write(
                move |tx| -> Result<(RowsRemoved, Vec<String>), StylistError> {
                    let conn = tx.connection();
                    let current = ledger::namespace_revision(conn, ns.as_str())?;
                    if current != archived_revision {
                        return Err(reject(
                            "revision_conflict",
                            format!(
                                "namespace {ns} changed (revision {current}) after its pre-purge archive (revision {archived_revision}); purge again"
                            ),
                        ));
                    }
                    let removed = ledger::delete_namespace(conn, ns.as_str())?;
                    Ok((removed, ledger::all_evidence_hashes(conn)?))
                },
            )
            .await?;
        // Only this namespace's own files are candidates, kept when any other
        // namespace or a bytes-less archive (a backup) still names them.
        let private = self.private.clone();
        let mut keep: HashSet<String> = referenced.into_iter().collect();
        let dir = self.private.archives();
        let files = tokio::task::spawn_blocking(move || -> Result<u64, StylistError> {
            keep.extend(digests_in_bytesless_archives(&dir)?);
            private.remove_stale_parts();
            private.remove_digests(&candidates, &keep)
        })
        .await
        .map_err(io)??;
        let mut rows = Map::new();
        for (table, count) in removed {
            rows.insert(table, json!(count));
        }
        tracing::info!(target: LOG, "Purged fixture namespace {namespace}; archive {}", exported.output.manifest.archive_id);
        Ok(PurgeOutput {
            namespace: namespace.as_str().to_owned(),
            archive: exported.output.manifest,
            rows_removed: rows,
            evidence_files_removed: files,
        })
    }
}

/// Evidence digests named by stored archives that carry no bytes (backups):
/// their files are the only copy of those bytes.
fn digests_in_bytesless_archives(dir: &Path) -> Result<HashSet<String>, StylistError> {
    let mut digests = HashSet::new();
    for manifest in read_manifests(dir)?
        .into_iter()
        .filter(|m| !m.includes_bytes)
    {
        let path = dir.join(format!("{}.json", manifest.archive_id));
        match read_doc(&path) {
            Ok((doc, _)) => digests.extend(doc.evidence.into_iter().map(|e| e.sha256)),
            Err(error) => {
                return Err(reject(
                    "archive",
                    format!(
                        "cannot read backup {} to protect its evidence: {}",
                        manifest.archive_id,
                        error.tool_text()
                    ),
                ));
            }
        }
    }
    Ok(digests)
}

fn read_manifests(dir: &Path) -> Result<Vec<ArchiveManifest>, StylistError> {
    let mut manifests = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(manifests);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".manifest.json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        match serde_json::from_slice::<ArchiveManifest>(&bytes) {
            Ok(manifest) => manifests.push(manifest),
            Err(e) => {
                tracing::warn!(target: LOG, "Skipping unreadable archive manifest {name}: {e}")
            }
        }
    }
    manifests.sort_by(|a, b| {
        (b.exported_at.as_str(), b.archive_id.as_str())
            .cmp(&(a.exported_at.as_str(), a.archive_id.as_str()))
    });
    Ok(manifests)
}

/// Inserts every row of `doc` into `namespace`.
fn insert_doc(conn: &Connection, namespace: &str, doc: &ArchiveDoc) -> Result<(), StylistError> {
    let text = |value: &Value| {
        serde_json::to_string(value).map_err(|e| StylistError::Corrupt(e.to_string()))
    };
    for r in &doc.records {
        ledger::insert_record(
            conn,
            namespace,
            &RecordRow {
                id: r.id.clone(),
                revision: r.revision,
                kind: r.kind.clone(),
                ledger_revision: r.ledger_revision,
                recorded_at: r.recorded_at,
                supersedes: r.supersedes.clone(),
                correction_reason: r.correction_reason.clone(),
                payload: text(&r.payload)?,
                derived: r.derived.as_ref().map(text).transpose()?,
            },
        )?;
    }
    for e in &doc.events {
        ledger::insert_event(
            conn,
            namespace,
            &EventRow {
                id: e.id.clone(),
                event_type: e.event_type.clone(),
                ledger_revision: e.ledger_revision,
                occurred_on: e.occurred_on.clone(),
                recorded_at: e.recorded_at,
                supersedes: e.supersedes.clone(),
                payload: text(&e.payload)?,
                derived: e.derived.as_ref().map(text).transpose()?,
            },
        )?;
    }
    for e in &doc.evidence {
        ledger::insert_evidence(
            conn,
            namespace,
            &EvidenceRow {
                id: e.id.clone(),
                sha256: e.sha256.clone(),
                mime_type: e.mime_type.clone(),
                size: e.size,
                ledger_revision: e.ledger_revision,
                recorded_at: e.recorded_at,
                payload: text(&e.info)?,
            },
        )?;
    }
    for i in &doc.idempotency {
        ledger::insert_idempotency(
            conn,
            namespace,
            &IdempotencyRow {
                key: i.key.clone(),
                tool: i.tool.clone(),
                fingerprint: i.fingerprint.clone(),
                ledger_revision: i.ledger_revision,
                recorded_at: i.recorded_at,
                result: text(&i.result)?,
            },
        )?;
    }
    Ok(())
}
