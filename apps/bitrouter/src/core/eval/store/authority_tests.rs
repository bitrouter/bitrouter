//! Manual Eval cannot acquire built-in authority through principal spelling.
use super::*;

#[tokio::test]
async fn manual_principal_spelling_cannot_mint_builtin_authority() -> Result<()> {
    use crate::trajectory::evaluation::{
        TRAJECTORY_EVALUATOR_AUTHORITY_ID, TRAJECTORY_EVALUATOR_ID,
    };
    use bitrouter_ai::eval::{EvalVerdict, EvaluatorKind};
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let (scope, provenance) = Scope::for_test(
        "installation".try_into()?,
        "tenant".try_into()?,
        TRAJECTORY_EVALUATOR_AUTHORITY_ID.try_into()?,
    );
    let store = EvalStore {
        db: db.clone(),
        scope,
        provenance,
        storage_budget: None,
    };
    store.insert_subject(&super::tests::subject()?).await?;
    let mut result = super::tests::result(EvalVerdict::Inconclusive);
    result.evaluator.kind = EvaluatorKind::Generic;
    result.evaluator.authority_id = TRAJECTORY_EVALUATOR_AUTHORITY_ID.into();
    result.evaluator.evaluator_id = TRAJECTORY_EVALUATOR_ID.into();
    let tx = store.transaction(false).await?;
    let check = || Ok(());
    assert!(matches!(
        store.submit_owned_in(&tx, &result, &check).await,
        Err(OwnedError::Rejected(ErrorCode::Forbidden))
    ));
    result.evaluator.evaluator_id = "manual-observer".into();
    assert_eq!(
        store.submit_owned_in(&tx, &result, &check).await?.status,
        AdmissionStatus::Admitted
    );
    tx.rollback().await?;
    assert!(store.results_for_subject("eval-1").await?.is_empty());
    db.close().await?;
    Ok(())
}
