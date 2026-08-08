//! Daemon connection for the panel: a dedicated single-thread tokio runtime
//! on a background thread, bridged to the GTK main loop with async channels.
//!
//! Snapshots are truth: on connect and after every project-topic event the
//! client re-fetches the full snapshot and pushes the project list to the UI.
//! Events are only a trigger, which makes reconnects trivially correct.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use workspace_proto::{
    PROTOCOL_VERSION, ProjectSummary, Request, RequestEnvelope, ServerMessage, Snapshot,
};

/// Messages from the connection thread to the UI.
#[derive(Debug)]
pub enum UiUpdate {
    /// Fresh project list (connected).
    Projects(Vec<ProjectSummary>),
    /// The daemon is unreachable.
    Disconnected,
    /// A newer daemon release is known (or `None` again after an update).
    UpdateAvailable(Option<String>),
    /// Reply to `Get`: the full project definition as JSON.
    ProjectDetails(serde_json::Value),
    /// Reply to `Capture`: freshly detected app slots as JSON.
    CaptureResult(serde_json::Value),
}

/// Messages from the UI to the connection thread.
#[derive(Debug)]
pub enum UiRequest {
    /// Switch to a project by slug.
    Switch(String),
    /// Create a project with this display name.
    Create(String),
    /// Rename a project (slug stays).
    Rename {
        /// Exact project slug.
        slug: String,
        /// New display name.
        name: String,
    },
    /// Delete a project by exact slug.
    Delete(String),
    /// Capture the project's current windows (`project.save`).
    Save(String),
    /// Restore the project (adopt + launch missing).
    Restore(String),
    /// Close the project: gracefully close all of its windows.
    Close(String),
    /// Reorder projects to this exact slug order.
    Reorder(Vec<String>),
    /// Fetch a project's full definition (replied as `ProjectDetails`).
    Get(String),
    /// Preview auto-detected launch settings (replied as `CaptureResult`).
    Capture(String),
    /// Update one slot's launch settings.
    UpdateSlot {
        /// Exact project slug.
        slug: String,
        /// Slot UUID as a string.
        slot_id: String,
        /// Full launch command line ("" clears).
        command: String,
        /// Working directory ("" clears).
        workdir: String,
        /// Browser profile directory ("" clears); `None` when the field
        /// does not apply to this slot.
        profile: Option<String>,
    },
}

/// Spawn the connection thread. Returns the channel endpoints for the UI.
pub fn spawn(
    socket: Option<PathBuf>,
) -> (
    async_channel::Receiver<UiUpdate>,
    async_channel::Sender<UiRequest>,
) {
    let (update_tx, update_rx) = async_channel::unbounded();
    let (request_tx, request_rx) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("daemon-client".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            runtime.block_on(run(socket, update_tx, request_rx));
        })
        .expect("spawn client thread");
    (update_rx, request_tx)
}

async fn send(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    id: u64,
    request: Request,
) -> anyhow::Result<()> {
    let mut payload = serde_json::to_string(&RequestEnvelope {
        v: PROTOCOL_VERSION,
        id,
        request,
    })?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;
    Ok(())
}

