use super::*;
use anyhow::{Result, ensure};
fn failure(code: ErrorCode) -> anyhow::Error {
    anyhow::anyhow!("{code:?}")
}

#[test]
fn future_completion_and_physical_writes_share_fresh_free_space() -> Result<()> {
    let mut state = Accounting::default();
    state
        .initialize(Inventory::default(), ADMISSION_FLOOR)
        .map_err(failure)?;
    state.reserve(0, 0).map_err(failure)?;
    let physical = ADMISSION_FLOOR - MAINTENANCE_FLOOR - RESERVATION;
    state
        .reserve_physical_write(physical, false)
        .map_err(failure)?;
    ensure!(!state.ready(0, 1));
    ensure!(state.reserve_physical_write(1, false) == Err(ErrorCode::CapacityExceeded));
    state.finish_physical_write(physical).map_err(failure)?;
    ensure!(state.ready(0, 1) && state.totals.reserved == RESERVATION);
    state.free = ADMISSION_FLOOR - 1;
    ensure!(!state.ready(0, 1));
    ensure!(state.reserve_physical_write(1, false).is_err());
    state.reserve_physical_write(1, true).map_err(failure)?;
    Ok(())
}

#[test]
fn management_physical_fit_ignores_logical_history_and_unknown_limits() -> Result<()> {
    let mut state = Accounting::default();
    state
        .initialize(
            Inventory {
                unknown_count: u64::MAX,
                live_bytes: u64::MAX,
                ..Default::default()
            },
            ADMISSION_FLOOR,
        )
        .map_err(failure)?;
    ensure!(!state.globally_ready());
    state.reserve_physical_write(1, false).map_err(failure)?;
    state.finish_physical_write(1).map_err(failure)?;
    state.free = 7;
    state.reserve_physical_write(7, true).map_err(failure)?;
    ensure!(state.reserve_physical_write(1, true).is_err());
    state.finish_physical_write(7).map_err(failure)?;
    ensure!(state.finish_physical_write(1).is_err() && state.fenced);
    Ok(())
}

#[test]
fn restored_history_and_pending_sources_have_no_live_row_count_limit() -> Result<()> {
    let mut state = Accounting::default();
    state
        .initialize(
            Inventory {
                bytes: u64::MAX,
                rows: u64::MAX,
                history_bytes: u64::MAX,
                history_rows: u64::MAX,
                oauth_bytes: u64::MAX,
                live_bytes: 17,
                unknown_count: 100_001,
                unknown_digest: [7; 32],
            },
            u64::MAX,
        )
        .map_err(failure)?;
    ensure!(state.totals.bytes == 17 && state.totals.unknown == 100_001 && !state.fenced);
    ensure!(!state.globally_ready());
    Ok(())
}

#[test]
fn configured_unknown_limits_and_overflow_are_checked_without_partial_reservation() -> Result<()> {
    let limits = crate::config::tenancy::capacity::Receipts {
        unknown_tenant: 128,
        high_water_bytes: 101 * RESERVATION,
        ..Default::default()
    };
    let mut state = Accounting::configured(&limits);
    for count in 0..101 {
        state.reserve(0, count).map_err(failure)?;
    }
    ensure!(!state.ready(0, 101));
    let before = state.totals;
    ensure!(state.reserve(0, 101).is_err() && state.totals == before);
    ensure!(
        state.reserve_physical_write(u64::MAX, true).is_err() && state.physical_write_reserved == 0
    );
    let mut state = Accounting::default();
    state.totals.unknown = u64::MAX;
    state.totals.uncovered = 1;
    ensure!(!state.globally_ready());
    Ok(())
}

#[cfg(all(feature = "core", feature = "harness"))]
#[test]
fn maintenance_repair_ignores_lowered_ordinary_budgets_but_keeps_physical_custody() -> Result<()> {
    let mut state = Accounting::default();
    state
        .initialize(
            Inventory {
                unknown_count: u64::MAX,
                live_bytes: u64::MAX,
                ..Default::default()
            },
            2 * ROW_OVERHEAD + 9,
        )
        .map_err(failure)?;
    let charge = state.reserve_repair(9, 2, 1).map_err(failure)?;
    ensure!(charge == state.free && state.totals.uncovered == 1 && state.totals.live == 2);
    ensure!(state.reserve_physical_write(1, true).is_err());
    let before = state.totals;
    ensure!(state.reserve_repair(u64::MAX, 1, 1).is_err() && state.totals == before);
    Ok(())
}

#[test]
fn archive_watermark_precedes_admission_refusal_and_stops_below_three_quarters() {
    let mut accounting = Accounting::default();
    let high_water = accounting.limits.high_water_bytes;
    let watermark = high_water - high_water / 4;
    accounting.totals.bytes = watermark - 1;
    assert!(!accounting.archive_needed());
    accounting.totals.bytes = watermark;
    assert!(accounting.archive_needed());
    assert!(
        accounting.globally_ready(),
        "archive starts while the next completion still fits"
    );
    accounting.totals.bytes -= RESERVATION;
    accounting.totals.reserved = RESERVATION;
    assert!(
        accounting.archive_needed(),
        "future completion custody contributes to pressure"
    );
    accounting.totals.bytes -= 1;
    assert!(!accounting.archive_needed());
    accounting.totals.reserved = u64::MAX;
    assert!(accounting.archive_needed(), "overflow cannot hide pressure");
    accounting.totals.bytes = 0;
    assert!(
        !accounting.archive_needed(),
        "no inline class remains to archive"
    );

    accounting.limits.high_water_bytes = u64::MAX;
    accounting.totals.reserved = 0;
    accounting.totals.bytes = u64::MAX - u64::MAX / 4;
    assert!(
        accounting.archive_needed(),
        "watermark does not multiply into overflow"
    );
    accounting.totals.bytes -= 1;
    assert!(!accounting.archive_needed());
}
