//! `workspace`: CLI for omarchy-workspaces.
//!
//! The command tree grows milestone by milestone; `doctor` (M1) is the first
//! real surface.

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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Doctor => {
            println!("workspace doctor: not yet implemented (lands in M1)");
        }
    }
    Ok(())
}
