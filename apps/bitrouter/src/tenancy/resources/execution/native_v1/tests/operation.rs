//! The shared startup quota observes real v1 work without replacing local caps.
use super::*;
use crate::storage::operation_budget::{Error as BudgetError, OperationBudget};

#[test]
fn two_distinct_retained_decodes_share_actual_work_and_the_original_row_limit() -> TestResult<()> {
    let check = || Ok(());
    let operation = OperationBudget::with_row_limit(&check, 1024);
    for name in ["first-revision", "second-revision"] {
        let input = basic(name)?;
        let decoded = retained::decode_with_operation(envelope(&input, None)?, Some(&operation))?;
        let view = checked(RetainedExecutionRevision::new(
            "installation".try_into()?,
            "tenant".try_into()?,
            name.try_into()?,
            1.try_into()?,
            decoded,
        ))?;
        checked(view.select_native(name, ModelOperation::Generation))?;
    }
    // Small real decodes leave most of the cumulative 32 MiB for bounded SQL work.
    // No ceiling reservation or second row budget is allowed for each revision.
    operation.charge(1024, 24 * 1024 * 1024)?;
    assert!(matches!(operation.charge(1, 0), Err(BudgetError::Capacity)));
    Ok(())
}

#[test]
fn exhausted_outer_budget_is_an_error_not_a_native_unavailable_http_view() -> TestResult<()> {
    let input = basic("literal:route")?;
    let check = || Ok(());
    let operation = OperationBudget::with_row_limit(&check, 1024);
    operation.charge(0, 32 * 1024 * 1024)?;
    let error = retained::decode_with_operation(envelope(&input, None)?, Some(&operation))
        .err()
        .context("shared quota must stop the actual decoder")?;
    ensure!(matches!(
        error.downcast_ref::<BudgetError>(),
        Some(BudgetError::Capacity)
    ));
    // Identical authenticated v1 content remains readable without a caller quota.
    view(envelope(&input, None)?)?;
    Ok(())
}

#[test]
fn provider_ancestry_is_charged_before_resolved_config_expansion() -> TestResult<()> {
    let input = basic("route")?;
    let Definition::Route { routing, .. } = input.definition else {
        anyhow::bail!("route fixture")
    };
    let check = || Ok(());
    let operation = OperationBudget::with_row_limit(&check, 1024);
    let mut measured = ByteBudget::new(super::super::super::super::MAX_ARTIFACT_BYTES);
    measured.charge(&routing)?;
    let source_bytes = super::super::super::super::MAX_ARTIFACT_BYTES - measured.remaining();
    operation.charge(0, 32 * 1024 * 1024 - source_bytes)?;
    let mut local = ByteBudget::new(MAX_WORK_BYTES);
    let error = compiler::routing_config_observed(&routing, Some(&mut local), Some(&operation))
        .err()
        .context("source fits but provider ancestry must not escape shared quota")?;
    ensure!(matches!(
        error.downcast_ref::<BudgetError>(),
        Some(BudgetError::Capacity)
    ));
    Ok(())
}

#[test]
fn native_local_copy_limit_remains_independent_of_larger_outer_allowance() -> TestResult<()> {
    let check = || Ok(());
    let operation = OperationBudget::with_row_limit(&check, 1024);
    let mut native = Budget::with_operation(Some(&operation));
    native
        .bytes(MAX_WORK_BYTES)
        .map_err(|_| anyhow::anyhow!("exact native bound"))?;
    assert!(matches!(native.bytes(1), Err(Unavailable::Capacity)));
    operation.charge(0, 24 * 1024 * 1024)?;
    assert!(matches!(operation.charge(0, 1), Err(BudgetError::Capacity)));
    Ok(())
}
