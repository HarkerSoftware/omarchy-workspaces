//! Core domain model for omarchy-workspaces.
//!
//! This crate is intentionally pure: no async runtime, no file or socket IO.
//! Everything here is synchronous, deterministic, and unit-testable. The
//! daemon composes these types with the Hyprland IPC layer; the CLI and panel
//! consume them through `workspace-proto`.

#![warn(missing_docs)]

pub mod config;
pub mod events;
pub mod model;
pub mod rules;
pub mod search;
pub mod world;
pub mod ws_names;

pub use events::DomainEvent;
pub use model::{AppSlot, Group, Placement, Project, ProjectId, Slug, WindowIdentity};
pub use world::{TrackedWindow, WindowFacts, World};
