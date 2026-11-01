//! Names a download Plex imported without tags.
//!
//! Some releases carry no embedded tags, so Plex files them as a blank album
//! under "Various Artists" with blank, unnumbered tracks. The files are
//! seeding and must not change, so Omni edits only Plex's metadata, locked:
//! the album gets the grab's book title and moves to the author's artist,
//! and each track gets a title and number ([`plan`]). The edits name exact
//! Plex ids, never a whole section.

use omni_http::{Method, SideEffectMode, Url};

use super::{Audiobooks, READ_MAX_BYTES, READ_TIMEOUT, Recorded};

/// One Plex track to name: its rating key and file path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackFile {
    pub rating_key: String,
    pub file: String,
}

/// The metadata to write for one untagged album.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagPlan {
    pub title: String,
    pub artist: String,
    /// (rating key, title, track number), in play order.
    pub tracks: Vec<(String, String, u32)>,
}

fn stem(file: &str) -> &str {
    let name = file.rsplit('/').next().unwrap_or(file);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// The number a file name starts with (`07 Chapter.mp3` is 7); four or more
/// digits read as a year, not a track.
fn leading_number(file: &str) -> Option<u32> {
    let digits: String = stem(file)
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.len() > 3 {
        return None;
    }
    digits.parse().ok()
}

/// Album title, artist and per-track titles and numbers. Tracks are
/// numbered by the number each file name starts with when every file has a
/// distinct one, else by path order; a single track is titled with the
/// book. `None` without a book, an author or tracks.
pub fn plan(book: Option<&str>, author: Option<&str>, files: &[TrackFile]) -> Option<TagPlan> {
    let title = book.map(str::trim).filter(|b| !b.is_empty())?;
    let artist = author.map(str::trim).filter(|a| !a.is_empty())?;
    if files.is_empty() {
        return None;
    }
    let mut ordered: Vec<&TrackFile> = files.iter().collect();
    let numbers: Vec<Option<u32>> = ordered.iter().map(|t| leading_number(&t.file)).collect();
    let mut distinct: Vec<u32> = numbers.iter().flatten().copied().collect();
    distinct.sort_unstable();
    distinct.dedup();
    let numbered = distinct.len() == files.len();
    if numbered {
        ordered.sort_by_key(|t| leading_number(&t.file));
    } else {
        ordered.sort_by(|a, b| a.file.cmp(&b.file));
    }
    let single = ordered.len() == 1;
    let tracks = ordered
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            let position = u32::try_from(i + 1).unwrap_or(u32::MAX);
            let index = if numbered {
                leading_number(&t.file).unwrap_or(position)
            } else {
                position
            };
            let name = if single {
                title.to_owned()
            } else {
                format!("Part {index}")
            };
            (t.rating_key.clone(), name, index)
        })
        .collect();
    Some(TagPlan {
        title: title.to_owned(),
        artist: artist.to_owned(),
        tracks,
    })
}

impl Audiobooks {
    /// Writes `plan` for album `album_key` in music section `section`.
    pub(super) async fn tag_in_plex(
        &self,
        section: &str,
        album_key: &str,
        plan: &TagPlan,
    ) -> Result<(), String> {
        if self.mode == SideEffectMode::Record {
            self.record(Recorded::PlexTag {
                album_key: album_key.to_owned(),
                title: plan.title.clone(),
                artist: plan.artist.clone(),
            });
            return Ok(());
        }
        // Tracks first: a titled album is what marks the book ready, so it
        // must not appear before its tracks are named.
        for (key, title, index) in &plan.tracks {
            let index = index.to_string();
            self.plex_edit(
                section,
                &[
                    ("type", "10"),
                    ("id", key),
                    ("title.value", title),
                    ("title.locked", "1"),
                    ("index.value", &index),
                    ("index.locked", "1"),
                ],
            )
            .await?;
        }
        self.plex_edit(
            section,
            &[
                ("type", "9"),
                ("id", album_key),
                ("title.value", &plan.title),
                ("title.locked", "1"),
                ("artist.title.value", &plan.artist),
            ],
        )
        .await
    }

