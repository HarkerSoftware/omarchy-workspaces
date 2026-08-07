//! Single-instance enforcement via `flock` on a lock file.
//!
//! Holding the exclusive lock is what makes unlinking a stale daemon socket
//! safe: only the lock holder may bind the socket path.

use std::fs::File;
use std::path::Path;

use anyhow::Context;
use rustix::fs::{FlockOperation, flock};

/// Held for the daemon's lifetime; the lock releases when dropped (or when the
/// process dies, which is the point).
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

/// Try to take the exclusive daemon lock. Fails fast with a clear message when
/// another daemon instance already holds it.
pub fn acquire(path: &Path) -> anyhow::Result<InstanceLock> {
    let file = File::create(path)
        .with_context(|| format!("cannot create lock file {}", path.display()))?;
    flock(&file, FlockOperation::NonBlockingLockExclusive).map_err(|errno| {
        if errno == rustix::io::Errno::WOULDBLOCK {
            anyhow::anyhow!(
                "another workspace-daemon instance is already running (lock held on {})",
                path.display()
            )
        } else {
            anyhow::anyhow!("flock on {} failed: {errno}", path.display())
        }
    })?;
    Ok(InstanceLock { _file: file })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_while_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        let held = acquire(&path).unwrap();
        let err = acquire(&path).unwrap_err();
        assert!(err.to_string().contains("already running"), "{err}");
        drop(held);
        acquire(&path).unwrap();
    }
}
