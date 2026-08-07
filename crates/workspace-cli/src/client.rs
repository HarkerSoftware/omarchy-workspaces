//! NDJSON client for the daemon socket.

use std::path::PathBuf;

use anyhow::{Context, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use workspace_proto::{PROTOCOL_VERSION, Request, RequestEnvelope, ServerMessage};

/// Process exit code used when the daemon is unreachable.
pub const EXIT_DAEMON_DOWN: u8 = 3;

/// A connected daemon client.
pub struct DaemonClient {
    reader: Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    next_id: u64,
}

impl DaemonClient {
    /// Connect to the daemon socket, with an actionable error when it is down.
    pub async fn connect(socket_override: Option<PathBuf>) -> anyhow::Result<Self> {
        let path = match socket_override {
            Some(path) => path,
            None => {
                workspace_storage::paths::daemon_socket().context("XDG_RUNTIME_DIR is not set")?
            }
        };
        let stream = UnixStream::connect(&path).await.with_context(|| {
            format!(
                "daemon not running (cannot connect to {}) — start it with `workspace-daemon` \
                 or check `workspace doctor`",
                path.display()
            )
        })?;
        let (read_half, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(read_half).lines(),
            writer,
            next_id: 1,
        })
    }

    /// Send one request and wait for its response, skipping any interleaved
    /// pushed events. Returns the `result` payload or the daemon's error.
    pub async fn request(&mut self, request: Request) -> anyhow::Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;
        let envelope = RequestEnvelope {
            v: PROTOCOL_VERSION,
            id,
            request,
        };
        let mut payload = serde_json::to_string(&envelope)?;
        payload.push('\n');
        self.writer.write_all(payload.as_bytes()).await?;

        while let Some(line) = self.reader.next_line().await? {
            match serde_json::from_str::<ServerMessage>(&line) {
                Ok(ServerMessage::Response(response)) if response.id == id => {
                    if response.ok {
                        return Ok(response.result.unwrap_or(serde_json::Value::Null));
                    }
                    let error = response
                        .error
                        .context("failed response without error body")?;
                    bail!("{} ({})", error.message, error.code);
                }
                Ok(_) => continue, // event or stale response
                Err(_) => bail!("daemon sent an unparseable line: {line}"),
            }
        }
        bail!("daemon closed the connection before responding");
    }
}
