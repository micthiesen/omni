//! Reads a `.torrent` file's info hash (Deluge's torrent id) and name, so a
//! grab is reserved, verified and reconciled by an id known before it is sent.

use sha1::{Digest as _, Sha1};

const MAX_DEPTH: usize = 64;

/// What a `.torrent` file identifies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TorrentFile {
    /// Lowercase hex SHA-1 of the bencoded `info` dictionary.
    pub info_hash: String,
    pub name: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a valid .torrent file")]
pub struct NotATorrent;

/// The end of the bencoded value starting at `at`.
fn skip(bytes: &[u8], at: usize, depth: usize) -> Result<usize, NotATorrent> {
    if depth > MAX_DEPTH {
        return Err(NotATorrent);
    }
    match bytes.get(at).ok_or(NotATorrent)? {
        b'i' => {
            let end = bytes[at..]
                .iter()
                .position(|b| *b == b'e')
                .ok_or(NotATorrent)?;
            Ok(at + end + 1)
        }
        b'l' | b'd' => {
            let mut pos = at + 1;
            while *bytes.get(pos).ok_or(NotATorrent)? != b'e' {
                pos = skip(bytes, pos, depth + 1)?;
            }
            Ok(pos + 1)
        }
        b'0'..=b'9' => {
            let (_, end) = string(bytes, at)?;
            Ok(end)
        }
        _ => Err(NotATorrent),
    }
}

/// The byte string starting at `at` and the position after it.
fn string(bytes: &[u8], at: usize) -> Result<(&[u8], usize), NotATorrent> {
    let colon = bytes[at..]
        .iter()
        .position(|b| *b == b':')
        .ok_or(NotATorrent)?;
    let len: usize = std::str::from_utf8(&bytes[at..at + colon])
        .ok()
        .and_then(|t| t.parse().ok())
        .ok_or(NotATorrent)?;
    let start = at + colon + 1;
    let end = start.checked_add(len).ok_or(NotATorrent)?;
    Ok((bytes.get(start..end).ok_or(NotATorrent)?, end))
}

pub fn parse(bytes: &[u8]) -> Result<TorrentFile, NotATorrent> {
    if bytes.first() != Some(&b'd') {
        return Err(NotATorrent);
    }
    let mut pos = 1;
    while *bytes.get(pos).ok_or(NotATorrent)? != b'e' {
        let (key, value_at) = string(bytes, pos)?;
        let value_end = skip(bytes, value_at, 1)?;
        if key == b"info" && bytes.get(value_at) == Some(&b'd') {
            let info = &bytes[value_at..value_end];
            return Ok(TorrentFile {
                info_hash: hex::encode(Sha1::digest(info)),
                name: info_name(info),
            });
        }
        pos = value_end;
    }
    Err(NotATorrent)
}

fn info_name(info: &[u8]) -> Option<String> {
    let mut pos = 1;
    while *info.get(pos)? != b'e' {
        let (key, value_at) = string(info, pos).ok()?;
        if key == b"name" {
            let (name, _) = string(info, value_at).ok()?;
            return Some(String::from_utf8_lossy(name).into_owned());
        }
        pos = skip(info, value_at, 1).ok()?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{NotATorrent, parse};

    #[test]
    fn hashes_the_info_dictionary_exactly() {
        let info =
            b"d6:lengthi12e4:name8:book.m4b12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaae";
        let mut torrent = b"d8:announce14:http://t/a?x=113:creation datei1e4:info".to_vec();
        torrent.extend_from_slice(info);
        torrent.push(b'e');

        let parsed = parse(&torrent).expect("torrent");

        use sha1::{Digest as _, Sha1};
        assert_eq!(parsed.info_hash, hex::encode(Sha1::digest(info)));
        assert_eq!(parsed.name.as_deref(), Some("book.m4b"));
    }

    #[test]
    fn rejects_what_is_not_a_torrent() {
        assert_eq!(parse(b"<html>login</html>"), Err(NotATorrent));
        assert_eq!(parse(b"d4:infoi1ee"), Err(NotATorrent));
        assert_eq!(parse(b"d4:info"), Err(NotATorrent));
    }
}
