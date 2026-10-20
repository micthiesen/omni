//! `/omni`: every read-only tool that needs no arguments as a JSON file.
//!
//! `/omni/parcels_list.json` holds what `parcels_list` returns with its
//! default arguments. A file is read from the tool on its first read or
//! `stat` in a run and reused for the rest of that run; listing the
//! directory reads nothing, so sizes show as 0 until a file is read. The
//! mount is read-only.

use std::collections::BTreeMap;
use std::io::{Error as IoError, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bashkit::{DirEntry, FileSystem, FileSystemExt, FileType, Metadata, async_trait};

use super::commands::Command;
use super::run::Run;

/// Where the files are mounted.
pub(crate) const MOUNT: &str = "/omni";

pub(crate) struct OmniFs {
    files: Arc<BTreeMap<String, Arc<Command>>>,
    run: Arc<Run>,
    modified: SystemTime,
}

impl OmniFs {
    pub fn new(files: Arc<BTreeMap<String, Arc<Command>>>, run: Arc<Run>, now_ms: i64) -> Self {
        let modified = SystemTime::UNIX_EPOCH
            + Duration::from_millis(u64::try_from(now_ms).unwrap_or_default());
        Self {
            files,
            run,
            modified,
        }
    }

    /// The command behind `/<name>.json`, or `None` for the directory itself.
    fn lookup(&self, path: &Path) -> bashkit::Result<Option<&Arc<Command>>> {
        let text = path.to_string_lossy();
        let name = text.trim_start_matches('/');
        if name.is_empty() {
            return Ok(None);
        }
        name.strip_suffix(".json")
            .and_then(|tool| self.files.get(tool))
            .map(Some)
            .ok_or_else(|| IoError::new(ErrorKind::NotFound, "no such file").into())
    }

    fn metadata(&self, file_type: FileType, size: u64) -> Metadata {
        Metadata {
            file_type,
            size,
            mode: if file_type == FileType::Directory {
                0o555
            } else {
                0o444
            },
            modified: self.modified,
            created: self.modified,
        }
    }

    async fn content(&self, command: &Command) -> bashkit::Result<Arc<String>> {
        self.run
            .file(command)
            .await
            .map_err(|message| IoError::other(format!("{}: {message}", command.name())).into())
    }
}

fn read_only() -> bashkit::Error {
    IoError::new(ErrorKind::PermissionDenied, "/omni is read-only").into()
}

impl FileSystemExt for OmniFs {}

#[async_trait]
impl FileSystem for OmniFs {
    async fn read_file(&self, path: &Path) -> bashkit::Result<Vec<u8>> {
        match self.lookup(path)? {
            Some(command) => Ok(self.content(command).await?.as_bytes().to_vec()),
            None => Err(IoError::other("is a directory").into()),
        }
    }

    async fn write_file(&self, _path: &Path, _content: &[u8]) -> bashkit::Result<()> {
        Err(read_only())
    }

    async fn append_file(&self, _path: &Path, _content: &[u8]) -> bashkit::Result<()> {
        Err(read_only())
    }

    async fn mkdir(&self, _path: &Path, _recursive: bool) -> bashkit::Result<()> {
        Err(read_only())
    }

    async fn remove(&self, _path: &Path, _recursive: bool) -> bashkit::Result<()> {
        Err(read_only())
    }

    async fn stat(&self, path: &Path) -> bashkit::Result<Metadata> {
        match self.lookup(path)? {
            Some(command) => {
                let size = self.content(command).await?.len();
                Ok(self.metadata(FileType::File, u64::try_from(size).unwrap_or(u64::MAX)))
            }
            None => Ok(self.metadata(FileType::Directory, 0)),
        }
    }

    async fn read_dir(&self, path: &Path) -> bashkit::Result<Vec<DirEntry>> {
        if self.lookup(path)?.is_some() {
            return Err(IoError::other("not a directory").into());
        }
        Ok(self
            .files
            .keys()
            .map(|name| DirEntry {
                name: format!("{name}.json"),
                metadata: self.metadata(FileType::File, 0),
            })
            .collect())
    }

    async fn exists(&self, path: &Path) -> bashkit::Result<bool> {
        Ok(self.lookup(path).is_ok())
    }

    async fn rename(&self, _from: &Path, _to: &Path) -> bashkit::Result<()> {
        Err(read_only())
    }

    async fn copy(&self, _from: &Path, _to: &Path) -> bashkit::Result<()> {
        Err(read_only())
    }

    async fn symlink(&self, _target: &Path, _link: &Path) -> bashkit::Result<()> {
        Err(read_only())
    }

    async fn read_link(&self, _path: &Path) -> bashkit::Result<PathBuf> {
        Err(IoError::other("not a symbolic link").into())
    }

    async fn chmod(&self, _path: &Path, _mode: u32) -> bashkit::Result<()> {
        Err(read_only())
    }
}
