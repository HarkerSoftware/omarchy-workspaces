//! `workspace`: CLI for omarchy-workspaces.
//!
//! The command tree grows milestone by milestone; `doctor` (M1) is the first
//! real surface.

mod doctor;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "workspace",
    version,
    about = "Workspace/project manager for Hyprland on Omarchy"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check the environment and configuration for problems.
    Doctor,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Doctor => doctor::run().await,
    };
    std::process::ExitCode::from(code)
}
