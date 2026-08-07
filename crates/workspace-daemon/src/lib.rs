//! Daemon internals, exposed as a library so integration tests can run the
//! full daemon in-process against a fake Hyprland.

#![warn(missing_docs)]

pub mod actor;
pub mod app;
pub mod autosave;
pub mod capture;
pub mod hypr_task;
pub mod launcher;
pub mod lock;
pub mod server;
pub mod snss;
