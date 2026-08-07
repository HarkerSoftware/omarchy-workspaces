//! Canonical on-disk locations, following the XDG base directory spec and
//! Omarchy conventions. All other crates get paths from here.

use std::path::PathBuf;

/// Application directory name used under each XDG base directory.
pub const APP_DIR: &str = "omarchy-workspaces";

fn xdg_dir(env_var: &str, home_fallback: &str) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(env_var)
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir).join(APP_DIR));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(home_fallback).join(APP_DIR))
}

/// Config directory: `~/.config/omarchy-workspaces` (holds `config.toml`, `rules.toml`).
pub fn config_dir() -> Option<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

/// State directory: `~/.local/state/omarchy-workspaces` (projects, runtime.json, logs).
pub fn state_dir() -> Option<PathBuf> {
    xdg_dir("XDG_STATE_HOME", ".local/state")
}

/// Runtime directory: `$XDG_RUNTIME_DIR/omarchy-workspaces` (daemon socket and lock).
pub fn runtime_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    if dir.is_empty() {
        return None;
    }
    Some(PathBuf::from(dir).join(APP_DIR))
}

/// Path of the daemon's IPC socket.
pub fn daemon_socket() -> Option<PathBuf> {
    Some(runtime_dir()?.join("daemon.sock"))
}

/// Path of the daemon's config file.
pub fn config_file() -> Option<PathBuf> {
    Some(config_dir()?.join("config.toml"))
}

/// Path of the rules file.
pub fn rules_file() -> Option<PathBuf> {
    Some(config_dir()?.join("rules.toml"))
}
