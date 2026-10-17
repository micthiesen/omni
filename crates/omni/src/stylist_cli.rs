//! Operator commands for the stylist ledger (docs/stylist.md, "Backup and restore"):
//!
//! ```text
//! omni stylist-export --namespace NS --out FILE [--no-bytes] [--db PATH] [--private DIR]
//! omni stylist-verify --archive FILE [--private DIR]
//! omni stylist-restore --archive FILE --namespace NS [--allow-real] [--db PATH] [--private DIR]
//! ```
//!
//! `--db` defaults to `DB_NAME` and `--private` to the app's stylist directory
//! (`/data/stylist-private` in the container). A restore needs an empty target
//! namespace and commits only when the restored ledger reproduces the
//! archive's content checksum; `real` additionally needs `--allow-real`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use omni_core::clock::{SharedClock, SystemClock};
use omni_store::{Store, StoreOptions};
use omni_stylist::Stylist;
use omni_stylist::api::ArchiveSource;
use omni_stylist::types::Namespace;

use crate::cli::CliError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Export {
        namespace: String,
        out: PathBuf,
        include_bytes: bool,
    },
    Verify {
        archive: PathBuf,
    },
    Restore {
        archive: PathBuf,
        namespace: String,
        allow_real: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StylistArgs {
    pub action: Action,
    pub db: Option<PathBuf>,
    pub private: Option<PathBuf>,
}

/// Parses the arguments after `stylist-export`, `stylist-verify` or `stylist-restore`.
pub fn parse(command: &str, args: &[String]) -> Result<StylistArgs, CliError> {
    let mut namespace = None;
    let mut out = None;
    let mut archive = None;
    let mut db = None;
    let mut private = None;
    let mut include_bytes = true;
    let mut allow_real = false;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) => (f, Some(v.to_owned())),
            None => (arg.as_str(), None),
        };
        let mut value = |name: &'static str| {
            inline
                .clone()
                .or_else(|| rest.next().cloned())
                .ok_or(CliError::MissingValue(name))
        };
        match flag {
            "--namespace" => namespace = Some(value("--namespace")?),
            "--out" => out = Some(PathBuf::from(value("--out")?)),
            "--archive" => archive = Some(PathBuf::from(value("--archive")?)),
            "--db" => db = Some(PathBuf::from(value("--db")?)),
            "--private" => private = Some(PathBuf::from(value("--private")?)),
            "--no-bytes" if command == "stylist-export" => include_bytes = false,
            "--allow-real" if command == "stylist-restore" => allow_real = true,
            other => return Err(CliError::Unknown(other.to_owned())),
        }
    }
    let action = match command {
        "stylist-export" => Action::Export {
            namespace: namespace.ok_or(CliError::MissingValue("--namespace"))?,
            out: out.ok_or(CliError::MissingValue("--out"))?,
            include_bytes,
        },
        "stylist-verify" => Action::Verify {
            archive: archive.ok_or(CliError::MissingValue("--archive"))?,
        },
        _ => Action::Restore {
            archive: archive.ok_or(CliError::MissingValue("--archive"))?,
            namespace: namespace.ok_or(CliError::MissingValue("--namespace"))?,
            allow_real,
        },
    };
    Ok(StylistArgs {
        action,
        db,
        private,
    })
}

fn namespace(value: &str) -> anyhow::Result<Namespace> {
    Namespace::parse(value).map_err(anyhow::Error::msg)
}

