//! Validation of the ids tools accept, including YouTube URLs.

use crate::error::YoutubeError;

fn id_chars(id: &str) -> bool {
    id.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_video_id(id: &str) -> bool {
    id.len() == 11 && id_chars(id)
}

fn youtube_url(raw: &str) -> Option<url::Url> {
    let with_scheme = if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("https://{raw}")
    };
    let url = url::Url::parse(&with_scheme).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    matches!(
        host,
        "youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtu.be"
    )
    .then_some(url)
}

/// A video id, or a watch, youtu.be, shorts, live or embed URL.
pub fn video_id(raw: &str) -> Result<String, YoutubeError> {
    let raw = raw.trim();
    if is_video_id(raw) {
        return Ok(raw.to_owned());
    }
    let found = youtube_url(raw).and_then(|url| {
        if url.host_str()?.ends_with("youtu.be") {
            return url.path_segments()?.next().map(str::to_owned);
        }
        if let Some((_, v)) = url.query_pairs().find(|(k, _)| k == "v") {
            return Some(v.into_owned());
        }
        let mut segments = url.path_segments()?;
        match segments.next()? {
            "shorts" | "live" | "embed" | "v" => segments.next().map(str::to_owned),
            _ => None,
        }
    });
    found
        .filter(|id| is_video_id(id))
        .ok_or_else(|| YoutubeError::Invalid(format!("{raw:?} is not a YouTube video id or URL")))
}

/// A playlist id or a URL with `list=`. Watch Later (`WL`) and watch history
/// (`HL`) are refused: the API has not served them since 2016.
pub fn playlist_id(raw: &str) -> Result<String, YoutubeError> {
    let raw = raw.trim();
    let id = if raw.contains('/') || raw.contains('?') {
        youtube_url(raw)
            .and_then(|url| {
                url.query_pairs()
                    .find(|(k, _)| k == "list")
                    .map(|(_, v)| v.into_owned())
            })
            .unwrap_or_default()
    } else {
        raw.to_owned()
    };
    if matches!(id.as_str(), "WL" | "HL") {
        return Err(YoutubeError::Invalid(
            "Watch Later and watch history are not available through the YouTube Data API".into(),
        ));
    }
    if (2..=64).contains(&id.len()) && id_chars(&id) {
        Ok(id)
    } else {
        Err(YoutubeError::Invalid(format!(
            "{raw:?} is not a YouTube playlist id or URL"
        )))
    }
}

/// A `UC…` channel id or a `/channel/UC…` URL.
pub fn channel_id(raw: &str) -> Result<String, YoutubeError> {
    let raw = raw.trim();
    let id = match youtube_url(raw).filter(|_| raw.contains('/')) {
        Some(url) => {
            let mut segments = url.path_segments().into_iter().flatten();
            match segments.next() {
                Some("channel") => segments.next().unwrap_or("").to_owned(),
                _ => String::new(),
            }
        }
        None => raw.to_owned(),
    };
    if id.len() == 24 && id.starts_with("UC") && id_chars(&id) {
        Ok(id)
    } else {
        Err(YoutubeError::Invalid(format!(
            "{raw:?} is not a YouTube channel id (UC followed by 22 characters)"
        )))
    }
}

/// A handle such as `@name`, with or without the `@`, or a youtube.com/@name URL.
pub fn handle(raw: &str) -> Result<String, YoutubeError> {
    let raw = raw.trim();
    let name = match youtube_url(raw).filter(|_| raw.contains('/')) {
        Some(url) => url
            .path_segments()
            .and_then(|mut s| s.next())
            .unwrap_or("")
            .to_owned(),
        None => raw.to_owned(),
    };
    let name = name.strip_prefix('@').unwrap_or(&name);
    let valid = (3..=30).contains(&name.chars().count())
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if valid {
        Ok(format!("@{name}"))
    } else {
        Err(YoutubeError::Invalid(format!(
            "{raw:?} is not a YouTube handle"
        )))
    }
}

/// A playlist item id (opaque base64 from YouTube).
pub fn playlist_item_id(raw: &str) -> Result<String, YoutubeError> {
    let raw = raw.trim();
    let valid = (8..=200).contains(&raw.len())
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'='));
    if valid {
        Ok(raw.to_owned())
    } else {
        Err(YoutubeError::Invalid(format!(
            "{raw:?} is not a playlist item id; take it from youtube_playlist_items_list"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_ids_come_from_ids_and_urls() {
        for raw in [
            "dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=10",
            "youtu.be/dQw4w9WgXcQ?si=x",
            "https://youtube.com/shorts/dQw4w9WgXcQ",
            "https://m.youtube.com/live/dQw4w9WgXcQ",
        ] {
            assert_eq!(video_id(raw).unwrap(), "dQw4w9WgXcQ", "{raw}");
        }
        assert!(video_id("https://example.com/watch?v=dQw4w9WgXcQ").is_err());
        assert!(video_id("short").is_err());
    }

    #[test]
    fn watch_later_and_history_are_refused() {
        for raw in ["WL", "HL", "https://www.youtube.com/playlist?list=WL"] {
            assert!(
                matches!(playlist_id(raw), Err(YoutubeError::Invalid(m)) if m.contains("Watch Later")),
                "{raw}"
            );
        }
        assert_eq!(
            playlist_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLabc_123").unwrap(),
            "PLabc_123"
        );
        assert_eq!(playlist_id("LL").unwrap(), "LL");
    }

    #[test]
    fn channels_and_handles() {
        let id = "UCabcdefghijklmnopqrstuv";
        assert_eq!(channel_id(id).unwrap(), id);
        assert_eq!(
            channel_id(&format!("https://www.youtube.com/channel/{id}")).unwrap(),
            id
        );
        assert!(channel_id("UCshort").is_err());
        assert_eq!(handle("@Some.Name").unwrap(), "@Some.Name");
        assert_eq!(
            handle("https://www.youtube.com/@abc_def").unwrap(),
            "@abc_def"
        );
        assert!(handle("@a").is_err());
    }
}
