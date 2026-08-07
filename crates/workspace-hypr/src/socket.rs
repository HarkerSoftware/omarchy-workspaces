//! Discovery of the per-instance Hyprland socket paths.

use std::path::{Path, PathBuf};

use crate::HyprError;

/// Locations of the two Hyprland IPC sockets.
///
/// All connections flow through this struct so tests can point clients at a
/// fake server instead of the live compositor.
#[derive(Debug, Clone)]
pub struct HyprPaths {
    /// Request/response socket (`.socket.sock`).
    pub ctl: PathBuf,
    /// Event stream socket (`.socket2.sock`).
    pub events: PathBuf,
}

impl HyprPaths {
    /// Explicit paths (used by tests and the fake server).
    pub fn new(ctl: impl Into<PathBuf>, events: impl Into<PathBuf>) -> Self {
        Self {
            ctl: ctl.into(),
            events: events.into(),
        }
    }

    /// Paths inside an instance directory (`$XDG_RUNTIME_DIR/hypr/<signature>`).
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            ctl: dir.join(".socket.sock"),
            events: dir.join(".socket2.sock"),
        }
    }

    /// Discover the live instance from `XDG_RUNTIME_DIR` and
    /// `HYPRLAND_INSTANCE_SIGNATURE`.
    pub fn from_env() -> Result<Self, HyprError> {
        let runtime_dir =
            std::env::var_os("XDG_RUNTIME_DIR").ok_or(HyprError::MissingEnv("XDG_RUNTIME_DIR"))?;
        let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")
            .ok_or(HyprError::MissingEnv("HYPRLAND_INSTANCE_SIGNATURE"))?;
        let dir = PathBuf::from(runtime_dir).join("hypr").join(signature);
        Ok(Self::in_dir(&dir))
    }
}
