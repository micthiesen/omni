//! The omni application: boot, subsystem wiring, ops routes,
//! dashboard SSE, data manager, compat audit, doctor and the preview server.
//! `main.rs` only parses the command line and runs [`app::main`].

pub mod app;
pub mod boot;
pub mod build_identity;
pub mod cli;
pub mod compat_audit;
pub mod context;
pub mod data_manager;
pub mod doctor;
pub mod json;
pub mod logging;
pub mod maintenance;
pub mod ops;
pub mod preview;
pub mod preview_stylist;
pub mod services;
pub mod stylist_cli;
pub mod task_health;
pub mod wiring;
