use super::*;
use crate::telemetry::receipt_latency::capacity::ReadError;
use anyhow::{Result, ensure};
#[test]
fn observation_never_recovers_poison_or_mutates_locked_admission() -> Result<()> {
    let admission = Admission::new();
    let mut held = admission
        .state
        .lock()
        .map_err(|_| anyhow::anyhow!("state"))?;
    held.ingress = 1;
    held.ingress_bytes = 16 * 1024 * 1024;
    ensure!(admission.observed_capacity() == Err(ReadError::Unavailable));
    ensure!(!held.closed && held.ingress == 1);
    drop(held);
    let observed = admission
        .observed_capacity()
        .map_err(|_| anyhow::anyhow!("observation"))?;
    ensure!(observed.ingress == 1 && observed.ingress_reserved == 16 * 1024 * 1024);
    let owner = admission.clone();
    let poisoned = std::thread::spawn(move || {
        if let Ok(_held) = owner.state.lock() {
            std::panic::resume_unwind(Box::new("intentional mutex poison"));
        }
    })
    .join();
    ensure!(poisoned.is_err());
    ensure!(admission.observed_capacity() == Err(ReadError::Unavailable));
    match admission.state.lock() {
        Err(guard) => ensure!(!guard.into_inner().closed),
        Ok(_) => anyhow::bail!("expected poison"),
    }
    Ok(())
}
#[test]
fn real_inventory_unknown_survives_escalation_and_policy_bytes_are_not_reserve() -> Result<()> {
    let admission = Admission::new();
    admission
        .initialize_receipts(
            super::super::receipts::capacity::Inventory {
                bytes: 123,
                rows: 1,
                unknown: vec![(
                    "tenant".try_into()?,
                    "execution".try_into()?,
                    chrono::Utc::now() - chrono::Duration::minutes(16),
                )],
            },
            1024 * 1024 * 1024,
        )
        .map_err(|_| anyhow::anyhow!("inventory"))?;
    admission.escalate_receipts();
    let value = admission
        .observed_capacity()
        .map_err(|_| anyhow::anyhow!("observation"))?;
    ensure!(
        value.live_unknown == 1 && value.potential_unknown == 0 && value.completion_reserved == 0
    );
    ensure!(value.accounted == 123 + 16 * 1024);
    Ok(())
}

#[test]
fn independent_callback_schedules_use_the_real_admission_mutex() -> Result<()> {
    use crate::telemetry::receipt_latency::capacity::Observation;
    for byte_first in [true, false] {
        let admission = Admission::new();
        admission
            .initialize_receipts(
                super::super::receipts::capacity::Inventory {
                    bytes: 0,
                    rows: 0,
                    unknown: vec![],
                },
                1024 * 1024 * 1024,
            )
            .map_err(|_| anyhow::anyhow!("inventory"))?;
        let initial = admission
            .observed_capacity()
            .map_err(|_| anyhow::anyhow!("initial"))?;
        let weak = Arc::downgrade(&admission);
        let observation = Observation::default();
        ensure!(observation.bind(initial, move || {
            weak.upgrade()
                .ok_or(ReadError::Unavailable)?
                .observed_capacity()
        }));
        let readable = observation.collect();
        let guard = admission
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("state"))?;
        let busy = observation.collect();
        ensure!(busy[1] == Some(("current", Err(ReadError::Unavailable))));
        let (bytes, state) = if byte_first {
            (readable, busy)
        } else {
            (busy, readable)
        };
        ensure!(bytes[0] == state[0] && bytes[1] != state[1]);
        drop(guard);
        admission.close();
        let before_seal = observation.collect();
        ensure!(observation.seal(true));
        let after_seal = observation.collect();
        ensure!(before_seal[1].is_some_and(|(name, _)| name == "current"));
        ensure!(after_seal[1].is_some_and(|(name, value)| name == "final" && value.is_ok()));
        drop(admission);
        ensure!(observation.collect() == after_seal);
    }
    Ok(())
}
