//! HLS playlist parsing for the Kick relay: the master playlist's top variant
//! and the media playlist's segments. Pure.

use url::Url;

/// One `#EXT-X-STREAM-INF` entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Variant {
    pub url: Url,
    pub bandwidth: u64,
    pub width: u32,
    pub height: u32,
    pub frame_rate: Option<f64>,
    /// The `VIDEO` group or `NAME`, e.g. `1080p60` (Kick names the source
    /// rendition `chunked`).
    pub name: Option<String>,
}

impl Variant {
    /// `1920x1080`, or `None` when the playlist omits `RESOLUTION`.
    pub fn resolution(&self) -> Option<String> {
        (self.width > 0 && self.height > 0).then(|| format!("{}x{}", self.width, self.height))
    }
}

/// One media segment.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaSegment {
    pub sequence: u64,
    pub duration_ms: i64,
    pub url: Url,
    /// An `#EXT-X-DISCONTINUITY` precedes this segment.
    pub discontinuity: bool,
}

/// A parsed media playlist.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaPlaylist {
    pub segments: Vec<MediaSegment>,
    /// `#EXT-X-ENDLIST`: the broadcast ended or the playlist was closed.
    pub ended: bool,
}

/// `KEY=value,KEY="quoted, value"` attribute lists.
fn attributes(list: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = list.trim();
    while !rest.is_empty() {
        let Some((key, after)) = rest.split_once('=') else {
            break;
        };
        let (value, tail) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.split_once('"') {
                Some((value, tail)) => (value, tail),
                None => (quoted, ""),
            }
        } else {
            match after.split_once(',') {
                Some((value, tail)) => (value, tail),
                None => (after, ""),
            }
        };
        out.push((key.trim().to_ascii_uppercase(), value.to_owned()));
        rest = tail.trim_start_matches(',').trim_start();
    }
    out
}

fn attribute<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// The highest variant: largest picture, then frame rate, then bandwidth.
/// `None` when the playlist lists no usable variant.
pub fn top_variant(master: &str, base: &Url) -> Option<Variant> {
    let mut variants = Vec::new();
    let mut pending: Option<Vec<(String, String)>> = None;
    for line in master.lines().map(str::trim) {
        if let Some(list) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(attributes(list));
        } else if line.is_empty() || line.starts_with('#') {
            continue;
        } else if let Some(attrs) = pending.take() {
            let Ok(url) = base.join(line) else {
                continue;
            };
            let (width, height) = attribute(&attrs, "RESOLUTION")
                .and_then(|r| r.split_once('x'))
                .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                .unwrap_or((0, 0));
            variants.push(Variant {
                url,
                bandwidth: attribute(&attrs, "BANDWIDTH")
                    .and_then(|b| b.parse().ok())
                    .unwrap_or(0),
                width,
                height,
                frame_rate: attribute(&attrs, "FRAME-RATE").and_then(|f| f.parse().ok()),
                name: attribute(&attrs, "VIDEO")
                    .or_else(|| attribute(&attrs, "NAME"))
                    .map(str::to_owned),
            });
        }
    }
    variants.into_iter().max_by(|a, b| {
        let pixels = |v: &Variant| u64::from(v.width) * u64::from(v.height);
        pixels(a)
            .cmp(&pixels(b))
            .then(
                a.frame_rate
                    .unwrap_or(0.0)
                    .total_cmp(&b.frame_rate.unwrap_or(0.0)),
            )
            .then(a.bandwidth.cmp(&b.bandwidth))
    })
}

