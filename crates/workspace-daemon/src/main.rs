//! `workspace-daemon`: the omarchy-workspaces background daemon.
//!
//! Milestone M2 fills this in with the state actor, Hyprland event pump, and
//! IPC server. For now it only reports its version so the workspace builds
//! end-to-end.

fn main() -> anyhow::Result<()> {
    println!(
        "workspace-daemon {} (not yet functional)",
        env!("CARGO_PKG_VERSION")
    );
    Ok(())
}
