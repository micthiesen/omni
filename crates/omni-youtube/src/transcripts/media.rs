//! Video audio through `yt-dlp` and `ffmpeg` subprocesses: metadata, an
//! audio-only download into the job's work directory, and decoding the
//! downloaded file one bounded window at a time.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::future::BoxFuture;
use omni_core::process::{ProcessError, run_bounded};
use omni_runtime::ports::SPEECH_SAMPLE_RATE;
use serde::Deserialize;
use tokio::process::Command;

/// The largest audio file a job downloads. Six hours of YouTube's audio-only
/// formats stay well under it.
pub const MAX_DOWNLOAD_BYTES: u64 = 1024 * 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(90);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const DECODE_TIMEOUT: Duration = Duration::from_secs(3 * 60);
const MAX_STDERR_BYTES: usize = 256 * 1024;
const MAX_MESSAGE_CHARS: usize = 300;
const MAX_METADATA_CHARS: usize = 200;

/// The original-language track when a video has dubs, at a bitrate speech
/// recognition handles well; `worstaudio` alone can pick a dubbed track.
const AUDIO_FORMAT: &str = "worstaudio[language_preference>=5][abr>=40]/worstaudio[abr>=40]/worstaudio[language_preference>=5]/worstaudio/worst";
/// Only the fields a job needs: a full info dump of a long video can run to megabytes.
const PROBE_TEMPLATE: &str = "%(.{title,channel,uploader,duration,live_status,upload_date})j";

/// What yt-dlp reports about a video.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VideoInfo {
    pub title: Option<String>,
    pub channel: Option<String>,
    pub duration_ms: Option<i64>,
    /// yt-dlp's `live_status`: `not_live`, `was_live`, `is_live`, `is_upcoming`, `post_live`.
    pub live_status: Option<String>,
    /// `YYYYMMDD`.
    pub upload_date: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MediaError {
    /// `yt-dlp` or `ffmpeg` exited with an error; `message` is its last error line.
    #[error("{tool} failed: {message}")]
    Tool { tool: &'static str, message: String },
    #[error("the audio is larger than {} MiB", MAX_DOWNLOAD_BYTES / (1024 * 1024))]
    TooLarge,
    /// yt-dlp exited cleanly without leaving an audio file.
    #[error(
        "yt-dlp downloaded no audio: the file is over the size limit or the video has no audio format"
    )]
    NoAudio,
    #[error("{0}")]
    Io(String),
}

/// The media seam of a transcript job (fakes in tests).
pub trait MediaSource: Send + Sync + 'static {
    fn probe<'a>(&'a self, video_id: &'a str) -> BoxFuture<'a, Result<VideoInfo, MediaError>>;
    /// Downloads the audio into `dir` (created by the caller) and returns the file.
    fn download<'a>(
        &'a self,
        video_id: &'a str,
        dir: &'a Path,
    ) -> BoxFuture<'a, Result<PathBuf, MediaError>>;
    /// Mono [`SPEECH_SAMPLE_RATE`] samples of `length_ms` from `offset_ms`;
    /// fewer at the end of the file, none past it.
    fn decode<'a>(
        &'a self,
        file: &'a Path,
        offset_ms: i64,
        length_ms: i64,
    ) -> BoxFuture<'a, Result<Vec<f32>, MediaError>>;
}

/// The only URL form handed to yt-dlp, so no caller-supplied URL reaches it.
pub fn watch_url(video_id: &str) -> String {
    format!("https://www.youtube.com/watch?v={video_id}")
}

#[derive(Deserialize)]
struct ProbeOutput {
    title: Option<String>,
    channel: Option<String>,
    uploader: Option<String>,
    duration: Option<f64>,
    live_status: Option<String>,
    upload_date: Option<String>,
}

/// Uploader-controlled text: one line, bounded.
fn non_blank(value: Option<String>) -> Option<String> {
    let value = value?;
    let line: String = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let line = line.trim();
    if line.is_empty() || line == "NA" {
        return None;
    }
    let mut bounded: String = line.chars().take(MAX_METADATA_CHARS).collect();
    if line.chars().count() > MAX_METADATA_CHARS {
        bounded.push_str("...");
    }
    Some(bounded)
}

