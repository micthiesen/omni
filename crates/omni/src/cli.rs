//! Command-line parsing (no flag library: the surface is small and fixed).
//!
//! ```text
//! omni [--server-only] [--side-effects=live|record] [--web-dist DIR]
//! omni --run-task <Name>
//! omni --preview [--port N]
//! omni healthcheck
//! omni doctor [--image]
//! omni compat-audit --db COPY
//! omni stylist-export|stylist-verify|stylist-restore ... (see stylist_cli)
//! ```
//!
//! `OMNI_SIDE_EFFECTS=record` selects record mode for shadow runs like the
//! flag does; `OMNI_WEB_DIST` sets the frontend directory (default `/app/web`).

use std::path::PathBuf;

use omni_http::SideEffectMode;

use crate::compat_audit::AuditArgs;
use crate::context::DEFAULT_WEB_DIST;

/// What the process does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Serve(Serve),
    /// `--run-task` without a name.
    RunTaskUsage,
    RunTask {
        name: String,
        side_effects: SideEffectMode,
    },
    Preview {
        port: Option<u16>,
        web_dist: PathBuf,
    },
    Healthcheck,
    Doctor {
        image: bool,
    },
    CompatAudit(AuditArgs),
    Stylist(crate::stylist_cli::StylistArgs),
    Help,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Serve {
    pub server_only: bool,
    pub side_effects: SideEffectMode,
    pub web_dist: PathBuf,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum CliError {
    #[error("unknown argument {0:?}")]
    Unknown(String),
    #[error("{0} needs a value")]
    MissingValue(&'static str),
    #[error("invalid --side-effects value {0:?} (live or record)")]
    SideEffects(String),
    #[error("invalid --port value {0:?}")]
    Port(String),
}

pub const USAGE: &str = "usage:
  omni [--server-only] [--side-effects=live|record] [--web-dist DIR]
  omni --run-task <TaskName> [--side-effects=live|record]
  omni --preview [--port N] [--web-dist DIR]
  omni healthcheck
  omni doctor [--image]
  omni compat-audit --db COPY
  omni stylist-export --namespace NS --out FILE [--no-bytes] [--db PATH] [--private DIR]
  omni stylist-verify --archive FILE [--private DIR]
  omni stylist-restore --archive FILE --namespace NS [--allow-real] [--db PATH] [--private DIR]
";

fn side_effects(value: &str) -> Result<SideEffectMode, CliError> {
    match value {
        "live" => Ok(SideEffectMode::Live),
        "record" => Ok(SideEffectMode::Record),
        other => Err(CliError::SideEffects(other.to_owned())),
    }
}

/// Splits `--flag=value` / `--flag value`.
fn value<'a>(
    flag: &'static str,
    inline: Option<&'a str>,
    rest: &mut impl Iterator<Item = &'a String>,
) -> Result<&'a str, CliError> {
    match inline {
        Some(v) => Ok(v),
        None => rest
            .next()
            .map(String::as_str)
            .ok_or(CliError::MissingValue(flag)),
    }
}

