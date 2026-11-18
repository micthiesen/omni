//! The recipe service: one handle over the store, the image files, the change
//! feed and the ports. The operations live in the sibling modules as
//! `impl Recipes` blocks.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use omni_ai::Ai;
use omni_alerts::Pushover;
use omni_config::Config;
use omni_core::clock::SharedClock;
use omni_http::public::PublicHttpClient;
use omni_runtime::AppContext;
use omni_runtime::ports::Ports;
use omni_store::Store;

use crate::error::{R, io};
use crate::files::ImageFiles;
use crate::hub::Hub;
use crate::schema;

/// What only the running app has; absent in the operator commands.
#[derive(Clone)]
pub(crate) struct Online {
    pub ai: Ai,
    pub config: Arc<Config>,
    pub pushover: Pushover,
    pub public_http: PublicHttpClient,
}

#[derive(Clone)]
pub struct Recipes {
    pub(crate) store: Store,
    pub(crate) clock: SharedClock,
    pub(crate) tz: jiff::tz::TimeZone,
    pub(crate) dir: PathBuf,
    pub(crate) files: ImageFiles,
    pub(crate) hub: Hub,
    pub(crate) ports: Ports,
    pub(crate) online: Option<Online>,
    /// The app's origin when it is served (`RECIPES_APP_URL`).
    pub(crate) app_url: Option<String>,
    /// Held while an uploaded photo is stored and its row committed, and
    /// while the purge deletes unreferenced files, so the purge never runs
    /// between those two. Images added through the tools hold it only while
    /// the file is written; the purge's grace period for recently written
    /// files covers the gap until their row commits.
    pub(crate) files_lock: Arc<tokio::sync::Mutex<()>>,
    /// Image files younger than this are never purged.
    pub(crate) file_grace: std::time::Duration,
    ready: Arc<tokio::sync::OnceCell<()>>,
}

impl Recipes {
    /// The service over a store and a recipes directory alone (operator
    /// commands and tests): no model, no fetching, no notifications.
    pub fn new(store: Store, clock: SharedClock, tz: jiff::tz::TimeZone, dir: PathBuf) -> Self {
        Self {
            store,
            clock,
            tz,
            files: ImageFiles::new(&dir),
            dir,
            hub: Hub::default(),
            ports: Ports::default(),
            online: None,
            app_url: None,
            files_lock: Arc::new(tokio::sync::Mutex::new(())),
            file_grace: crate::tasks::FILE_GRACE,
            ready: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// The service of the running app.
    pub fn from_context(ctx: &AppContext) -> Self {
        let tz = jiff::tz::TimeZone::get(&ctx.config.tz).unwrap_or(jiff::tz::TimeZone::UTC);
        Self {
            ports: ctx.ports.clone(),
            online: Some(Online {
                ai: ctx.ai.clone(),
                config: ctx.config.clone(),
                pushover: ctx.pushover.clone(),
                public_http: ctx.public_http.clone(),
            }),
            app_url: ctx.config.recipes_app_url.clone(),
            ..Self::new(
                ctx.store.clone(),
                ctx.clock.clone(),
                tz,
                ctx.paths.recipes_dir.clone(),
            )
        }
    }

    /// Creates or migrates the tables and the directories once per process; a
    /// failure is retried on the next call.
    pub async fn ready(&self) -> R<()> {
        self.ready
            .get_or_try_init(|| async {
                self.store
                    .write(|tx| schema::migrate(tx.connection()))
                    .await?;
                let dirs = [self.files.root().to_path_buf(), self.archives_dir()];
                tokio::task::spawn_blocking(move || {
                    dirs.iter().try_for_each(std::fs::create_dir_all)
                })
                .await
                .map_err(io)?
                .map_err(io)
            })
            .await
            .map(|_| ())
    }

    /// The same service with another grace period for unreferenced image
    /// files (tests purge without waiting a day).
    pub fn with_file_grace(mut self, grace: std::time::Duration) -> Self {
        self.file_grace = grace;
        self
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn hub(&self) -> &Hub {
        &self.hub
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn files(&self) -> &ImageFiles {
        &self.files
    }

    pub fn archives_dir(&self) -> PathBuf {
        self.dir.join("archives")
    }

    pub(crate) fn now(&self) -> i64 {
        self.clock.now_ms()
    }
}
