//! Wire protocol between the daemon and its clients (CLI, panel).
//!
//! Transport: a Unix socket at `$XDG_RUNTIME_DIR/omarchy-workspaces/daemon.sock`
//! carrying newline-delimited JSON. Requests carry an `id` echoed in the
//! response; subscribed clients additionally receive pushed events with a
//! monotonic `seq`. See `docs/protocol.md`.

#![warn(missing_docs)]

/// Protocol major version; the daemon rejects envelopes with a different major.
pub const PROTOCOL_VERSION: u32 = 1;