/// Decodes the [`PROBE_TEMPLATE`] line.
pub fn parse_probe(stdout: &[u8]) -> Result<VideoInfo, MediaError> {
    let raw: ProbeOutput = serde_json::from_slice(stdout).map_err(|e| MediaError::Tool {
        tool: "yt-dlp",
        message: format!("unreadable video metadata: {e}"),
    })?;
    #[allow(clippy::cast_possible_truncation)]
    let duration_ms = raw
        .duration
        .filter(|d| d.is_finite() && *d > 0.0)
        .map(|d| (d * 1000.0).round() as i64);
    Ok(VideoInfo {
        title: non_blank(raw.title),
        channel: non_blank(raw.channel).or_else(|| non_blank(raw.uploader)),
        duration_ms,
        live_status: non_blank(raw.live_status),
        upload_date: raw
            .upload_date
            .filter(|d| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit())),
    })
}

/// The tool's last `ERROR:` line (or last line), bounded.
pub fn error_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let line = lines
        .iter()
        .rev()
        .find(|l| l.starts_with("ERROR:"))
        .or(lines.last())
        .map_or("no error output", |l| l.trim_start_matches("ERROR:").trim());
    let mut message: String = line.chars().take(MAX_MESSAGE_CHARS).collect();
    if line.chars().count() > MAX_MESSAGE_CHARS {
        message.push_str("...");
    }
    message
}

fn seconds(ms: i64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}

/// `ffmpeg` arguments that decode one window of a local file to raw samples.
pub fn decode_args(file: &Path, offset_ms: i64, length_ms: i64) -> Vec<String> {
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    if offset_ms > 0 {
        args.extend(["-ss".to_owned(), seconds(offset_ms)]);
    }
    args.extend(["-t".to_owned(), seconds(length_ms), "-i".to_owned()]);
    args.push(file.to_string_lossy().into_owned());
    args.extend(["-vn", "-ac", "1", "-ar"].iter().map(|s| (*s).to_owned()));
    args.push(SPEECH_SAMPLE_RATE.to_string());
    args.extend(["-f", "f32le", "pipe:1"].iter().map(|s| (*s).to_owned()));
    args
}

