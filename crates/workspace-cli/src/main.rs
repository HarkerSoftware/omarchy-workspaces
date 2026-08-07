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
    /// Create a project.
    Create {
        /// Display name, e.g. "Web Development".
        name: String,
        /// Explicit slug (defaults to a slugified name).
        #[arg(long)]
        slug: Option<String>,
    },
    /// Delete a project (requires the exact slug).
    Delete {
        /// Exact project slug.
        slug: String,
        /// Skip the confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Rename a project's display name.
    Rename {
        /// Exact project slug.
        slug: String,
        /// New display name.
        name: String,
    },
    /// Switch to a project (slug, prefix, or fuzzy query).
    Switch {
        /// Project query.
        query: String,
    },
    /// List projects.
    List,
    /// Assign a window to a project manually.
    Assign {
        /// Window address (see `workspace windows`).
        address: String,
        /// Project query.
        project: String,
        /// Group slug within the project.
        #[arg(long)]
        group: Option<String>,
    },
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
        Command::Create { name, slug } => commands::create(cli.socket, name, slug, cli.json).await,
        Command::Delete { slug, yes } => commands::delete(cli.socket, slug, yes).await,
        Command::Rename { slug, name } => commands::rename(cli.socket, slug, name).await,
        Command::Switch { query } => commands::switch(cli.socket, query).await,
        Command::List => commands::list(cli.socket, cli.json).await,
        Command::Assign {
            address,
            project,
            group,
        } => commands::assign(cli.socket, address, project, group).await,
        Command::Doctor => doctor::run().await,
        Command::Status => commands::status(cli.socket, cli.json).await,
        Command::Windows => commands::windows(cli.socket, cli.json).await,
        Command::Logs { lines } => commands::logs(lines),
    };
    std::process::ExitCode::from(code)
}
