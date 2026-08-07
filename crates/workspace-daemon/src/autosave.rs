//! Debounced autosave: watches the event bus and asks the actor to persist
//! the runtime snapshot after things quiet down (or at a maximum interval
//! while events keep arriving).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use workspace_core::DomainEvent;
use workspace_core::config::Autosave;
use workspace_proto::EventEnvelope;

use crate::actor::Command;

fn relevant(event: &DomainEvent) -> bool {
    !matches!(
        event,
        DomainEvent::HyprConnection { .. } | DomainEvent::ShuttingDown
    )
}

/// Run until the bus closes.
pub async fn run(
    settings: Autosave,
    mut bus: broadcast::Receiver<Arc<EventEnvelope>>,
    commands: mpsc::Sender<Command>,
) {
    if !settings.enabled {
        return;
    }
    let debounce = Duration::from_millis(settings.debounce_ms);
    let max_interval = Duration::from_secs(settings.interval_s);
    let mut dirty_since: Option<Instant> = None;
    let mut deadline: Option<Instant> = None;

    loop {
        let sleep_until = deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
        tokio::select! {
            received = bus.recv() => match received {
                Ok(envelope) => {
                    if relevant(&envelope.data) {
                        let now = Instant::now();
                        let first_dirty = *dirty_since.get_or_insert(now);
                        // Debounce, but never postpone past first_dirty + max.
                        deadline = Some((now + debounce).min(first_dirty + max_interval));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = tokio::time::sleep_until(sleep_until), if deadline.is_some() => {
                dirty_since = None;
                deadline = None;
                if commands.send(Command::Persist).await.is_err() {
                    break;
                }
            }
        }
    }
}
