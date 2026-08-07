//! Async Hyprland IPC: typed request/response client (`.socket.sock`) and
//! event stream (`.socket2.sock`).
//!
//! Hand-rolled against the wire protocol rather than depending on the
//! `hyprland` crate, so models can stay tolerant of Hyprland schema drift.
//! This crate knows nothing about projects or groups.
//!
//! - [`HyprPaths`] discovers the per-instance socket directory.
//! - [`HyprCtl`] issues requests (`j/clients`, `dispatch …`) over the control
//!   socket, one connection per request, exactly like `hyprctl`.
//! - [`events`] parses the newline-delimited event stream and reconnects with
//!   backoff.
//! - The `fake` feature provides a scripted in-process Hyprland for tests.

#![warn(missing_docs)]

pub mod ctl;
pub mod events;
pub mod model;
pub mod socket;

#[cfg(any(test, feature = "fake"))]
pub mod fake;

pub use ctl::{Dispatch, HyprCtl, WsTarget};
pub use events::{HyprEvent, StreamItem};
pub use model::{Client, Monitor, WindowAddress, Workspace, WorkspaceRef};
pub use socket::HyprPaths;

/// Errors from talking to Hyprland.
#[derive(Debug, thiserror::Error)]
pub enum HyprError {
    /// Environment variables needed to locate the sockets are missing.
    #[error("cannot locate Hyprland sockets: {0} is not set (is Hyprland running?)")]
    MissingEnv(&'static str),
    /// Socket-level IO failure.
    #[error("Hyprland IPC error: {0}")]
    Io(#[from] std::io::Error),
    /// Hyprland returned JSON we could not decode.
    #[error("unexpected reply to {request:?}: {source}")]
    Decode {
        /// The request that produced the reply.
        request: String,
        /// The decode failure.
        #[source]
        source: serde_json::Error,
    },
    /// A dispatcher was rejected by Hyprland.
    #[error("dispatch {request:?} failed: {reply}")]
    DispatchFailed {
        /// The rejected dispatch command.
        request: String,
        /// Hyprland's error reply.
        reply: String,
    },
}
