//! A scripted in-process Hyprland for integration tests.
//!
//! Speaks the real wire protocols on both sockets: scripted JSON replies on
//! the control socket, an injectable event stream on the event socket, and a
//! recorder for every dispatch it receives. Enabled with the `fake` feature
//! (or automatically in this crate's own tests).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::socket::HyprPaths;

#[derive(Default)]
struct FakeState {
    /// Scripted replies keyed by exact request string (e.g. `"j/clients"`).
    responses: HashMap<String, String>,
    /// Every dispatcher received, in order, batch entries flattened.
    dispatches: Vec<String>,
    /// Connected event-stream clients.
    event_clients: Vec<UnixStream>,
}

/// Handle to a running fake Hyprland instance.
#[derive(Debug)]
pub struct FakeHypr {
    /// Socket paths to hand to the code under test.
    pub paths: HyprPaths,
    state: Arc<Mutex<FakeState>>,
    ctl_task: JoinHandle<()>,
    events_task: JoinHandle<()>,
}

impl std::fmt::Debug for FakeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeState")
            .field("dispatches", &self.dispatches.len())
            .field("event_clients", &self.event_clients.len())
            .finish()
    }
}

impl FakeHypr {
    /// Bind both sockets inside `dir` (typically a tempdir) and start serving.
    pub async fn spawn(dir: &Path) -> std::io::Result<Self> {
        let paths = HyprPaths::in_dir(dir);
        let ctl_listener = UnixListener::bind(&paths.ctl)?;
        let events_listener = UnixListener::bind(&paths.events)?;
        let state = Arc::new(Mutex::new(FakeState::default()));

        let ctl_state = Arc::clone(&state);
        let ctl_task = tokio::spawn(async move {
            while let Ok((stream, _)) = ctl_listener.accept().await {
                let state = Arc::clone(&ctl_state);
                tokio::spawn(handle_ctl_connection(stream, state));
            }
        });

        let events_state = Arc::clone(&state);
        let events_task = tokio::spawn(async move {
            while let Ok((stream, _)) = events_listener.accept().await {
                events_state.lock().await.event_clients.push(stream);
            }
        });

        Ok(Self {
            paths,
            state,
            ctl_task,
            events_task,
        })
    }

    /// Script the reply for an exact request string.
    pub async fn set_response(&self, request: &str, reply: &str) {
        self.state
            .lock()
            .await
            .responses
            .insert(request.to_owned(), reply.to_owned());
    }

    /// All dispatcher strings received so far (batch entries flattened).
    pub async fn dispatches(&self) -> Vec<String> {
        self.state.lock().await.dispatches.clone()
    }

    /// Send one event line (without trailing newline) to all event clients.
    pub async fn emit(&self, line: &str) {
        let mut state = self.state.lock().await;
        let payload = format!("{line}\n");
        let mut alive = Vec::new();
        for mut client in state.event_clients.drain(..) {
            if client.write_all(payload.as_bytes()).await.is_ok() {
                alive.push(client);
            }
        }
        state.event_clients = alive;
    }

    /// Drop all event-stream connections (simulates a Hyprland restart).
    pub async fn drop_event_clients(&self) {
        self.state.lock().await.event_clients.clear();
    }

    /// Number of currently connected event clients.
    pub async fn event_client_count(&self) -> usize {
        self.state.lock().await.event_clients.len()
    }
}

impl Drop for FakeHypr {
    fn drop(&mut self) {
        self.ctl_task.abort();
        self.events_task.abort();
    }
}

/// Serve one control-socket connection: read the request, reply, close —
/// mirroring Hyprland's one-request-per-connection behavior.
async fn handle_ctl_connection(mut stream: UnixStream, state: Arc<Mutex<FakeState>>) {
    let mut buf = vec![0u8; 8192];
    let Ok(n) = stream.read(&mut buf).await else {
        return;
    };
    let request = String::from_utf8_lossy(&buf[..n]).into_owned();

    let reply = {
        let mut state = state.lock().await;
        if let Some(batch) = request.strip_prefix("[[BATCH]]") {
            let mut oks = Vec::new();
            for cmd in batch.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                if let Some(dispatch) = cmd.strip_prefix("dispatch ") {
                    state.dispatches.push(dispatch.to_owned());
                }
                oks.push("ok");
            }
            oks.join("\n")
        } else if let Some(dispatch) = request.strip_prefix("dispatch ") {
            state.dispatches.push(dispatch.to_owned());
            "ok".to_owned()
        } else if let Some(reply) = state.responses.get(&request) {
            reply.clone()
        } else {
            format!("unknown request: {request}")
        }
    };

    let _ = stream.write_all(reply.as_bytes()).await;
    // Dropping the stream closes the connection, which is what signals
    // end-of-reply to the client.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctl::{Dispatch, HyprCtl, WsTarget};
    use crate::events::{HyprEvent, StreamItem, run_event_pump};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn ctl_round_trip_and_dispatch_recording() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeHypr::spawn(dir.path()).await.unwrap();
        fake.set_response("j/clients", "[]").await;

        let ctl = HyprCtl::new(fake.paths.clone());
        assert!(ctl.clients().await.unwrap().is_empty());

        ctl.dispatch(&Dispatch::Workspace(WsTarget::Name("web-dev".into())))
            .await
            .unwrap();
        ctl.dispatch_batch(&[
            Dispatch::Workspace(WsTarget::Id(1)),
            Dispatch::Workspace(WsTarget::Id(2)),
        ])
        .await
        .unwrap();

        assert_eq!(
            fake.dispatches().await,
            vec!["workspace name:web-dev", "workspace 1", "workspace 2"]
        );
    }

    #[tokio::test]
    async fn undecodable_reply_is_a_decode_error() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeHypr::spawn(dir.path()).await.unwrap();
        fake.set_response("j/clients", "this is not json").await;

        let ctl = HyprCtl::new(fake.paths.clone());
        let err = ctl.clients().await.unwrap_err();
        assert!(matches!(err, crate::HyprError::Decode { .. }));
    }

    #[tokio::test]
    async fn event_pump_delivers_and_reconnects() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeHypr::spawn(dir.path()).await.unwrap();

        let (tx, mut rx) = mpsc::channel(64);
        let pump = tokio::spawn(run_event_pump(fake.paths.clone(), tx));

        assert_eq!(rx.recv().await, Some(StreamItem::Connected));
        // Wait for the fake to register the client before emitting.
        while fake.event_client_count().await == 0 {
            tokio::task::yield_now().await;
        }

        fake.emit("workspacev2>>-1337,web-dev").await;
        assert_eq!(
            rx.recv().await,
            Some(StreamItem::Event(HyprEvent::Workspace {
                id: -1337,
                name: "web-dev".into()
            }))
        );

        // Simulate Hyprland dropping the connection: pump reports it and
        // reconnects on its own.
        fake.drop_event_clients().await;
        assert_eq!(rx.recv().await, Some(StreamItem::Disconnected));
        assert_eq!(rx.recv().await, Some(StreamItem::Connected));

        while fake.event_client_count().await == 0 {
            tokio::task::yield_now().await;
        }
        fake.emit("configreloaded>>").await;
        assert_eq!(
            rx.recv().await,
            Some(StreamItem::Event(HyprEvent::ConfigReloaded))
        );

        pump.abort();
    }
}
