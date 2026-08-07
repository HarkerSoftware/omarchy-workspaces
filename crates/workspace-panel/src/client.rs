//! Daemon connection for the panel: a dedicated single-thread tokio runtime
//! on a background thread, bridged to the GTK main loop with async channels.
//!
//! Snapshots are truth: on connect and after every project-topic event the
//! client re-fetches the full snapshot and pushes the project list to the UI.
//! Events are only a trigger, which makes reconnects trivially correct.

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
    let (read_half, mut writer) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let mut next_id: u64 = 1;

    send(
        &mut writer,
        next_id,
        Request::Subscribe {
            topics: Some(vec!["projects".into(), "daemon".into()]),
        },
    )
    .await?;
    next_id += 1;
    send(&mut writer, next_id, Request::StateSnapshot).await?;
    // Id of the snapshot request we are waiting for, if any.
    let mut pending_snapshot: Option<u64> = Some(next_id);
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
                        if pending_snapshot == Some(response.id)
                            && let Some(result) = response.result
                        {
                            pending_snapshot = None;
                            if let Ok(snapshot) = serde_json::from_value::<Snapshot>(result)
                                && updates
                                    .send(UiUpdate::Projects(snapshot.projects))
                                    .await
                                    .is_err()
                            {
                                return Ok(());
                            }
                        }
                    }
                    Ok(ServerMessage::Event(_)) => {
                        // Any pushed event on our topics may change the list;
                        // refetch unless one is already in flight.
                        if pending_snapshot.is_none() {
                            send(&mut writer, next_id, Request::StateSnapshot).await?;
                            pending_snapshot = Some(next_id);
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
