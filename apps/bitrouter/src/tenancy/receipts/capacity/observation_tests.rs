use super::*;
use anyhow::{Result, ensure};
#[test]
fn metrics_keep_total_live_potentials_while_policy_uses_uncovered() -> Result<()> {
    let mut accounting = Accounting::default();
    accounting
        .reserve(0, 0)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let charge = accounting
        .reserve_repair(123, 2, 1)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    ensure!(accounting.observed(1) == Some((RESERVATION + charge, 0, 3, 0)));
    ensure!(accounting.observed(0).is_none());
    accounting.totals.uncovered = 0;
    accounting.totals.unknown = 3;
    ensure!(accounting.observed(1) == Some((RESERVATION + charge, 0, 3, 3)));
    accounting
        .reserve_physical_write(100, true)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    ensure!(accounting.observed(1) == Some((RESERVATION + charge, 0, 3, 3)));
    accounting.totals.repair_live = 4;
    ensure!(accounting.observed(1).is_none());
    Ok(())
}
