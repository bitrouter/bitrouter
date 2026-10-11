//! Byte admission shared by every live transport generation of a session.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use serde::Serialize;
use tokio::sync::Notify;

use super::{CoreError, ErrorCode};

#[derive(Default)]
pub(super) struct Budget {
    ledger: Mutex<Ledger>,
    changed: Notify,
}

#[derive(Default)]
struct Ledger {
    next: u64,
    scopes: BTreeMap<u64, Account>,
}

struct Account {
    session_limit: usize,
    run: Option<String>,
    run_limit: usize,
    used: usize,
}

pub(super) struct Scope {
    budget: Arc<Budget>,
    id: u64,
}

pub(super) struct Lease {
    scope: Arc<Scope>,
    bytes: usize,
}

impl Budget {
    pub(super) fn scope(
        self: &Arc<Self>,
        session_limit: u64,
        run: Option<(&str, u64)>,
    ) -> Result<Arc<Scope>, CoreError> {
        let session_limit = usize::try_from(session_limit).map_err(|_| limit())?;
        let run_limit = usize::try_from(run.map_or(session_limit as u64, |(_, cap)| cap))
            .map_err(|_| limit())?;
        if session_limit == 0 || run_limit == 0 {
            return Err(limit());
        }
        let mut ledger = self
            .ledger
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let used: usize = ledger.scopes.values().map(|account| account.used).sum();
        let run_used: usize = ledger
            .scopes
            .values()
            .filter(|account| run.is_some_and(|(id, _)| account.run.as_deref() == Some(id)))
            .map(|account| account.used)
            .sum();
        if used > session_limit || run_used > run_limit {
            return Err(CoreError::rejected(
                ErrorCode::Busy,
                "retained output exceeds the requested transport limit",
            ));
        }
        let id = ledger.next.checked_add(1).ok_or_else(limit)?;
        ledger.next = id;
        ledger.scopes.insert(
            id,
            Account {
                session_limit,
                run: run.map(|(id, _)| id.to_owned()),
                run_limit,
                used: 0,
            },
        );
        Ok(Arc::new(Scope {
            budget: self.clone(),
            id,
        }))
    }
}

impl Scope {
    pub(super) fn budget(&self) -> Arc<Budget> {
        self.budget.clone()
    }

    /// Exact reservations admit a whole WebSocket message; chunk reservations
    /// use currently available capacity without waiting for an entire frame.
    pub(super) async fn reserve(
        self: &Arc<Self>,
        bytes: usize,
        exact: bool,
    ) -> Result<Lease, CoreError> {
        loop {
            let changed = self.budget.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut ledger = self
                    .budget
                    .ledger
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let own = ledger.scopes.get(&self.id).ok_or_else(limit)?;
                let session_limit = ledger
                    .scopes
                    .values()
                    .map(|account| account.session_limit)
                    .min()
                    .ok_or_else(limit)?;
                let used: usize = ledger.scopes.values().map(|account| account.used).sum();
                let (run_limit, run_used) = if let Some(run) = &own.run {
                    let matching = || {
                        ledger
                            .scopes
                            .values()
                            .filter(|account| account.run.as_ref() == Some(run))
                    };
                    (
                        matching()
                            .map(|account| account.run_limit)
                            .min()
                            .ok_or_else(limit)?,
                        matching().map(|account| account.used).sum(),
                    )
                } else {
                    (session_limit, used)
                };
                if exact && bytes > session_limit.min(run_limit) {
                    return Err(limit());
                }
                let available = session_limit
                    .saturating_sub(used)
                    .min(run_limit.saturating_sub(run_used));
                let admitted = if exact { bytes } else { bytes.min(available) };
                if admitted <= available && (admitted > 0 || bytes == 0) {
                    ledger.scopes.get_mut(&self.id).ok_or_else(limit)?.used += admitted;
                    return Ok(Lease {
                        scope: self.clone(),
                        bytes: admitted,
                    });
                }
            }
            changed.await;
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        self.budget
            .ledger
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .scopes
            .remove(&self.id);
        self.budget.changed.notify_waiters();
    }
}