async fn run(
    socket: Option<PathBuf>,
    updates: async_channel::Sender<UiUpdate>,
    requests: async_channel::Receiver<UiRequest>,
) {
    let path = socket.or_else(workspace_storage::paths::daemon_socket);
    let Some(path) = path else {
        tracing::error!("cannot determine daemon socket path");
        return;
    };
    loop {
        match connection(&path, &updates, &requests).await {
            Ok(()) => return, // UI dropped the channels
            Err(error) => {
                tracing::debug!(%error, "daemon connection lost");
                if updates.send(UiUpdate::Disconnected).await.is_err() {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
}

/// One connection lifetime. Returns `Ok` only when the UI side hung up.
async fn connection(
    path: &PathBuf,
    updates: &async_channel::Sender<UiUpdate>,
    requests: &async_channel::Receiver<UiRequest>,
) -> anyhow::Result<()> {
    let stream = UnixStream::connect(path).await?;
    tracing::info!("connected to workspace-daemon");
    let (read_half, mut writer) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let mut next_id: u64 = 1;

    send(
        &mut writer,
        next_id,
        Request::Subscribe {
            // `windows` feeds the open/viewing indicators (window counts
            // change on open/close/move without any project event).
            topics: Some(vec!["projects".into(), "daemon".into(), "windows".into()]),
        },
    )
    .await?;
    next_id += 1;
    send(&mut writer, next_id, Request::StateSnapshot).await?;
    // Requests whose replies the UI cares about, by request id.
    let mut pending: std::collections::HashMap<u64, Pending> = HashMap::new();
    pending.insert(next_id, Pending::Snapshot);
    next_id += 1;

    loop {
        tokio::select! {
            request = requests.recv() => {
                let Ok(request) = request else { return Ok(()) };
                let request = match request {
                    UiRequest::Switch(slug) => Request::ProjectSwitch { project: slug },
                    UiRequest::Create(name) => Request::ProjectCreate { name, slug: None },
                    UiRequest::Rename { slug, name } => Request::ProjectRename { slug, name },
                    UiRequest::Delete(slug) => Request::ProjectDelete { slug },
                    UiRequest::Save(slug) => Request::ProjectSave { project: Some(slug) },
                    UiRequest::Restore(slug) => Request::ProjectRestore {
                        project: Some(slug),
                        dry_run: false,
                    },
                    UiRequest::Close(slug) => Request::ProjectClose {
                        project: Some(slug),
                    },
                    UiRequest::Reorder(order) => Request::ProjectReorder { order },
                    UiRequest::Get(slug) => {
                        pending.insert(next_id, Pending::Details);
                        Request::ProjectGet { project: slug }
                    }
                    UiRequest::Capture(slug) => {
                        pending.insert(next_id, Pending::Capture);
                        Request::ProjectCapture { project: Some(slug) }
                    }
                    UiRequest::UpdateSlot { slug, slot_id, command, workdir, profile } => {
                        Request::SlotUpdate {
                            project: slug,
                            slot_id,
                            command: Some(command),
                            workdir: Some(workdir),
                            profile,
                        }
                    }
                };
                send(&mut writer, next_id, request).await?;
                next_id += 1;
            }
            line = lines.next_line() => {
                let Some(line) = line? else {
                    anyhow::bail!("daemon closed the connection");
                };
                match serde_json::from_str::<ServerMessage>(&line) {
                    Ok(ServerMessage::Response(response)) => {
                        let Some(kind) = pending.remove(&response.id) else { continue };
                        let Some(result) = response.result else {
                            if let Some(error) = response.error {
                                tracing::warn!(?error, "daemon rejected a panel request");
                            }
                            continue;
                        };
                        let update = match kind {
                            Pending::Snapshot => {
                                match serde_json::from_value::<Snapshot>(result) {
                                    Ok(snapshot) => {
                                        let _ = updates
                                            .send(UiUpdate::UpdateAvailable(
                                                snapshot.update_available.clone(),
                                            ))
                                            .await;
                                        UiUpdate::Projects(snapshot.projects)
                                    }
                                    Err(_) => continue,
                                }
                            }
                            Pending::Details => UiUpdate::ProjectDetails(result),
                            Pending::Capture => UiUpdate::CaptureResult(result),
                        };
                        if updates.send(update).await.is_err() {
                            return Ok(());
                        }
                    }
                    Ok(ServerMessage::Event(event)) => {
                        // Title and focus churn constantly and never change
                        // the list or the indicators — skip those; refetch
                        // for the rest unless a fetch is already in flight.
                        let noisy = matches!(
                            event.data,
                            workspace_core::DomainEvent::WindowTitleChanged { .. }
                                | workspace_core::DomainEvent::WindowFocused { .. }
                        );
                        if !noisy && !pending.values().any(|p| matches!(p, Pending::Snapshot)) {
                            send(&mut writer, next_id, Request::StateSnapshot).await?;
                            pending.insert(next_id, Pending::Snapshot);
                            next_id += 1;
                        }
                    }
                    Err(error) => {
                        tracing::debug!(%error, "unparseable line from daemon");
                    }
                }
            }
        }
    }
}

/// What the UI expects back for an in-flight request id.
enum Pending {
    Snapshot,
    Details,
    Capture,
}
