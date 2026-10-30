use super::*;
use anyhow::{Result, ensure};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
fn value() -> Snapshot {
    Snapshot {
        completion_reserved: 0,
        accounted: 37_000,
        ingress_reserved: 0,
        potential_unknown: 0,
        live_unknown: 1,
        active: 0,
        queued: 0,
        ingress: 0,
        closed: false,
        fenced: false,
    }
}
#[test]
fn callbacks_can_straddle_seal_without_stitching_current() -> Result<()> {
    let observer = Observation::default();
    let mut final_value = value();
    final_value.closed = true;
    let source = Arc::new(Mutex::new(value()));
    let reads = Arc::new(AtomicUsize::new(0));
    let retained = source.clone();
    let count = reads.clone();
    ensure!(observer.bind(value(), move || {
        count.fetch_add(1, Ordering::Relaxed);
        retained
            .try_lock()
            .map(|v| *v)
            .map_err(|_| ReadError::Unavailable)
    }));
    let before = observer.collect();
    ensure!(before[1] == Some(("current", Ok(value()))));
    *source.lock().map_err(|_| anyhow::anyhow!("source"))? = final_value;
    ensure!(observer.seal(true));
    let after = observer.collect();
    ensure!(after[1] == Some(("final", Ok(final_value))));
    // The old current is not relabelled. Future callbacks need no live owner.
    ensure!(before[1] != after[1]);
    let count = reads.load(Ordering::Relaxed);
    drop(source);
    ensure!(observer.seal(false));
    ensure!(observer.collect() == after && reads.load(Ordering::Relaxed) == count);
    ensure!(after[1].is_some_and(|(_, v)| v.is_ok_and(|v| v.live_unknown == 1)));
    Ok(())
}
#[test]
fn callback_contention_is_partial_and_failed_final_is_sticky() -> Result<()> {
    for blocked_first in [true, false] {
        let observer = Observation::default();
        let source = Arc::new(Mutex::new(value()));
        let weak = Arc::downgrade(&source);
        ensure!(observer.bind(value(), move || {
            weak.upgrade()
                .ok_or(ReadError::Unavailable)?
                .try_lock()
                .map(|v| *v)
                .map_err(|_| ReadError::Unavailable)
        }));
        let free = observer.collect();
        let held = source.lock().map_err(|_| anyhow::anyhow!("source"))?;
        let blocked = observer.collect();
        ensure!(blocked[0] == free[0]);
        ensure!(blocked[1] == Some(("current", Err(ReadError::Unavailable))));
        let callbacks = if blocked_first {
            [blocked, free]
        } else {
            [free, blocked]
        };
        ensure!(
            callbacks
                .iter()
                .filter(|c| c[1].is_some_and(|(_, v)| v.is_ok()))
                .count()
                == 1
        );
        ensure!(!observer.seal(true));
        drop(held);
        ensure!(!observer.seal(true));
        ensure!(observer.collect()[1] == Some(("final", Err(ReadError::Unavailable))));
    }
    Ok(())
}
#[test]
fn unbound_unclean_active_and_out_of_range_cannot_freeze_success() -> Result<()> {
    ensure!(!Observation::default().seal(true));
    for (clean, active) in [(false, 0), (true, 1)] {
        let observer = Observation::default();
        let mut v = value();
        v.closed = true;
        v.active = active;
        ensure!(observer.bind(value(), move || Ok(v)));
        ensure!(!observer.seal(clean));
    }
    let mut bad = value();
    bad.accounted = i64::MAX as u64 + 1;
    ensure!(bad.validate() == Err(ReadError::Invalid));
    bad = value();
    bad.potential_unknown = 1;
    bad.active = 1;
    ensure!(bad.validate() == Err(ReadError::Invalid));
    Ok(())
}
