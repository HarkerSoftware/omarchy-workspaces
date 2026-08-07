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
    /// Save a project's current windows into its file.
    Save {
        /// Project query; defaults to the active project.
        project: Option<String>,
    },
    /// Restore a project: adopt matching windows, launch what is missing.
    Restore {
        /// Project query; defaults to the active project.
        project: Option<String>,
        /// Show the plan without executing it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Rules-engine helpers.
    Rules {
        #[command(subcommand)]
        cmd: RulesCmd,
    },
    /// Daemon management.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
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

#[derive(Subcommand)]
enum RulesCmd {
    /// Show which rules would match a window (no side effects).
    Test {
        /// Window address; defaults to the focused window.
        address: Option<String>,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Re-read config.toml and rules.toml without restarting.
    Reload,
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
        Command::Save { project } => commands::save(cli.socket, project).await,
        Command::Restore { project, dry_run } => {
            commands::restore(cli.socket, project, dry_run, cli.json).await
        }
        Command::Rules {
            cmd: RulesCmd::Test { address },
        } => commands::rules_test(cli.socket, address, cli.json).await,
        Command::Daemon {
            cmd: DaemonCmd::Reload,
        } => commands::daemon_reload(cli.socket).await,
        Command::Doctor => doctor::run().await,
        Command::Status => commands::status(cli.socket, cli.json).await,
        Command::Windows => commands::windows(cli.socket, cli.json).await,
        Command::Logs { lines } => commands::logs(lines),
    };
    std::process::ExitCode::from(code)
}
