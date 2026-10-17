//! Runtime prerequisites for `omni doctor`:
//! ffmpeg must ship `arnndn`, `firequalizer` and `loudnorm`, the `libmp3lame`
//! encoder, and the RNNoise model must be in the image.

use std::path::Path;
use std::time::Duration;

use crate::speech::audio_chain::DENOISE_MODEL_ASSET;

/// One prerequisite and whether it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctorCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

async fn ffmpeg_listing(ffmpeg: &str, flag: &str) -> Result<String, String> {
    let mut command = tokio::process::Command::new(ffmpeg);
    command.args(["-hide_banner", flag]);
    let output = omni_core::process::run_bounded(
        command,
        None,
        4 * 1024 * 1024,
        4 * 1024 * 1024,
        Duration::from_secs(30),
    )
    .await
    .map_err(|e| e.to_string())?;
    if output.status != 0 {
        return Err(format!("{ffmpeg} {flag} exited with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Lists every PressPods prerequisite with its status.
pub async fn checks(ffmpeg: &str, assets_dir: &Path) -> Vec<DoctorCheck> {
    let mut out = Vec::new();
    let filters = ffmpeg_listing(ffmpeg, "-filters").await;
    for filter in [
        "arnndn",
        "firequalizer",
        "loudnorm",
        "silenceremove",
        "atempo",
    ] {
        let ok = filters.as_ref().is_ok_and(|text| {
            text.lines()
                .any(|l| l.split_whitespace().nth(1) == Some(filter))
        });
        out.push(DoctorCheck {
            name: format!("ffmpeg filter {filter}"),
            ok,
            detail: filters.as_ref().err().cloned().unwrap_or_default(),
        });
    }
    let encoders = ffmpeg_listing(ffmpeg, "-encoders").await;
    out.push(DoctorCheck {
        name: "ffmpeg encoder libmp3lame".to_owned(),
        ok: encoders
            .as_ref()
            .is_ok_and(|text| text.contains("libmp3lame")),
        detail: encoders.err().unwrap_or_default(),
    });
    for asset in [
        DENOISE_MODEL_ASSET,
        "press-pods/intro.mp3",
        "press-pods/logo.jpeg",
    ] {
        let path = assets_dir.join(asset);
        out.push(DoctorCheck {
            name: format!("asset {asset}"),
            ok: path.is_file(),
            detail: path.display().to_string(),
        });
    }
    out
}
