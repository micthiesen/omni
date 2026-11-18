//! Content-addressed image files under `<recipes_dir>/images/sha256`.
//!
//! `<aa>/<digest>` holds the original bytes and `<digest>.thumb.jpg` and
//! `<digest>.full.jpg` the two renditions written at intake. Each file is
//! written to a temporary name and renamed into place, so a reader (and a
//! file-level backup) never sees a partial file, and the same image added
//! twice is stored once. Final names never end in `.tmp`, `.part` or `.log`:
//! the off-site backup ignores those.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use omni_image::{ImageError, ImageKind, sniff_image};

use crate::db::sha256_hex;
use crate::error::{R, io, reject};

/// The long edge of the `thumb` rendition.
pub const THUMB_EDGE: u32 = 480;
/// The long edge of the `full` rendition.
pub const FULL_EDGE: u32 = 1600;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rendition {
    Original,
    Thumb,
    Full,
}

impl Rendition {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "thumb" => Some(Self::Thumb),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

/// Makes a rename durable: syncs the directory that holds `path`.
pub fn sync_dir(path: &Path) -> std::io::Result<()> {
    match path.parent() {
        Some(parent) => std::fs::File::open(parent)?.sync_all(),
        None => Ok(()),
    }
}

/// What intake learned about one stored image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    pub sha256: String,
    pub mime: String,
    pub bytes: u32,
    /// As displayed, after EXIF orientation.
    pub width: u32,
    pub height: u32,
    /// Average colour, `#rrggbb`.
    pub color: String,
}

#[derive(Clone, Debug)]
pub struct ImageFiles {
    root: PathBuf,
}

fn is_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The accepted formats, decided from magic bytes.
pub fn accepted_kind(data: &[u8]) -> R<ImageKind> {
    match sniff_image(data) {
        Some(kind @ (ImageKind::Jpeg | ImageKind::Png | ImageKind::Webp | ImageKind::Heif)) => {
            Ok(kind)
        }
        Some(other) => Err(reject(
            "unsupported_media",
            format!(
                "expected a JPEG, PNG, WebP or HEIC image; the bytes are {}",
                other.label()
            ),
        )),
        None => Err(reject(
            "unsupported_media",
            "expected a JPEG, PNG, WebP or HEIC image; the bytes are none of those",
        )),
    }
}

pub(crate) fn image_error(error: &ImageError) -> crate::error::RecipesError {
    match error {
        ImageError::Unsupported | ImageError::Decode(_) => {
            reject("unsupported_media", error.to_string())
        }
        ImageError::TooManyPixels | ImageError::TooLarge => reject("too_large", error.to_string()),
        // The decoder is busy or slow, not the image wrong: worth a retry.
        ImageError::Timeout => reject(
            "unavailable",
            "image processing is busy and timed out; try again",
        ),
        other => io(other),
    }
}

impl ImageFiles {
    pub fn new(recipes_dir: &Path) -> Self {
        Self {
            root: recipes_dir.join("images").join("sha256"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The path of one rendition; `None` when `digest` is not a SHA-256.
    pub fn path(&self, digest: &str, rendition: Rendition) -> Option<PathBuf> {
        if !is_digest(digest) {
            return None;
        }
        let dir = self.root.join(&digest[..2]);
        Some(match rendition {
            Rendition::Original => dir.join(digest),
            Rendition::Thumb => dir.join(format!("{digest}.thumb.jpg")),
            Rendition::Full => dir.join(format!("{digest}.full.jpg")),
        })
    }

    /// Stores `data` with its two renditions and returns what was learned.
    /// Storing the same bytes again rewrites nothing.
    pub async fn put(&self, data: Vec<u8>) -> R<Stored> {
        let kind = accepted_kind(&data)?;
        let digest = sha256_hex(&data);
        let bytes = u32::try_from(data.len()).unwrap_or(u32::MAX);
        let data = Arc::new(data);
        let variants = omni_image::variants(data.clone(), vec![THUMB_EDGE, FULL_EDGE])
            .await
            .map_err(|e| image_error(&e))?;
        let [thumb, full] = <[omni_image::Variant; 2]>::try_from(variants.variants)
            .map_err(|_| io("image renditions are missing"))?;
        let files = self.clone();
        let stored = digest.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let write = |rendition: Rendition, bytes: &[u8]| -> std::io::Result<()> {
                let path = files
                    .path(&stored, rendition)
                    .ok_or_else(|| std::io::Error::other("bad digest"))?;
                if path.exists() {
                    // Fresh again: the purge leaves recently used files alone.
                    return std::fs::File::options()
                        .write(true)
                        .open(&path)?
                        .set_modified(std::time::SystemTime::now());
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let temp = path.with_extension(format!("{}.tmp", omni_core::ids::uuid_v4()));
                std::fs::write(&temp, bytes)?;
                std::fs::File::open(&temp)?.sync_all()?;
                std::fs::rename(&temp, &path)?;
                sync_dir(&path)
            };
            // The original lands last: it is what marks the image as stored.
            write(Rendition::Thumb, &thumb.jpeg)?;
            write(Rendition::Full, &full.jpeg)?;
            write(Rendition::Original, &data)
        })
        .await
        .map_err(io)?
        .map_err(io)?;
        Ok(Stored {
            sha256: digest,
            mime: kind.mime().to_owned(),
            bytes,
            width: variants.width,
            height: variants.height,
            color: variants.average_color,
        })
    }

    pub async fn read(&self, digest: &str, rendition: Rendition) -> R<Option<Vec<u8>>> {
        let Some(path) = self.path(digest, rendition) else {
            return Ok(None);
        };
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io(error)),
        }
    }

    /// Every stored digest (blocking).
    pub fn digests(&self) -> std::io::Result<Vec<String>> {
        let mut found = Vec::new();
        let shards = match std::fs::read_dir(&self.root) {
            Ok(shards) => shards,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(found),
            Err(error) => return Err(error),
        };
        for shard in shards {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for file in std::fs::read_dir(shard.path())? {
                let name = file?.file_name();
                if let Some(name) = name.to_str()
                    && is_digest(name)
                {
                    found.push(name.to_owned());
                }
            }
        }
        found.sort();
        Ok(found)
    }

    /// Rendition files whose original is gone (a crash between the writes of
    /// one image, or a file removed by hand), by path (blocking).
    pub fn orphan_renditions(&self) -> std::io::Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        let shards = match std::fs::read_dir(&self.root) {
            Ok(shards) => shards,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(found),
            Err(error) => return Err(error),
        };
        for shard in shards {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for file in std::fs::read_dir(shard.path())? {
                let path = file?.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let digest = name
                    .strip_suffix(".thumb.jpg")
                    .or_else(|| name.strip_suffix(".full.jpg"));
                if let Some(digest) = digest
                    && is_digest(digest)
                    && !shard.path().join(digest).exists()
                {
                    found.push(path);
                }
            }
        }
        found.sort();
        Ok(found)
    }

    /// Deletes one image's files by name (blocking). The original goes first.
    pub fn remove(&self, digest: &str) -> std::io::Result<()> {
        for rendition in [Rendition::Original, Rendition::Thumb, Rendition::Full] {
            if let Some(path) = self.path(digest, rendition) {
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }
}

/// A JPEG of `data` for an agent: EXIF-rotated, long edge at most `max_edge`.
pub async fn for_agent(data: Vec<u8>, max_edge: u32) -> R<omni_image::Variant> {
    omni_image::variant(Arc::new(data), max_edge)
        .await
        .map_err(|e| image_error(&e))
}
