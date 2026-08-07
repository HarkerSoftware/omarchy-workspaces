//! The daemon's IPC server: NDJSON over a Unix socket.
//!
//! Each connection is one task. Requests are forwarded to the state actor;
//! `subscribe` is connection-scoped and switches the connection into
//! push mode, filtered by topic. Malformed input never kills the daemon —
//! the offending connection gets an error envelope instead.

use std::collections::HashSet;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;
use workspace_proto::{
    EventEnvelope, PROTOCOL_VERSION, Request, RequestEnvelope, ResponseEnvelope, error_code,
};

use crate::actor::{ActorHandles, Command};

/// Accept connections until cancelled.
pub async fn serve(listener: UnixListener, handles: ActorHandles, shutdown: CancellationToken) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let handles = handles.clone();
                    let shutdown = shutdown.clone();
                    tokio::spawn(async move {
                        if let Err(error) = handle_connection(stream, handles, shutdown).await {
                            tracing::debug!(%error, "client connection ended with error");
                        }
                    });
                }
                Err(error) => {
                    tracing::warn!(%error, "accept failed");
                }
            },
        }
    }
    tracing::debug!("ipc server stopped");
}

async fn handle_connection(
    stream: UnixStream,
    handles: ActorHandles,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    // `None` until the client subscribes.
    let mut subscription: Option<(HashSet<String>, broadcast::Receiver<Arc<EventEnvelope>>)> = None;

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,

            line = lines.next_line() => {
                let Some(line) = line? else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let response = process_line(&line, &handles, &mut subscription).await;
                let mut payload = serde_json::to_string(&response).expect("response serializes");
                payload.push('\n');
                write_half.write_all(payload.as_bytes()).await?;
            }

            event = recv_event(&mut subscription), if subscription.is_some() => {
                match event {
                    Ok(envelope) => {
                        let wanted = subscription
                            .as_ref()
                            .is_some_and(|(topics, _)| {
                                topics.is_empty() || topics.contains(envelope.data.topic())
                            });
                        if wanted {
                            let mut payload =
                                serde_json::to_string(&*envelope).expect("event serializes");
                            payload.push('\n');
                            write_half.write_all(payload.as_bytes()).await?;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        // The client fell behind; it will notice the seq gap
                        // and re-query the snapshot.
                        tracing::debug!(skipped, "subscriber lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    Ok(())
}

/// Await the next bus event; only polled when a subscription exists.
async fn recv_event(
    subscription: &mut Option<(HashSet<String>, broadcast::Receiver<Arc<EventEnvelope>>)>,
) -> Result<Arc<EventEnvelope>, broadcast::error::RecvError> {
    match subscription {
        Some((_, receiver)) => receiver.recv().await,
        // Never polled in this state; pend forever to keep select! simple.
        None => std::future::pending().await,
    }
}

async fn process_line(
    line: &str,
    handles: &ActorHandles,
    subscription: &mut Option<(HashSet<String>, broadcast::Receiver<Arc<EventEnvelope>>)>,
) -> ResponseEnvelope {
    // Loose parse first so even malformed requests get an id-bearing reply.
    let loose: Option<serde_json::Value> = serde_json::from_str(line).ok();
    let id = loose
        .as_ref()
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let envelope: RequestEnvelope = match serde_json::from_str(line) {
        Ok(envelope) => envelope,
        Err(_) => {
            let has_method = loose.as_ref().is_some_and(|v| v.get("method").is_some());
            let (code, message) = if has_method {
                (
                    error_code::UNKNOWN_METHOD,
                    "unknown or malformed method".to_owned(),
                )
            } else {
                (
                    error_code::BAD_REQUEST,
                    "not a valid request envelope".to_owned(),
                )
            };
            return ResponseEnvelope::failure(id, code, message);
        }
    };

    if envelope.v != PROTOCOL_VERSION {
        return ResponseEnvelope::failure(
            envelope.id,
            error_code::UNSUPPORTED_VERSION,
            format!(
                "protocol version {} not supported (daemon speaks {PROTOCOL_VERSION})",
                envelope.v
            ),
        );
    }

    match envelope.request {
        Request::Subscribe { topics } => {
            let topics: HashSet<String> = topics.unwrap_or_default().into_iter().collect();
            *subscription = Some((topics.clone(), handles.bus.subscribe()));
            ResponseEnvelope::success(
                envelope.id,
                &serde_json::json!({ "subscribed": topics.into_iter().collect::<Vec<_>>() }),
            )
        }
        request => {
            let (tx, rx) = oneshot::channel();
            if handles
                .commands
                .send(Command::Request { request, resp: tx })
                .await
                .is_err()
            {
                return ResponseEnvelope::failure(
                    envelope.id,
                    error_code::INTERNAL,
                    "daemon is shutting down",
                );
            }
            match rx.await {
                Ok(Ok(result)) => ResponseEnvelope {
                    v: PROTOCOL_VERSION,
                    id: envelope.id,
                    ok: true,
                    result: Some(result),
                    error: None,
                },
                Ok(Err(error)) => ResponseEnvelope {
                    v: PROTOCOL_VERSION,
                    id: envelope.id,
                    ok: false,
                    result: None,
                    error: Some(error),
                },
                Err(_) => ResponseEnvelope::failure(
                    envelope.id,
                    error_code::INTERNAL,
                    "actor dropped the request",
                ),
            }
        }
    }
}