fn decode_f32le(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

/// `yt-dlp` and `ffmpeg` on this host.
pub struct YtDlpMedia {
    yt_dlp: String,
    ffmpeg: String,
}

impl YtDlpMedia {
    pub fn new(yt_dlp: impl Into<String>, ffmpeg: impl Into<String>) -> Self {
        Self {
            yt_dlp: yt_dlp.into(),
            ffmpeg: ffmpeg.into(),
        }
    }

    fn yt_dlp(&self, extra: &[&str], video_id: &str) -> Command {
        let mut cmd = Command::new(&self.yt_dlp);
        cmd.args([
            "--quiet",
            "--no-warnings",
            "--no-playlist",
            "--js-runtimes",
            "node",
            "--user-agent",
            omni_http::USER_AGENT,
        ]);
        cmd.args(extra);
        cmd.arg(watch_url(video_id));
        cmd
    }

    async fn run(
        tool: &'static str,
        cmd: Command,
        stdout_cap: usize,
        timeout: Duration,
    ) -> Result<Vec<u8>, MediaError> {
        let output = run_bounded(cmd, None, stdout_cap, MAX_STDERR_BYTES, timeout)
            .await
            .map_err(|error| match error {
                ProcessError::Timeout(_) => MediaError::Tool {
                    tool,
                    message: format!("timed out after {} s", timeout.as_secs()),
                },
                other => MediaError::Tool {
                    tool,
                    message: other.to_string(),
                },
            })?;
        if output.status == 0 {
            Ok(output.stdout)
        } else {
            Err(MediaError::Tool {
                tool,
                message: error_line(&output.stderr),
            })
        }
    }
}

/// The downloaded audio in `dir`: yt-dlp leaves `.part` and `.ytdl` files
/// behind for a download it did not finish.
async fn downloaded_file(dir: &Path) -> Result<Option<PathBuf>, MediaError> {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| MediaError::Io(format!("read the work directory: {e}")))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| MediaError::Io(format!("read the work directory: {e}")))?
    {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let partial = name.ends_with(".part") || name.ends_with(".ytdl");
        if name.starts_with("audio.") && !partial {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

impl MediaSource for YtDlpMedia {
    fn probe<'a>(&'a self, video_id: &'a str) -> BoxFuture<'a, Result<VideoInfo, MediaError>> {
        Box::pin(async move {
            let cmd = self.yt_dlp(&["--skip-download", "--print", PROBE_TEMPLATE], video_id);
            let stdout = Self::run("yt-dlp", cmd, 256 * 1024, PROBE_TIMEOUT).await?;
            parse_probe(&stdout)
        })
    }

    fn download<'a>(
        &'a self,
        video_id: &'a str,
        dir: &'a Path,
    ) -> BoxFuture<'a, Result<PathBuf, MediaError>> {
        Box::pin(async move {
            let template = dir.join("audio.%(ext)s").to_string_lossy().into_owned();
            let max = MAX_DOWNLOAD_BYTES.to_string();
            let cmd = self.yt_dlp(
                &[
                    "--format",
                    AUDIO_FORMAT,
                    "--max-filesize",
                    &max,
                    "--retries",
                    "3",
                    "--socket-timeout",
                    "30",
                    "--output",
                    &template,
                ],
                video_id,
            );
            Self::run("yt-dlp", cmd, 256 * 1024, DOWNLOAD_TIMEOUT).await?;
            // yt-dlp skips a file over --max-filesize and still exits 0.
            let file = downloaded_file(dir).await?.ok_or(MediaError::NoAudio)?;
            let size = tokio::fs::metadata(&file)
                .await
                .map_err(|e| MediaError::Io(format!("read the downloaded audio: {e}")))?
                .len();
            if size > MAX_DOWNLOAD_BYTES {
                return Err(MediaError::TooLarge);
            }
            Ok(file)
        })
    }

    fn decode<'a>(
        &'a self,
        file: &'a Path,
        offset_ms: i64,
        length_ms: i64,
    ) -> BoxFuture<'a, Result<Vec<f32>, MediaError>> {
        Box::pin(async move {
            let mut cmd = Command::new(&self.ffmpeg);
            cmd.args(decode_args(file, offset_ms, length_ms));
            let samples =
                usize::try_from(length_ms).unwrap_or(0) / 1000 * SPEECH_SAMPLE_RATE as usize;
            // A tenth over the requested window, as resampling can round up.
            let cap = samples * 4 + samples * 4 / 10 + 64 * 1024;
            let stdout = Self::run("ffmpeg", cmd, cap, DECODE_TIMEOUT).await?;
            Ok(decode_f32le(&stdout))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_output_becomes_video_info() {
        let info = parse_probe(
            br#"{"title": " Me at the zoo ", "channel": "NA", "uploader": "jawed", "duration": 19.4, "live_status": "not_live", "upload_date": "20050424"}"#,
        );
        assert_eq!(
            info,
            Ok(VideoInfo {
                title: Some("Me at the zoo".into()),
                channel: Some("jawed".into()),
                duration_ms: Some(19_400),
                live_status: Some("not_live".into()),
                upload_date: Some("20050424".into()),
            })
        );
        // A live stream has no duration yet.
        let live = parse_probe(br#"{"title": "Live", "live_status": "is_live"}"#);
        assert_eq!(live.map(|i| i.duration_ms), Ok(None));
        // An uploader's title is cut to one bounded line; a malformed date is dropped.
        let hostile = parse_probe(
            format!(
                r#"{{"title": "line one\nline two {}", "upload_date": "2005-04-24"}}"#,
                "x".repeat(500)
            )
            .as_bytes(),
        )
        .unwrap_or_default();
        let title = hostile.title.unwrap_or_default();
        assert!(title.starts_with("line one line two xxx"), "{title}");
        assert_eq!(title.chars().count(), 203);
        assert_eq!(hostile.upload_date, None);
        assert!(matches!(
            parse_probe(b"not json"),
            Err(MediaError::Tool { tool: "yt-dlp", .. })
        ));
    }

    #[test]
    fn the_last_error_line_is_reported_and_bounded() {
        let stderr = b"WARNING: slow\nERROR: [youtube] abc: Private video. Sign in\n\n";
        assert_eq!(error_line(stderr), "[youtube] abc: Private video. Sign in");
        assert_eq!(error_line(b"something broke\n"), "something broke");
        assert_eq!(error_line(b""), "no error output");
        let long = format!("ERROR: {}", "x".repeat(1000));
        assert_eq!(error_line(long.as_bytes()).chars().count(), 303);
    }

    #[test]
    fn decoding_seeks_inside_the_local_file() {
        let args = decode_args(Path::new("/work/audio.webm"), 125_250, 120_000);
        assert_eq!(
            args.join(" "),
            "-hide_banner -loglevel error -nostdin -ss 125.250 -t 120.000 -i /work/audio.webm -vn -ac 1 -ar 16000 -f f32le pipe:1"
        );
        assert!(!decode_args(Path::new("a"), 0, 1000).contains(&"-ss".to_owned()));
    }

    #[test]
    fn only_the_canonical_watch_url_reaches_yt_dlp() {
        assert_eq!(
            watch_url("jNQXAC9IVRw"),
            "https://www.youtube.com/watch?v=jNQXAC9IVRw"
        );
    }
}
