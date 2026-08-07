//! Async Hyprland IPC: typed request/response client (`.socket.sock`) and
//! event stream (`.socket2.sock`).
//!
//! Hand-rolled against the wire protocol rather than depending on the
//! `hyprland` crate, so models can stay tolerant of Hyprland schema drift.
//! This crate knows nothing about projects or groups.

#![warn(missing_docs)]
