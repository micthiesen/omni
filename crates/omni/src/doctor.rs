//! `omni doctor [--image]`: runtime invariants of the image
//! (see docs/architecture.md, "Deployment pipeline"). The Docker build runs `doctor --image`, which
//! fails the build when any check fails; without `--image` the checks are
//! the same but reported for a local install.
//!
//! Doctor reads only the variables it checks ([`DoctorEnv`]), never the full
//! production config, so the image gate needs no runtime secrets.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// `LIVESTREAM_MODEL_DIR`'s default, as `omni-config`.
pub const DEFAULT_MODEL_DIR: &str = "/app/assets/livestream-intelligence/models";

/// The environment doctor depends on, with the production config's defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctorEnv {
    /// `FFMPEG_PATH || "ffmpeg"`.
    pub ffmpeg: String,
    /// `YT_DLP_PATH || "yt-dlp"`.
    pub yt_dlp: String,
    /// `LIVESTREAM_MODEL_DIR`, defaulted only when absent.
    pub model_dir: PathBuf,
}

impl DoctorEnv {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let binary = |key: &str, default: &str| {
            get(key)
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_owned())
        };
        Self {
            ffmpeg: binary("FFMPEG_PATH", "ffmpeg"),
            yt_dlp: binary("YT_DLP_PATH", "yt-dlp"),
            model_dir: PathBuf::from(
                get("LIVESTREAM_MODEL_DIR").unwrap_or_else(|| DEFAULT_MODEL_DIR.to_owned()),
            ),
        }
    }

    pub fn from_process_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }
}

/// One check's outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

impl Check {
    fn new(name: impl Into<String>, ok: bool, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok,
            detail: detail.into(),
        }
    }
}

const OUTPUT_CAP: usize = 4 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

async fn command_output(program: &str, args: &[&str]) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    let output = omni_core::process::run_bounded(cmd, None, OUTPUT_CAP, OUTPUT_CAP, TIMEOUT)
        .await
        .map_err(|e| e.to_string())?;
    if output.status != 0 {
        return Err(format!("exit status {}", output.status));
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(text)
}

/// `true` when `listing` (ffmpeg `-filters` / `-encoders`) names `name` as a column.
pub fn lists(listing: &str, name: &str) -> bool {
    listing
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(name))
}

/// Present and structurally sound, so sherpa-onnx cannot abort on it.
fn model_check(path: &Path) -> Check {
    match omni_live_intel::model_check::check_model_file(path) {
        Ok(()) => Check::new(path.display().to_string(), true, "valid"),
        Err(detail) => Check::new(path.display().to_string(), false, detail),
    }
}

fn file_check(path: &Path) -> Check {
    Check::new(
        path.display().to_string(),
        path.is_file(),
        if path.is_file() { "present" } else { "missing" },
    )
}

/// Runs every check.
pub async fn checks(env: &DoctorEnv, assets_dir: &Path) -> Vec<Check> {
    let mut out = Vec::new();
    let ffmpeg = env.ffmpeg.as_str();
    match command_output(ffmpeg, &["-hide_banner", "-filters"]).await {
        Ok(listing) => {
            for filter in ["arnndn", "firequalizer"] {
                out.push(Check::new(
                    format!("ffmpeg filter {filter}"),
                    lists(&listing, filter),
                    ffmpeg,
                ));
            }
        }
        Err(error) => out.push(Check::new("ffmpeg -filters", false, error)),
    }
    match command_output(ffmpeg, &["-hide_banner", "-encoders"]).await {
        Ok(listing) => out.push(Check::new(
            "ffmpeg encoder libmp3lame",
            lists(&listing, "libmp3lame"),
            ffmpeg,
        )),
        Err(error) => out.push(Check::new("ffmpeg -encoders", false, error)),
    }
    out.push(file_check(&assets_dir.join("press-pods/denoise.rnnn")));
    for path in [
        omni_personal::printer::service::BRLASER_FILTER,
        omni_personal::printer::service::CUPS_FILTER,
        omni_personal::printer::service::BRLASER_PPD,
    ] {
        out.push(file_check(Path::new(path)));
    }
    out.push(match command_output("pdfinfo", &["-v"]).await {
        Ok(_) => Check::new("pdfinfo -v", true, "runs"),
        Err(error) => Check::new("pdfinfo -v", false, error),
    });
    let models = omni_live_intel::speech::ModelFiles::in_dir(&env.model_dir);
    for path in models.all() {
        out.push(model_check(path));
    }
    let yt_dlp = env.yt_dlp.as_str();
    out.push(match command_output(yt_dlp, &["--version"]).await {
        Ok(version) => Check::new("yt-dlp --version", true, version.trim().to_owned()),
        Err(error) => Check::new("yt-dlp --version", false, error),
    });
    out
}

