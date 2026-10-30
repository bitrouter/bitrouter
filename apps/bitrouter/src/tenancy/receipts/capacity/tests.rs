use super::*;
use anyhow::{Result, ensure};

fn inventory() -> Inventory {
    Inventory {
        bytes: 0,
        rows: 0,
        unknown: Vec::new(),
    }
}
fn tenant() -> Result<TenantId> {
    Ok("tenant-a".try_into()?)
}

#[test]
fn completion_reservation_and_physical_headroom_are_atomic() -> Result<()> {
    let tenant = tenant()?;
    let mut state = Accounting::default();
    state
        .initialize(inventory(), ADMISSION_FLOOR - 1)
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    ensure!(!state.ready(&tenant));
    state.free = ADMISSION_FLOOR;
    // A single tenant's possible unknown outcomes impose the smaller bound.
    for _ in 0..UNKNOWN_TENANT {
        state
            .reserve(&tenant)
            .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    }
    ensure!(state.reserve(&tenant) == Err(ErrorCode::CapacityExceeded));
    ensure!(state.reserved == RESERVATION * UNKNOWN_TENANT as u64);
    let other: TenantId = "tenant-b".try_into()?;
    for _ in UNKNOWN_TENANT..128 {
        state
            .reserve(&other)
            .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    }
    ensure!(state.reserved + MAINTENANCE_FLOOR == ADMISSION_FLOOR);
    ensure!(state.reserve(&other) == Err(ErrorCode::CapacityExceeded));
    state
        .release(&tenant, None)
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    state
        .reserve(&other)
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    ensure!(state.reserved == RESERVATION * 128);
    Ok(())
}

#[test]
fn unknown_capacity_survives_restart_and_escalation_never_frees_a_slot() -> Result<()> {
    let tenant = tenant()?;
    let now = chrono::Utc::now();
    let rows = (0..100)
        .map(|i| {
            Ok((
                tenant.clone(),
                format!("execution-{i}").try_into()?,
                now - chrono::Duration::minutes(16),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut state = Accounting::default();
    state
        .initialize(
            Inventory {
                bytes: 100,
                rows: 100,
                unknown: rows.clone(),
            },
            u64::MAX,
        )
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    ensure!(!state.ready(&tenant));
    ensure!(state.escalate(now) == 100 && state.escalate(now) == 0);
    ensure!(!state.ready(&tenant) && state.next_escalation().is_none());
    let mut reopened = Accounting::default();
    reopened
        .initialize(
            Inventory {
                bytes: 100,
                rows: 100,
                unknown: rows,
            },
            u64::MAX,
        )
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    ensure!(!reopened.ready(&tenant));
    ensure!(reopened.escalate(now) == 100);
    Ok(())
}

#[test]
fn committed_unknown_converts_reserved_slot_without_capacity_gap() -> Result<()> {
    let tenant = tenant()?;
    let now = chrono::Utc::now();
    let mut state = Accounting::default();
    state
        .initialize(inventory(), u64::MAX)
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    for i in 0..100 {
        state
            .reserve(&tenant)
            .map_err(|code| anyhow::anyhow!("{code:?}"))?;
        state
            .release(
                &tenant,
                Some((1, Some((format!("execution-{i}").try_into()?, now)))),
            )
            .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    }
    ensure!(state.reserved == 0 && state.potential_unknown.is_empty());
    ensure!(state.bytes == 100 * (ROW_OVERHEAD + 1));
    ensure!(!state.ready(&tenant));
    ensure!(state.escalate(now + chrono::Duration::minutes(14)) == 0);
    ensure!(state.escalate(now + chrono::Duration::minutes(15)) == 100);
    ensure!(!state.ready(&tenant));
    Ok(())
}

#[test]
fn inventory_and_live_bytes_enforce_high_water_without_overflow() -> Result<()> {
    let tenant = tenant()?;
    let mut state = Accounting::default();
    state
        .initialize(
            Inventory {
                bytes: HIGH_WATER - RESERVATION,
                rows: 0,
                unknown: Vec::new(),
            },
            u64::MAX,
        )
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    state
        .reserve(&tenant)
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    ensure!(!state.ready(&tenant));
    state
        .release(&tenant, Some((17, None)))
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    ensure!(!state.ready(&tenant));
    let mut corrupt = Accounting::default();
    ensure!(
        corrupt
            .initialize(
                Inventory {
                    bytes: u64::MAX,
                    rows: 1,
                    unknown: Vec::new()
                },
                u64::MAX
            )
            .is_err()
    );
    state.fenced = true;
    ensure!(!state.ready(&tenant));
    Ok(())
}
