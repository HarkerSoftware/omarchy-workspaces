//! `workspace`: CLI for omarchy-workspaces.

mod client;
mod commands;
mod doctor;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "workspace",
    version,
    about = "Workspace/project manager for Hyprland on Omarchy"
)]
struct Cli {
    /// Emit machine-readable JSON instead of human output.
    #[arg(long, global = true)]
    json: bool,

    /// Override the daemon socket path (mainly for tests).
    #[arg(long, global = true, value_name = "PATH")]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check the environment and configuration for problems.
    Doctor,
    /// Show daemon status.
    Status,
    /// List tracked windows.
    Windows,
    /// Show the daemon log tail.
    Logs {
        /// Number of lines to print.
        #[arg(short = 'n', long, default_value_t = 200)]
        lines: usize,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Doctor => doctor::run().await,
        Command::Status => commands::status(cli.socket, cli.json).await,
        Command::Windows => commands::windows(cli.socket, cli.json).await,
        Command::Logs { lines } => commands::logs(lines),
    };
    std::process::ExitCode::from(code)
}