/// A report line per check.
pub fn report(checks: &[Check]) -> String {
    checks
        .iter()
        .map(|c| {
            format!(
                "{} {}: {}\n",
                if c.ok { "ok  " } else { "FAIL" },
                c.name,
                c.detail
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_filters_in_ffmpeg_listings() {
        let listing = " ... arnndn            A->A       Reduce noise from speech using Recurrent Neural Networks.\n T.. firequalizer      A->A       Finite Impulse Response Equalizer.\n";
        assert!(lists(listing, "arnndn"));
        assert!(lists(listing, "firequalizer"));
        assert!(!lists(listing, "afftdn"));
        let encoders =
            " A....D libmp3lame           libmp3lame MP3 (MPEG audio layer 3) (codec mp3)\n";
        assert!(lists(encoders, "libmp3lame"));
    }

    #[test]
    fn reads_only_its_own_variables_with_the_config_defaults() {
        let env = DoctorEnv::from_lookup(|_| None);
        assert_eq!(env.ffmpeg, "ffmpeg");
        assert_eq!(env.yt_dlp, "yt-dlp");
        assert_eq!(env.model_dir, PathBuf::from(DEFAULT_MODEL_DIR));
        let env = DoctorEnv::from_lookup(|key| match key {
            "FFMPEG_PATH" => Some(String::new()),
            "YT_DLP_PATH" => Some("/bin/yt".to_owned()),
            "LIVESTREAM_MODEL_DIR" => Some("/m".to_owned()),
            _ => None,
        });
        assert_eq!(
            env,
            DoctorEnv {
                ffmpeg: "ffmpeg".to_owned(),
                yt_dlp: "/bin/yt".to_owned(),
                model_dir: PathBuf::from("/m"),
            }
        );
    }

    #[test]
    fn the_default_model_dir_matches_the_production_config() {
        let config = omni_config::Config::from_env(&std::collections::BTreeMap::new())
            .expect("default config");
        assert_eq!(config.livestream_model_dir, DEFAULT_MODEL_DIR);
    }

    #[tokio::test]
    async fn fails_empty_or_corrupt_model_files_instead_of_passing_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = omni_live_intel::speech::ModelFiles::in_dir(dir.path());
        for path in models.all() {
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, b"").expect("write");
        }
        std::fs::write(&models.vad, [0x08, 0x08, 0x3a, 0x01, 0x00]).expect("write vad");
        let env = DoctorEnv {
            ffmpeg: "/nonexistent/ffmpeg".to_owned(),
            yt_dlp: "/nonexistent/yt-dlp".to_owned(),
            model_dir: dir.path().to_path_buf(),
        };
        let checks = checks(&env, dir.path()).await;
        let model = |path: &Path| {
            checks
                .iter()
                .find(|c| c.name == path.display().to_string())
                .expect("model check")
                .clone()
        };
        assert!(model(&models.vad).ok);
        let encoder = model(&models.encoder);
        assert!(!encoder.ok);
        assert_eq!(encoder.detail, "file is empty");
    }
}