impl Lease {
    pub(super) fn len(&self) -> usize {
        self.bytes
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(account) = self
            .scope
            .budget
            .ledger
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .scopes
            .get_mut(&self.scope.id)
        {
            account.used -= self.bytes;
        }
        self.scope.budget.changed.notify_waiters();
    }
}

struct OwnedBytes {
    bytes: Vec<u8>,
    _lease: Lease,
}

impl AsRef<[u8]> for OwnedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

// The owner survives downstream Bytes clones and WebSocket conversions.
// <https://docs.rs/bytes/latest/bytes/struct.Bytes.html#method.from_owner>
pub(super) fn retain(bytes: Vec<u8>, lease: Lease) -> Bytes {
    Bytes::from_owner(OwnedBytes {
        bytes,
        _lease: lease,
    })
}

fn limit() -> CoreError {
    CoreError::rejected(
        ErrorCode::LimitExceeded,
        "output exceeds negotiated transport capacity",
    )
}

/// Count before allocating a non-chunkable message. serde writes strings and
/// base64 directly to this sink, without a second encoded copy.
pub(super) fn encoded_len(value: &impl Serialize, bound: u64) -> Result<usize, CoreError> {
    struct Counter {
        bytes: usize,
        bound: u64,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let total = self
                .bytes
                .checked_add(bytes.len())
                .ok_or_else(|| io::Error::other("output overflow"))?;
            if total as u64 > self.bound {
                return Err(io::Error::other("output limit"));
            }
            self.bytes = total;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { bytes: 0, bound };
    serde_json::to_writer(&mut counter, value).map_err(|_| limit())?;
    Ok(counter.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    #[tokio::test]
    async fn output_budget_combines_runs_consumers_and_unattributed_frames()
    -> Result<(), Box<dyn std::error::Error>> {
        let budget = Arc::new(Budget::default());
        let channel = budget.scope(1024, None)?;
        let first = budget.scope(1024, Some(("run", 256)))?;
        let replay = budget.scope(1024, Some(("run", 256)))?;
        let other = budget.scope(1024, Some(("other", 512)))?;
        let a = first.reserve(128, true).await?;
        let b = replay.reserve(128, true).await?;
        assert!(replay.reserve(1, true).now_or_never().is_none());
        let control = channel.reserve(512, true).await?;
        let c = other.reserve(512, false).await?;
        assert_eq!(c.len(), 256);
        assert!(channel.reserve(1, false).now_or_never().is_none());
        drop(a);
        let fourth = replay.reserve(256, false).await?;
        assert_eq!(fourth.len(), 128);
        drop((b, control, c, fourth));
        assert_eq!(channel.reserve(1024, true).await?.len(), 1024);
        Ok(())
    }

    #[tokio::test]
    async fn retained_bytes_fence_lower_limits_and_wake_waiters()
    -> Result<(), Box<dyn std::error::Error>> {
        let budget = Arc::new(Budget::default());
        let old = budget.scope(256, None)?;
        let bytes = retain(vec![7; 256], old.reserve(256, true).await?);
        let retained = bytes.clone();
        drop(bytes);
        assert!(matches!(budget.scope(64, None), Err(error) if error.code == ErrorCode::Busy));
        let waiting = old.reserve(64, true);
        tokio::pin!(waiting);
        assert!(waiting.as_mut().now_or_never().is_none());
        drop(retained);
        let lease = tokio::time::timeout(std::time::Duration::from_secs(5), waiting).await??;
        let new = budget.scope(64, None)?;
        assert!(old.reserve(1, false).now_or_never().is_none());
        drop(lease);
        assert_eq!(old.reserve(256, false).await?.len(), 64);
        assert!(
            matches!(old.reserve(65, true).await, Err(error) if error.code == ErrorCode::LimitExceeded)
        );
        drop(new);
        assert_eq!(old.reserve(256, true).await?.len(), 256);
        Ok(())
    }
}
