//! Supplement the actual native/SDK cases with checked decomposition failures.
use super::*;
use anyhow::{Result, ensure};

#[test]
fn execution_and_repair_share_totals_without_fabricated_active_slots() -> Result<()> {
    let mut accounting = Accounting::default();
    let tenant: TenantId = "mixed-tenant".try_into()?;
    accounting
        .reserve(&tenant)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let charge = accounting
        .reserve_repair(&tenant, 123, 2)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    ensure!(charge == 123 + 2 * ROW_OVERHEAD);
    ensure!(accounting.observed(1) == Some((RESERVATION + charge, 0, 3, 0)));
    ensure!(
        accounting.observed(0).is_none(),
        "ordinary execution still requires its slot"
    );
    accounting
        .finish_repair(&tenant, charge, 2, None)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    ensure!(accounting.observed(1) == Some((RESERVATION, 0, 1, 0)));
    accounting
        .release(&tenant, None)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    ensure!(accounting.observed(0) == Some((0, 0, 0, 0)));
    let charge = accounting
        .reserve_repair(&tenant, 321, 2)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let entries = [
        ("parent".try_into()?, chrono::Utc::now()),
        ("child".try_into()?, chrono::Utc::now()),
    ];
    ensure!(accounting.observed(0) == Some((charge, 0, 2, 0)));
    accounting
        .finish_repair(&tenant, charge, 2, Some(&entries))
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    ensure!(accounting.observed(0) == Some((0, charge, 0, 2)));
    ensure!(
        accounting
            .finish_repair(&tenant, charge, 2, Some(&entries))
            .is_err()
    );
    ensure!(
        accounting.observed(0) == Some((0, charge, 0, 2)),
        "duplicate settlement changed totals"
    );
    Ok(())
}

#[test]
fn corrupted_or_overflowing_decomposition_is_unavailable() -> Result<()> {
    let tenant: TenantId = "repair-tenant".try_into()?;
    for corrupt in 0..5 {
        let mut accounting = Accounting::default();
        accounting
            .reserve_repair(&tenant, 123, 2)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        match corrupt {
            0 => accounting.repair_potential = 3,
            1 => accounting.repair_reserved += 1,
            2 => accounting.reserved = u64::MAX,
            3 => accounting.repair_reserved = 1,
            _ => {
                accounting.repair_potential = 0;
                accounting.potential_unknown.clear();
            }
        }
        ensure!(accounting.observed(0).is_none());
    }
    let mut accounting = Accounting::default();
    ensure!(accounting.reserve_repair(&tenant, u64::MAX, 1).is_err());
    ensure!(
        accounting.observed(0) == Some((0, 0, 0, 0)),
        "rejected reservation mutated accounting"
    );
    Ok(())
}
