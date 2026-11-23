//! The audio chain, with the ffmpeg arguments pinned
//! (`tests/golden/ffmpeg-args.json`) by a recording fake ffmpeg. The
//! real-ffmpeg run is `#[ignore]` (it needs ffmpeg with arnndn on PATH).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use omni_presspods::speech::audio_chain::{AudioChain, FIZZ_SHELF, RESAMPLE_HQ};

fn pp_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("pp_"))
        .collect()
}

#[tokio::test]
async fn kills_an_interrupted_ffmpeg_child_and_removes_every_owned_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let marker = dir.path().join("still-running");
    let ffmpeg = common::script(
        dir.path(),
        "ffmpeg",
        &format!("sleep 2\ntouch '{}'", marker.display()),
    );
    let chain = AudioChain::new(
        ffmpeg.display().to_string(),
        "ffprobe".to_owned(),
        Path::new("/assets/press-pods/denoise.rnnn"),
        tmp.clone(),
    )
    .unwrap();
    let chunk = dir.path().join("chunk.wav");
    std::fs::write(&chunk, b"wav").unwrap();
    let result = tokio::time::timeout(
        Duration::from_millis(200),
        chain.assemble_episode(&[chunk.as_path()], b"unused intro"),
    )
    .await;
    assert!(result.is_err(), "assembly should have been interrupted");
    assert!(pp_files(&tmp).is_empty(), "{:?}", pp_files(&tmp));
    // The killed child never reaches its next command.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(!marker.exists());
}

/// Replaces temporary paths with stable placeholders for comparison.
fn normalize(calls: &[Vec<String>], dir: &Path) -> Vec<Vec<String>> {
    let tmp = dir.join("tmp").display().to_string();
    calls
        .iter()
        .map(|call| {
            call.iter()
                .map(|arg| {
                    if arg.starts_with(&tmp) {
                        let ext = arg.rsplit('.').next().unwrap_or("");
                        format!("<tmp>.{ext}")
                    } else {
                        arg.replace(&dir.display().to_string(), "<dir>")
                    }
                })
                .collect()
        })
        .collect()
}

#[tokio::test]
async fn higgs_chunk_and_episode_arguments_match_the_golden() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let log = dir.path().join("ffmpeg.log");
    let ffmpeg = common::fake_ffmpeg(dir.path(), &log);
    let ffprobe = common::fake_ffprobe(dir.path(), "3.25");
    let chain = AudioChain::new(
        ffmpeg.display().to_string(),
        ffprobe.display().to_string(),
        Path::new("assets/press-pods/denoise.rnnn"),
        tmp.clone(),
    )
    .unwrap();

    let chunk = chain.prepare_chunk(b"raw mp3", true).await.unwrap();
    assert_eq!(chunk.duration_seconds, 3.25);
    let gap = chain.make_silence_wav(0.7).await.unwrap();
    let audio = chain
        .assemble_episode(&[chunk.wav.path(), gap.path(), chunk.wav.path()], b"intro")
        .await
        .unwrap();
    assert_eq!(audio, b"FAKE-AUDIO");
    drop((chunk, gap));
    assert!(pp_files(&tmp).is_empty(), "{:?}", pp_files(&tmp));

    let calls = normalize(&common::ffmpeg_calls(&log), dir.path());
    let golden: Vec<Vec<String>> = serde_json::from_str(
        &std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/ffmpeg-args.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(calls, golden);

    // Invariants, independent of the golden: every aresample is HQ, and the
    // Higgs chunk pass denoises with arnndn followed by the fizz shelf.
    for call in &calls {
        for arg in call {
            for (i, _) in arg.match_indices("aresample=") {
                let rest = &arg[i + "aresample=".len()..];
                let colon = rest.find(':').unwrap();
                assert!(
                    rest[colon..].starts_with(&format!(":{RESAMPLE_HQ}")),
                    "{arg}"
                );
            }
        }
    }
    let edge = &calls[0][calls[0].iter().position(|a| a == "-af").unwrap() + 1];
    let arnndn = edge
        .find("arnndn=m=assets/press-pods/denoise.rnnn")
        .unwrap();
    let shelf = edge.find(FIZZ_SHELF).unwrap();
    assert!(arnndn < shelf);
}

#[tokio::test]
#[ignore = "needs ffmpeg with arnndn and ffprobe on PATH; run manually"]
async fn real_ffmpeg_prepares_and_assembles_an_episode() {
    let dir = tempfile::tempdir().unwrap();
    // A 24 kHz mono MP3 like Higgs output: the intro jingle (RNNoise would
    // silence a pure test tone entirely, leaving loudnorm nothing to measure).
    let raw = dir.path().join("raw.mp3");
    let intro_asset =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/press-pods/intro.mp3");
    let status = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&intro_asset)
        .args(["-ar", "24000", "-ac", "1", "-c:a", "libmp3lame"])
        .arg(&raw)
        .status()
        .unwrap();
    assert!(status.success());
    let assets = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/press-pods");
    let chain = AudioChain::new(
        "ffmpeg",
        "ffprobe",
        &assets.join("denoise.rnnn").canonicalize().unwrap(),
        dir.path().to_owned(),
    )
    .unwrap();
    let mp3 = std::fs::read(&raw).unwrap();
    let denoised = chain.prepare_chunk(&mp3, true).await.unwrap();
    let clean = chain.prepare_chunk(&mp3, false).await.unwrap();
    assert!(denoised.duration_seconds > 0.0 && clean.duration_seconds > 0.0);
    let gap = chain.make_silence_wav(0.7).await.unwrap();
    let intro = std::fs::read(assets.join("intro.mp3")).unwrap();
    let episode = chain
        .assemble_episode(&[denoised.wav.path(), gap.path(), clean.wav.path()], &intro)
        .await
        .unwrap();
    let duration = omni_presspods::audio::audio_duration_seconds(&episode).unwrap();
    assert!(duration > 4.0, "{duration}");
    // The Xing frame count gives an exact frame multiple, like music-metadata.
    let frames = duration * 44_100.0 / 1152.0;
    assert!((frames - frames.round()).abs() < 1e-6, "{duration}");
}