/// Parses `args` (without the program name) and the relevant environment.
pub fn parse(args: &[String], env: impl Fn(&str) -> Option<String>) -> Result<Command, CliError> {
    let mut side = match env("OMNI_SIDE_EFFECTS").as_deref() {
        None | Some("") => SideEffectMode::Live,
        Some(v) => side_effects(v)?,
    };
    let mut web_dist = env("OMNI_WEB_DIST")
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_WEB_DIST), PathBuf::from);
    match args.first().map(String::as_str) {
        Some("healthcheck") => return Ok(Command::Healthcheck),
        Some("doctor") => {
            return match args.get(1).map(String::as_str) {
                None => Ok(Command::Doctor { image: false }),
                Some("--image") => Ok(Command::Doctor { image: true }),
                Some(other) => Err(CliError::Unknown(other.to_owned())),
            };
        }
        Some("compat-audit") => {
            let mut db = None;
            let mut rest = args[1..].iter();
            while let Some(arg) = rest.next() {
                let (flag, inline) = match arg.split_once('=') {
                    Some((f, v)) => (f, Some(v)),
                    None => (arg.as_str(), None),
                };
                match flag {
                    "--db" => db = Some(PathBuf::from(value("--db", inline, &mut rest)?)),
                    other => return Err(CliError::Unknown(other.to_owned())),
                }
            }
            let db = db.ok_or(CliError::MissingValue("--db"))?;
            return Ok(Command::CompatAudit(AuditArgs { db }));
        }
        Some(command @ ("stylist-export" | "stylist-verify" | "stylist-restore")) => {
            return crate::stylist_cli::parse(command, &args[1..]).map(Command::Stylist);
        }
        _ => {}
    }
    let mut server_only = false;
    let mut run_task: Option<Option<String>> = None;
    let mut preview = false;
    let mut port = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v)),
            _ => (arg.as_str(), None),
        };
        match flag {
            "--server-only" => server_only = true,
            "--preview" => preview = true,
            "--help" | "-h" => return Ok(Command::Help),
            "--side-effects" => side = side_effects(value("--side-effects", inline, &mut rest)?)?,
            "--web-dist" => web_dist = PathBuf::from(value("--web-dist", inline, &mut rest)?),
            "--port" => {
                let v = value("--port", inline, &mut rest)?;
                port = Some(v.parse::<u16>().map_err(|_| CliError::Port(v.to_owned()))?);
            }
            "--run-task" => {
                run_task = Some(match inline {
                    Some(v) => Some(v.to_owned()),
                    None => rest.next().cloned(),
                });
            }
            other => return Err(CliError::Unknown(other.to_owned())),
        }
    }
    if preview {
        return Ok(Command::Preview { port, web_dist });
    }
    match run_task {
        Some(Some(name)) if !name.is_empty() => Ok(Command::RunTask {
            name,
            side_effects: side,
        }),
        Some(_) => Ok(Command::RunTaskUsage),
        None => Ok(Command::Serve(Serve {
            server_only,
            side_effects: side,
            web_dist,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn serves_by_default_with_live_side_effects() {
        assert_eq!(
            parse(&[], no_env),
            Ok(Command::Serve(Serve {
                server_only: false,
                side_effects: SideEffectMode::Live,
                web_dist: PathBuf::from("/app/web"),
            }))
        );
    }

    #[test]
    fn record_mode_from_flag_or_env() {
        let Ok(Command::Serve(serve)) =
            parse(&args(&["--server-only", "--side-effects=record"]), no_env)
        else {
            panic!("expected serve");
        };
        assert!(serve.server_only);
        assert_eq!(serve.side_effects, SideEffectMode::Record);
        let env = |k: &str| (k == "OMNI_SIDE_EFFECTS").then(|| "record".to_owned());
        let Ok(Command::Serve(serve)) = parse(&[], env) else {
            panic!("expected serve");
        };
        assert_eq!(serve.side_effects, SideEffectMode::Record);
        assert!(parse(&args(&["--side-effects", "maybe"]), no_env).is_err());
    }

    #[test]
    fn run_task_needs_a_name() {
        assert_eq!(
            parse(&args(&["--run-task"]), no_env),
            Ok(Command::RunTaskUsage)
        );
        assert_eq!(
            parse(&args(&["--run-task", "livechecktask"]), no_env),
            Ok(Command::RunTask {
                name: "livechecktask".to_owned(),
                side_effects: SideEffectMode::Live
            })
        );
    }

    #[test]
    fn subcommands() {
        assert_eq!(
            parse(&args(&["healthcheck"]), no_env),
            Ok(Command::Healthcheck)
        );
        assert_eq!(
            parse(&args(&["doctor", "--image"]), no_env),
            Ok(Command::Doctor { image: true })
        );
        assert_eq!(
            parse(&args(&["compat-audit", "--db=a.db"]), no_env),
            Ok(Command::CompatAudit(AuditArgs {
                db: PathBuf::from("a.db"),
            }))
        );
        assert!(
            parse(
                &args(&["compat-audit", "--db", "a.db", "--rewrite-to=b.db"]),
                no_env
            )
            .is_err()
        );
        assert!(parse(&args(&["compat-audit"]), no_env).is_err());
        assert!(parse(&args(&["--bogus"]), no_env).is_err());
        assert_eq!(
            parse(&args(&["--preview", "--port", "3999"]), no_env),
            Ok(Command::Preview {
                port: Some(3999),
                web_dist: PathBuf::from("/app/web")
            })
        );
    }
}
