//! Operator commands for the recipe library (docs/recipes.md, "Backup and
//! restore"):
//!
//! ```text
//! omni recipes-verify <archive> [--dir DIR]
//! omni recipes-restore <archive> [--db PATH] [--dir DIR]
//! ```
//!
//! `--db` defaults to `DB_NAME` and `--dir` to the app's recipes directory
//! (`/data/recipes` in the container). Verification checks every table's
//! checksum and, when the directory exists, that every image the archive
//! refers to is stored there intact. A restore needs an empty library and
//! commits only when the restored tables read back exactly as archived.

use std::path::PathBuf;
use std::sync::Arc;

use omni_core::clock::{SharedClock, SystemClock};
use omni_recipes::Recipes;
use omni_store::{Store, StoreOptions};

use crate::cli::CliError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Verify,
    Restore,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecipesArgs {
    pub action: Action,
    pub archive: PathBuf,
    pub db: Option<PathBuf>,
    pub dir: Option<PathBuf>,
}

/// Parses the arguments after `recipes-verify` or `recipes-restore`.
pub fn parse(command: &str, args: &[String]) -> Result<RecipesArgs, CliError> {
    let mut archive = None;
    let mut db = None;
    let mut dir = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_owned())),
            _ => (arg.as_str(), None),
        };
        let mut value = |name: &'static str| {
            inline
                .clone()
                .or_else(|| rest.next().cloned())
                .ok_or(CliError::MissingValue(name))
        };
        match flag {
            "--archive" => archive = Some(PathBuf::from(value("--archive")?)),
            "--db" if command == "recipes-restore" => db = Some(PathBuf::from(value("--db")?)),
            "--dir" => dir = Some(PathBuf::from(value("--dir")?)),
            other if other.starts_with("--") => return Err(CliError::Unknown(other.to_owned())),
            path if archive.is_none() => archive = Some(PathBuf::from(path)),
            other => return Err(CliError::Unknown(other.to_owned())),
        }
    }
    Ok(RecipesArgs {
        action: if command == "recipes-verify" {
            Action::Verify
        } else {
            Action::Restore
        },
        archive: archive.ok_or(CliError::MissingValue("<archive>"))?,
        db,
        dir,
    })
}

/// Runs the command and returns the JSON report for stdout.
pub async fn run(args: &RecipesArgs) -> anyhow::Result<String> {
    let needs_config = args.dir.is_none() || (args.action == Action::Restore && args.db.is_none());
    let config = if needs_config {
        Some(
            crate::settings::effective_config()
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?,
        )
    } else {
        None
    };
    let dir = match (&args.dir, &config) {
        (Some(dir), _) => dir.clone(),
        (None, Some(config)) => {
            crate::context::app_paths(config, PathBuf::from(crate::context::DEFAULT_WEB_DIST))
                .recipes_dir
        }
        (None, None) => anyhow::bail!("--dir is required"),
    };
    let report = match args.action {
        Action::Verify => {
            // Images are checked when the directory is there to check.
            let images = dir.join("images").is_dir().then_some(dir.as_path());
            let verified = omni_recipes::archive::verify_file(&args.archive, images)
                .map_err(|e| anyhow::anyhow!(e.tool_text()))?;
            serde_json::json!({ "verified": true, "archive": verified })
        }
        Action::Restore => {
            let db = match (&args.db, &config) {
                (Some(db), _) => db.clone(),
                (None, Some(config)) => config.db_path(),
                (None, None) => anyhow::bail!("--db is required"),
            };
            let tz = config
                .as_ref()
                .and_then(|c| jiff::tz::TimeZone::get(&c.tz).ok())
                .unwrap_or(jiff::tz::TimeZone::UTC);
            let clock: SharedClock = Arc::new(SystemClock);
            let store = Store::open(&db, StoreOptions::new(clock.clone())).await?;
            let recipes = Recipes::new(store, clock, tz, dir);
            let restored = recipes
                .restore(args.archive.clone())
                .await
                .map_err(|e| anyhow::anyhow!(e.tool_text()))?;
            serde_json::json!({ "restored": true, "archive": restored })
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
                "recipes-verify",
                &args(&["a.json.gz", "--dir=/data/recipes"])
            ),
            Ok(RecipesArgs {
                action: Action::Verify,
                archive: PathBuf::from("a.json.gz"),
                db: None,
                dir: Some(PathBuf::from("/data/recipes")),
            })
        );
        assert_eq!(
            parse(
                "recipes-restore",
                &args(&["--archive", "a.json.gz", "--db", "x.db"])
            ),
            Ok(RecipesArgs {
                action: Action::Restore,
                archive: PathBuf::from("a.json.gz"),
                db: Some(PathBuf::from("x.db")),
                dir: None,
            })
        );
        assert!(parse("recipes-verify", &args(&[])).is_err());
        assert!(parse("recipes-verify", &args(&["a", "--db", "x.db"])).is_err());
        assert!(parse("recipes-restore", &args(&["a", "b"])).is_err());
    }
}
