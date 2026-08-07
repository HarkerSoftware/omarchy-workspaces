//! End-to-end daemon tests: full daemon in-process against a fake Hyprland,
//! driven through the real NDJSON protocol.

use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;
use workspace_core::DomainEvent;
use workspace_core::config::Config;
use workspace_daemon::app::{self, AppOptions};
use workspace_hypr::fake::FakeHypr;
use workspace_proto::{PROTOCOL_VERSION, Request, RequestEnvelope, ServerMessage, Snapshot};

const CLIENTS_JSON: &str = r#"[{
    "address": "0xaaa1",
    "mapped": true, "hidden": false,
    "at": [0, 26], "size": [3440, 1414],
    "workspace": {"id": 1, "name": "1"},
    "floating": false, "pinned": false, "fullscreen": 0,
    "monitor": 0,
    "class": "firefox", "title": "Mozilla Firefox",
    "initialClass": "firefox", "initialTitle": "Mozilla Firefox",
    "pid": 4242, "xwayland": false,
    "grouped": [], "focusHistoryID": 0,
    "stableId": "abc123"
}]"#;

const WORKSPACES_JSON: &str = r#"[
    {"id": 1, "name": "1", "monitor": "DP-1", "monitorID": 0, "windows": 1,
     "hasfullscreen": false, "lastwindow": "0xaaa1", "ispersistent": false}
]"#;

const MONITORS_JSON: &str = r#"[
    {"id": 0, "name": "DP-1", "description": "test", "focused": true,
     "activeWorkspace": {"id": 1, "name": "1"},
     "specialWorkspace": {"id": 0, "name": ""},
     "x": 0, "y": 0, "width": 3440, "height": 1440, "scale": 1.0}
]"#;

struct TestClient {
    reader: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    writer: tokio::net::unix::OwnedWriteHalf,
    next_id: u64,
}

impl TestClient {
    async fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).await.expect("connect daemon");
        let (read_half, writer) = stream.into_split();
        Self {
            reader: BufReader::new(read_half).lines(),
            writer,
            next_id: 1,
        }
    }

    async fn request(&mut self, request: Request) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let mut payload = serde_json::to_string(&RequestEnvelope {
            v: PROTOCOL_VERSION,
            id,
            request,
        })
        .unwrap();
        payload.push('\n');
        self.writer.write_all(payload.as_bytes()).await.unwrap();
        loop {
            let line = self
                .reader
                .next_line()
                .await
                .unwrap()
                .expect("daemon closed connection");
            match serde_json::from_str::<ServerMessage>(&line).unwrap() {
                ServerMessage::Response(response) if response.id == id => {
                    assert!(response.ok, "request failed: {:?}", response.error);
                    return response.result.unwrap_or(serde_json::Value::Null);
                }
                _ => continue,
            }
        }
    }

    /// Like `request`, but returns the full envelope so tests can assert on
    /// error responses.
    async fn request_raw(&mut self, request: Request) -> workspace_proto::ResponseEnvelope {
        let id = self.next_id;
        self.next_id += 1;
        let mut payload = serde_json::to_string(&RequestEnvelope {
            v: PROTOCOL_VERSION,
            id,
            request,
        })
        .unwrap();
        payload.push('\n');
        self.writer.write_all(payload.as_bytes()).await.unwrap();
        loop {
            let line = self
                .reader
                .next_line()
                .await
                .unwrap()
                .expect("daemon closed connection");
            if let Ok(ServerMessage::Response(response)) =
                serde_json::from_str::<ServerMessage>(&line)
                && response.id == id
            {
                return response;
            }
        }
    }

    async fn next_event(&mut self) -> workspace_proto::EventEnvelope {
        loop {
            let line = self
                .reader
                .next_line()
                .await
                .unwrap()
                .expect("daemon closed connection");
            if let Ok(ServerMessage::Event(event)) = serde_json::from_str::<ServerMessage>(&line) {
                return event;
            }
        }
    }
}

/// Boot fake Hyprland + daemon in a tempdir; returns everything needed to
/// drive the pair and shut down cleanly.
async fn boot() -> (
    tempfile::TempDir,
    FakeHypr,
    CancellationToken,
    tokio::task::JoinHandle<anyhow::Result<()>>,
    std::path::PathBuf,
) {
    let dir = tempfile::tempdir().unwrap();
    let hypr_dir = dir.path().join("hypr");
    std::fs::create_dir_all(&hypr_dir).unwrap();
    let fake = FakeHypr::spawn(&hypr_dir).await.unwrap();
    fake.set_response("j/clients", CLIENTS_JSON).await;
    fake.set_response("j/workspaces", WORKSPACES_JSON).await;
    fake.set_response("j/monitors", MONITORS_JSON).await;
    fake.set_response("j/activewindow", "{}").await;

    let runtime_dir = dir.path().join("run");
    let options = AppOptions {
        hypr_paths: fake.paths.clone(),
        runtime_dir: runtime_dir.clone(),
        config: Config::default(),
    };
    let shutdown = CancellationToken::new();
    let daemon = tokio::spawn(app::run(options, shutdown.clone()));

    // Wait for the daemon socket to appear.
    let socket = runtime_dir.join("daemon.sock");
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(socket.exists(), "daemon socket never appeared");
    (dir, fake, shutdown, daemon, socket)
}

