//! Daemon wiring: lock, socket, tasks, graceful shutdown.
//!
//! `run` is a library function taking explicit paths so integration tests can
//! boot the whole daemon against a fake Hyprland in a tempdir.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::Context;
use tokio_util::sync::CancellationToken;
use workspace_core::config::Config;
use workspace_hypr::HyprPaths;

use crate::actor::{self, Command};
use crate::{hypr_task, lock, server};

/// Everything the daemon needs to start; built from the environment in `main`
/// or from tempdirs in tests.
#[derive(Debug, Clone)]
pub struct AppOptions {
    /// Hyprland socket paths.
    pub hypr_paths: HyprPaths,
    /// Directory for the daemon socket and lock file (created, mode 0700).
    pub runtime_dir: PathBuf,
    /// Parsed configuration.
    pub config: Config,
    /// Config directory holding `config.toml`/`rules.toml`; enables rules
    /// loading and `config.reload`. `None` disables both.
    pub config_dir: Option<PathBuf>,
    /// State directory for projects/runtime persistence. `None` disables it.
    pub state_dir: Option<PathBuf>,
}

/// Run the daemon until `shutdown` is cancelled. Returns after cleanup.
pub async fn run(options: AppOptions, shutdown: CancellationToken) -> anyhow::Result<()> {
    std::fs::create_dir_all(&options.runtime_dir).with_context(|| {
        format!(
            "cannot create runtime dir {}",
            options.runtime_dir.display()
        )
    })?;
    std::fs::set_permissions(&options.runtime_dir, std::fs::Permissions::from_mode(0o700))?;

    let _lock = lock::acquire(&options.runtime_dir.join("daemon.lock"))?;

    // Holding the lock makes removing a stale socket safe.
    let socket_path = options.runtime_dir.join("daemon.sock");
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }
    let listener = tokio::net::UnixListener::bind(&socket_path)
        .with_context(|| format!("cannot bind {}", socket_path.display()))?;
    tracing::info!(socket = %socket_path.display(), "daemon listening");

    let handles = actor::spawn(
        options.config.clone(),
        workspace_hypr::HyprCtl::new(options.hypr_paths.clone()),
        options.config_dir.clone(),
        options.state_dir.clone(),
    );

    tokio::spawn(crate::autosave::run(
        options.config.autosave.clone(),
        handles.bus.subscribe(),
        handles.commands.clone(),
    ));

    let hypr = tokio::spawn(hypr_task::run(
        options.hypr_paths.clone(),
        handles.commands.clone(),
    ));

    server::serve(listener, handles.clone(), shutdown.clone()).await;

    // Graceful shutdown: tell subscribers, stop the actor, then clean up.
    let _ = handles.commands.send(Command::Shutdown).await;
    hypr.abort();
    let _ = std::fs::remove_file(&socket_path);
    tracing::info!("daemon stopped");
    Ok(())
}