/// Segments with their sequence numbers. Low-latency `#EXT-X-PREFETCH`
/// entries are segments still being written and are skipped; the next
/// refresh lists them once complete.
pub fn parse_media(text: &str, base: &Url) -> Option<MediaPlaylist> {
    if !text.trim_start().starts_with("#EXTM3U") {
        return None;
    }
    let mut sequence: u64 = 0;
    let mut segments = Vec::new();
    let mut duration: Option<f64> = None;
    let mut discontinuity = false;
    let mut ended = false;
    for line in text.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            sequence = value.trim().parse().ok()?;
        } else if let Some(value) = line.strip_prefix("#EXTINF:") {
            let number = value.split(',').next().unwrap_or("").trim();
            duration = number
                .parse()
                .ok()
                .filter(|d: &f64| d.is_finite() && *d > 0.0);
        } else if line == "#EXT-X-DISCONTINUITY" {
            discontinuity = true;
        } else if line == "#EXT-X-ENDLIST" {
            ended = true;
        } else if line.is_empty() || line.starts_with('#') {
            continue;
        } else {
            let Some(seconds) = duration.take() else {
                continue;
            };
            let Ok(url) = base.join(line) else {
                continue;
            };
            #[allow(clippy::cast_possible_truncation)]
            segments.push(MediaSegment {
                sequence: sequence + segments.len() as u64,
                duration_ms: (seconds * 1000.0).round() as i64,
                url,
                discontinuity: std::mem::take(&mut discontinuity),
            });
        }
    }
    Some(MediaPlaylist { segments, ended })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example/api/video/v1/channel.m3u8?token=t").unwrap()
    }

    const MASTER: &str = "#EXTM3U\n\
#EXT-X-SESSION-DATA:DATA-ID=\"NODE\",VALUE=\"n\"\n\
#EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID=\"720p60\",NAME=\"720p60\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=3422999,RESOLUTION=1280x720,CODECS=\"avc1.4D401F,mp4a.40.2\",VIDEO=\"720p60\",FRAME-RATE=60.000\n\
https://cdn.example/v1/playlist/720.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=8992204,RESOLUTION=1920x1080,CODECS=\"avc1.64002A,mp4a.40.2\",VIDEO=\"chunked\",FRAME-RATE=60.000\n\
https://cdn.example/v1/playlist/source.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=230000,RESOLUTION=284x160,CODECS=\"avc1.4D401F,mp4a.40.2\",VIDEO=\"160p30\",FRAME-RATE=30.000\n\
low/160.m3u8\n";

    #[test]
    fn picks_the_source_variant_whatever_its_position() {
        let top = top_variant(MASTER, &base()).unwrap();
        assert_eq!(
            top.url.as_str(),
            "https://cdn.example/v1/playlist/source.m3u8"
        );
        assert_eq!(top.resolution().as_deref(), Some("1920x1080"));
        assert_eq!(top.frame_rate, Some(60.0));
        assert_eq!(top.bandwidth, 8_992_204);
        assert_eq!(top.name.as_deref(), Some("chunked"));
    }

    #[test]
    fn frame_rate_breaks_a_resolution_tie() {
        let master = "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=9000000,RESOLUTION=1920x1080,FRAME-RATE=60\n60.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=9500000,RESOLUTION=1920x1080,FRAME-RATE=30\n30.m3u8\n";
        let top = top_variant(master, &base()).unwrap();
        assert!(top.url.as_str().ends_with("/60.m3u8"));
    }

    #[test]
    fn relative_variant_urls_resolve_against_the_master() {
        let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nlow/160.m3u8\n";
        let top = top_variant(master, &base()).unwrap();
        assert_eq!(
            top.url.as_str(),
            "https://cdn.example/api/video/v1/low/160.m3u8"
        );
        assert!(top_variant("#EXTM3U\n", &base()).is_none());
    }

    #[test]
    fn media_segments_carry_sequence_and_discontinuity_and_skip_prefetch() {
        let text = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n\
#EXT-X-MEDIA-SEQUENCE:10557\n\
#EXT-X-PROGRAM-DATE-TIME:2026-11-22T19:29:19.000Z\n\
#EXTINF:2.000,live\nhttps://cdn.example/v1/segment/a.ts\n\
#EXT-X-DISCONTINUITY\n\
#EXTINF:1.999,live\nseg/b.ts\n\
#EXT-X-PREFETCH:https://cdn.example/v1/segment/c.ts\n";
        let playlist = parse_media(text, &base()).unwrap();
        assert!(!playlist.ended);
        assert_eq!(playlist.segments.len(), 2);
        assert_eq!(playlist.segments[0].sequence, 10557);
        assert_eq!(playlist.segments[0].duration_ms, 2000);
        assert!(!playlist.segments[0].discontinuity);
        assert_eq!(playlist.segments[1].sequence, 10558);
        assert_eq!(playlist.segments[1].duration_ms, 1999);
        assert!(playlist.segments[1].discontinuity);
        assert_eq!(
            playlist.segments[1].url.as_str(),
            "https://cdn.example/api/video/v1/seg/b.ts"
        );
    }

    #[test]
    fn endlist_and_garbage() {
        let ended = parse_media("#EXTM3U\n#EXT-X-ENDLIST\n", &base()).unwrap();
        assert!(ended.ended && ended.segments.is_empty());
        assert!(parse_media("<html>blocked</html>", &base()).is_none());
    }
}