    /// One `PUT /library/sections/{section}/all` edit; `params` always name
    /// an `id`, so it never touches the whole section.
    async fn plex_edit(&self, section: &str, params: &[(&str, &str)]) -> Result<(), String> {
        if !params.iter().any(|(k, v)| *k == "id" && !v.is_empty()) {
            return Err("a Plex edit needs an item id".to_owned());
        }
        let (base, token) = self.plex_scan.as_ref().ok_or("Plex is not configured")?;
        let mut url = Url::parse(&format!("{}/", base.trim_end_matches('/')))
            .and_then(|b| b.join(&format!("library/sections/{section}/all")))
            .map_err(|_| "invalid PLEX_URL")?;
        url.query_pairs_mut().extend_pairs(params);
        let request = self
            .http
            .request(Method::PUT, url)
            .header("X-Plex-Token", token.as_str());
        let response = tokio::time::timeout(READ_TIMEOUT, request.send_bounded(READ_MAX_BYTES))
            .await
            .map_err(|_| "the Plex edit timed out".to_owned())?
            .map_err(|_| "the Plex edit failed".to_owned())?;
        if response.status.is_success() {
            Ok(())
        } else {
            Err(format!(
                "Plex edit answered HTTP {}",
                response.status.as_u16()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TrackFile, plan};

    fn files(names: &[&str]) -> Vec<TrackFile> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| TrackFile {
                rating_key: (100 + i).to_string(),
                file: format!("/books/Kelly Weill - Off The Edge/{n}"),
            })
            .collect()
    }

    /// The live Off the Edge import: twelve untagged mp3s named 1 to 12.
    #[test]
    fn numbers_tracks_by_their_file_number_not_text_order() {
        let names: Vec<String> = [1, 10, 11, 12, 2, 3, 4, 5, 6, 7, 8, 9]
            .iter()
            .map(|n| format!("{n}.mp3"))
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let p = plan(Some("Off the Edge"), Some("Kelly Weill"), &files(&refs)).expect("plan");
        assert_eq!(p.title, "Off the Edge");
        assert_eq!(p.artist, "Kelly Weill");
        let order: Vec<(String, u32)> = p.tracks.iter().map(|(_, t, i)| (t.clone(), *i)).collect();
        assert_eq!(order[0], ("Part 1".to_owned(), 1));
        assert_eq!(order[1], ("Part 2".to_owned(), 2));
        assert_eq!(order[11], ("Part 12".to_owned(), 12));
        assert_eq!(p.tracks[0].0, "100");
        assert_eq!(p.tracks[1].0, "104");
    }

    #[test]
    fn a_single_file_takes_the_book_title_and_unknown_books_are_left_alone() {
        let one = files(&["Trust the Plan.m4b"]);
        let p = plan(Some("Trust the Plan"), Some("Will Sommer"), &one).expect("plan");
        assert_eq!(
            p.tracks,
            [("100".to_owned(), "Trust the Plan".to_owned(), 1)]
        );
        assert!(plan(None, Some("Will Sommer"), &one).is_none());
        assert!(plan(Some("Trust the Plan"), Some(" "), &one).is_none());
        let unnumbered = files(&["b.mp3", "a.mp3"]);
        let p = plan(Some("X"), Some("Y"), &unnumbered).expect("plan");
        assert_eq!(p.tracks[0], ("101".to_owned(), "Part 1".to_owned(), 1));
    }

    #[test]
    fn repeated_disc_numbers_and_years_fall_back_to_path_order() {
        let discs = files(&["CD2/01.mp3", "CD1/02.mp3", "CD1/01.mp3"]);
        let p = plan(Some("X"), Some("Y"), &discs).expect("plan");
        let keys: Vec<&str> = p.tracks.iter().map(|(k, _, _)| k.as_str()).collect();
        assert_eq!(keys, ["102", "101", "100"]);
        let years = files(&["2024 Book.m4b", "2023 Book.m4b"]);
        let p = plan(Some("X"), Some("Y"), &years).expect("plan");
        assert_eq!(p.tracks[0], ("101".to_owned(), "Part 1".to_owned(), 1));
    }
}