/// Poll a condition derived from fresh snapshots until it holds or times out.
async fn wait_for_snapshot(
    client: &mut TestClient,
    what: &str,
    predicate: impl Fn(&Snapshot) -> bool,
) -> Snapshot {
    for _ in 0..200 {
        let value = client.request(Request::StateSnapshot).await;
        let snapshot: Snapshot = serde_json::from_value(value).unwrap();
        if predicate(&snapshot) {
            return snapshot;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

#[tokio::test]
async fn hydrates_tracks_and_pushes_events() {
    let (_dir, fake, shutdown, daemon, socket) = boot().await;
    let mut client = TestClient::connect(&socket).await;

    // Hydration from the dump: the pre-existing firefox window is tracked,
    // with workspace and stable id intact.
    let snapshot = wait_for_snapshot(&mut client, "hydration", |s| {
        s.hypr_connected && s.windows.len() == 1
    })
    .await;
    assert_eq!(snapshot.windows[0].address, "0xaaa1");
    assert_eq!(snapshot.windows[0].facts.class, "firefox");
    assert_eq!(snapshot.windows[0].stable_id.as_deref(), Some("abc123"));
    assert_eq!(snapshot.workspaces.len(), 1);
    assert_eq!(snapshot.monitors[0].name, "DP-1");

    // Subscribe on a second connection, then open a window on the fake.
    let mut subscriber = TestClient::connect(&socket).await;
    subscriber
        .request(Request::Subscribe { topics: None })
        .await;

    // The enrichment fetch will return both windows.
    let second_window = r#"{
        "address": "0xbbb2",
        "mapped": true, "hidden": false,
        "at": [10, 30], "size": [800, 600],
        "workspace": {"id": 1, "name": "1"},
        "floating": true, "pinned": false, "fullscreen": 0,
        "monitor": 0,
        "class": "kitty", "title": "shell",
        "initialClass": "kitty", "initialTitle": "kitty",
        "pid": 555, "xwayland": false,
        "grouped": [], "focusHistoryID": 1,
        "stableId": "def456"
    }"#;
    let both = format!(
        "[{}, {}]",
        CLIENTS_JSON
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']'),
        second_window
    );
    fake.set_response("j/clients", &both).await;
    fake.emit("openwindow>>bbb2,1,kitty,shell").await;

    let event = subscriber.next_event().await;
    assert_eq!(
        event.data,
        DomainEvent::WindowOpened {
            address: "0xbbb2".into(),
            class: "kitty".into(),
            title: "shell".into(),
            workspace: "1".into(),
        }
    );
    assert!(event.seq > 0);

    // Enrichment lands: floating + geometry from the follow-up clients fetch.
    let snapshot = wait_for_snapshot(&mut client, "enrichment", |s| {
        s.windows.len() == 2
            && s.windows
                .iter()
                .any(|w| w.address == "0xbbb2" && w.facts.floating && w.facts.pid == 555)
    })
    .await;
    assert!(
        snapshot
            .windows
            .iter()
            .any(|w| w.stable_id.as_deref() == Some("def456"))
    );

    // Close it again; tracked state and events follow.
    fake.emit("closewindow>>bbb2").await;
    let event = subscriber.next_event().await;
    assert_eq!(
        event.data,
        DomainEvent::WindowClosed {
            address: "0xbbb2".into()
        }
    );
    wait_for_snapshot(&mut client, "window removed", |s| s.windows.len() == 1).await;

    // Status reflects reality.
    let status = client.request(Request::DaemonStatus).await;
    assert_eq!(status["windows"], 1);
    assert_eq!(status["hypr_connected"], true);

    shutdown.cancel();
    daemon.await.unwrap().unwrap();
    assert!(!socket.exists(), "socket cleaned up on shutdown");
}

#[tokio::test]
async fn topic_filtering_and_focus_events() {
    let (_dir, fake, shutdown, daemon, socket) = boot().await;
    let mut client = TestClient::connect(&socket).await;
    wait_for_snapshot(&mut client, "hydration", |s| s.hypr_connected).await;

    // Subscriber filtered to workspace events only.
    let mut subscriber = TestClient::connect(&socket).await;
    subscriber
        .request(Request::Subscribe {
            topics: Some(vec!["workspaces".into()]),
        })
        .await;

    // A window-topic event must NOT arrive; a workspace one must.
    fake.emit("activewindowv2>>aaa1").await;
    fake.emit("workspacev2>>2,2").await;
    let event = subscriber.next_event().await;
    assert_eq!(
        event.data,
        DomainEvent::WorkspaceChanged {
            id: 2,
            name: "2".into()
        }
    );

    // Focus landed in the snapshot even though the event was filtered out.
    let snapshot = wait_for_snapshot(&mut client, "focus", |s| {
        s.focused_window.as_deref() == Some("0xaaa1")
    })
    .await;
    assert_eq!(snapshot.focused_window.as_deref(), Some("0xaaa1"));

    shutdown.cancel();
    daemon.await.unwrap().unwrap();
}

#[tokio::test]
async fn second_instance_is_rejected() {
    let (_dir, fake, shutdown, daemon, _socket) = boot().await;

    let options = AppOptions {
        hypr_paths: fake.paths.clone(),
        runtime_dir: _dir.path().join("run"),
        config: Config::default(),
    };
    let err = app::run(options, CancellationToken::new())
        .await
        .expect_err("second instance must fail");
    assert!(err.to_string().contains("already running"), "{err}");

    shutdown.cancel();
    daemon.await.unwrap().unwrap();
}

#[tokio::test]
async fn project_lifecycle_and_switching() {
    let (_dir, fake, shutdown, daemon, socket) = boot().await;
    let mut client = TestClient::connect(&socket).await;
    wait_for_snapshot(&mut client, "hydration", |s| s.hypr_connected).await;

    // Create two projects.
    let created = client
        .request(Request::ProjectCreate {
            name: "Web Development".into(),
            slug: None,
        })
        .await;
    assert_eq!(created["slug"], "web-development");
    assert_eq!(created["workspace"], "web-development");
    client
        .request(Request::ProjectCreate {
            name: "AI Research".into(),
            slug: Some("ai".into()),
        })
        .await;

    let list = client.request(Request::ProjectList).await;
    assert_eq!(list.as_array().unwrap().len(), 2);

    // Duplicate slug is refused.
    let raw = client
        .request_raw(Request::ProjectCreate {
            name: "Другой".into(),
            slug: Some("ai".into()),
        })
        .await;
    assert!(!raw.ok);
    assert_eq!(raw.error.unwrap().code, "CONFLICT");

    // Fuzzy switch by unique prefix issues the right dispatch.
    let switched = client
        .request(Request::ProjectSwitch {
            project: "web".into(),
        })
        .await;
    assert_eq!(switched["slug"], "web-development");
    assert!(
        fake.dispatches()
            .await
            .contains(&"workspace name:web-development".to_string())
    );

    // Unknown project is a NOT_FOUND with candidates.
    let raw = client
        .request_raw(Request::ProjectSwitch {
            project: "zzz".into(),
        })
        .await;
    assert!(!raw.ok);
    let error = raw.error.unwrap();
    assert_eq!(error.code, "NOT_FOUND");
    assert!(error.data.unwrap()["candidates"].is_array());

    // Manual window assignment moves the window silently.
    let assigned = client
        .request(Request::WindowAssign {
            address: "0xaaa1".into(),
            project: "ai".into(),
            group: None,
        })
        .await;
    assert_eq!(assigned["project"], "ai");
    assert!(
        fake.dispatches()
            .await
            .contains(&"movetoworkspacesilent name:ai,address:0xaaa1".to_string())
    );
    let snapshot = wait_for_snapshot(&mut client, "assignment", |s| {
        s.projects
            .iter()
            .any(|p| p.slug.as_str() == "ai" && p.windows == 1)
    })
    .await;
    assert!(snapshot.windows[0].assignment.is_some());

    // Status reflects the active project.
    let status = client.request(Request::DaemonStatus).await;
    assert_eq!(status["active_project"], "web-development");
    assert_eq!(status["projects"], 2);

    // Deleting the assigned project clears assignments.
    client
        .request(Request::ProjectDelete { slug: "ai".into() })
        .await;
    let snapshot = wait_for_snapshot(&mut client, "deletion", |s| s.projects.len() == 1).await;
    assert!(snapshot.windows[0].assignment.is_none());

    shutdown.cancel();
    daemon.await.unwrap().unwrap();
}

#[tokio::test]
async fn workspace_events_track_active_project() {
    let (_dir, fake, shutdown, daemon, socket) = boot().await;
    let mut client = TestClient::connect(&socket).await;
    wait_for_snapshot(&mut client, "hydration", |s| s.hypr_connected).await;

    client
        .request(Request::ProjectCreate {
            name: "Gaming".into(),
            slug: None,
        })
        .await;

    // The user focuses the project workspace by hand (keybind/waybar):
    // active_project follows.
    fake.emit("workspacev2>>-100,gaming").await;
    let status = wait_for(&mut client, "manual switch tracked", |status| {
        status["active_project"] == "gaming"
    })
    .await;
    assert_eq!(status["active_project"], "gaming");

    // Focusing a numeric workspace deactivates the project.
    fake.emit("workspacev2>>1,1").await;
    wait_for(&mut client, "deactivation", |status| {
        status["active_project"].is_null()
    })
    .await;

    shutdown.cancel();
    daemon.await.unwrap().unwrap();
}

/// Poll daemon status until a predicate holds.
async fn wait_for(
    client: &mut TestClient,
    what: &str,
    predicate: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    for _ in 0..200 {
        let status = client.request(Request::DaemonStatus).await;
        if predicate(&status) {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}
