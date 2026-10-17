//! Leptos CSR kit for the Omni frontend.
//!
//! `omni-web` (router, shell, ops pages) and `omni-web-pages` (domain pages)
//! build on these modules: the API client, live dashboard data, hooks,
//! components, SVG charts, Markdown and pure display utilities.

pub mod api;
pub mod charts;
pub mod chrome;
pub mod components;
pub mod feeds;
pub mod hooks;
pub mod live;
pub mod markdown;
pub mod router;
pub mod sse;
pub mod task;
pub mod utils;

pub use live::use_live_data;
