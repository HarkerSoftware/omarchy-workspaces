//! `workspace-daemon`: the omarchy-workspaces background daemon.

use anyhow::Context;
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use workspace_core::config::Config;
use workspace_daemon::app::{self, AppOptions};
use workspace_hypr::HyprPaths;

#[derive(Parser)]
#[command(
    name = "workspace-daemon",
    version,
    about = "omarchy-workspaces daemon"
)]
struct Args {
    /// Log to stderr only; skip the state-dir log file.
    #[arg(long)]
    no_log_file: bool,
}

fn load_config() -> anyhow::Result<Config> {
    let Some(path) = workspace_storage::paths::config_file() else {
        return Ok(Config::default());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            Config::parse(&text).with_context(|| format!("invalid config {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn init_tracing(
    config: &Config,
    no_log_file: bool,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(config.log.level.clone()));
    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    let mut guard = None;
    let file_layer = if no_log_file {
        None
    } else {
        workspace_storage::paths::state_dir().and_then(|state| {
            let logs = state.join("logs");
            std::fs::create_dir_all(&logs).ok()?;
            let appender = tracing_appender::rolling::never(logs, "daemon.log");
            let (writer, g) = tracing_appender::non_blocking(appender);
            guard = Some(g);
            Some(
                tracing_subscriber::fmt::layer()
                    .with_writer(writer)
                    .with_ansi(false),
            )
        })
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();
    guard
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = load_config()?;
    let _log_guard = init_tracing(&config, args.no_log_file);

    let options = AppOptions {
        hypr_paths: HyprPaths::from_env()?,
        runtime_dir: workspace_storage::paths::runtime_dir()
            .context("XDG_RUNTIME_DIR is not set")?,
        config,
        config_dir: workspace_storage::paths::config_dir(),
        state_dir: workspace_storage::paths::state_dir(),
    };

    let shutdown = CancellationToken::new();
    let signal_token = shutdown.clone();
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
        tracing::info!("shutdown signal received");
        signal_token.cancel();
    });

    app::run(options, shutdown).await
}