/// The default database and private directory from the configuration.
fn defaults(args: &StylistArgs) -> anyhow::Result<(PathBuf, PathBuf, jiff::tz::TimeZone)> {
    let needs_db = !matches!(args.action, Action::Verify { .. });
    let needs_config = (needs_db && args.db.is_none()) || args.private.is_none();
    let config = if needs_config {
        Some(omni_config::Config::from_process_env().map_err(|e| anyhow::anyhow!("{e}"))?)
    } else {
        None
    };
    let tz = config
        .as_ref()
        .and_then(|c| jiff::tz::TimeZone::get(&c.tz).ok())
        .unwrap_or(jiff::tz::TimeZone::UTC);
    let db = match (&args.db, &config) {
        (Some(db), _) => db.clone(),
        (None, Some(config)) => config.db_path(),
        (None, None) if !needs_db => PathBuf::new(),
        (None, None) => anyhow::bail!("--db is required"),
    };
    let private = match (&args.private, &config) {
        (Some(private), _) => private.clone(),
        (None, Some(config)) => {
            crate::context::app_paths(config, PathBuf::from(crate::context::DEFAULT_WEB_DIST))
                .stylist_private
        }
        (None, None) => anyhow::bail!("--private is required"),
    };
    Ok((db, private, tz))
}

async fn open(db: &Path, private: PathBuf, tz: jiff::tz::TimeZone) -> anyhow::Result<Stylist> {
    let clock: SharedClock = Arc::new(SystemClock);
    let store = Store::open(db, StoreOptions::new(clock.clone())).await?;
    Ok(Stylist::new(store, clock, tz, private))
}

/// Runs the command and returns the JSON report for stdout.
pub async fn run(args: &StylistArgs) -> anyhow::Result<String> {
    let (db, private, tz) = defaults(args)?;
    let report = match &args.action {
        Action::Export {
            namespace: ns,
            out,
            include_bytes,
        } => {
            let stylist = open(&db, private, tz).await?;
            let exported = stylist
                .export_to_path(&namespace(ns)?, *include_bytes, out.clone())
                .await
                .map_err(|e| anyhow::anyhow!(e.tool_text()))?;
            serde_json::json!({"archive": exported.path, "manifest": exported.output.manifest})
        }
        Action::Verify { archive } => {
            // Verification reads only the archive and the evidence files.
            let private = omni_stylist::evidence::PrivateDir::new(private);
            let (doc, evidence) = omni_stylist::archive::verify_file(archive, &private)
                .map_err(|e| anyhow::anyhow!(e.tool_text()))?;
            serde_json::json!({
                "verified": true,
                "archiveId": doc.archive_id,
                "namespace": doc.namespace,
                "ledgerRevision": doc.ledger_revision,
                "includesBytes": doc.includes_bytes,
                "evidenceVerified": evidence,
                "checksums": doc.checksums,
            })
        }
        Action::Restore {
            archive,
            namespace: ns,
            allow_real,
        } => {
            let stylist = open(&db, private, tz).await?;
            let restored = stylist
                .restore(
                    ArchiveSource::Path(archive.clone()),
                    &namespace(ns)?,
                    *allow_real,
                )
                .await
                .map_err(|e| anyhow::anyhow!(e.tool_text()))?;
            serde_json::to_value(restored)?
        }
    };
    Ok(format!("{}\n", serde_json::to_string_pretty(&report)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_each_command() {
        assert_eq!(
            parse(
                "stylist-export",
                &args(&["--namespace", "real", "--out=/tmp/a.json", "--no-bytes"])
            ),
            Ok(StylistArgs {
                action: Action::Export {
                    namespace: "real".to_owned(),
                    out: PathBuf::from("/tmp/a.json"),
                    include_bytes: false,
                },
                db: None,
                private: None,
            })
        );
        assert_eq!(
            parse(
                "stylist-restore",
                &args(&[
                    "--archive",
                    "a.json",
                    "--namespace",
                    "real",
                    "--allow-real",
                    "--db",
                    "x.db"
                ])
            ),
            Ok(StylistArgs {
                action: Action::Restore {
                    archive: PathBuf::from("a.json"),
                    namespace: "real".to_owned(),
                    allow_real: true,
                },
                db: Some(PathBuf::from("x.db")),
                private: None,
            })
        );
        assert!(parse("stylist-verify", &args(&[])).is_err());
        assert!(
            parse(
                "stylist-export",
                &args(&["--namespace", "real", "--out", "a", "--allow-real"])
            )
            .is_err()
        );
    }
}
