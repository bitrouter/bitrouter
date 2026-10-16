use super::*;
use bitrouter_orchestrator::goal::accounting::{Event, Work, WorkKind};

#[tokio::test]
async fn estimates_and_unpriced_work_remain_distinct_and_duplicates_do_not_add() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    for (index, estimate) in [Some(100_u64), None].into_iter().enumerate() {
        let key = format!("provider-{index}");
        let begin = Change::Accounting {
            event: Event::Begin {
                work: Work {
                    id: key.clone(),
                    kind: WorkKind::ProviderAttempt,
                    requirements_revision: 1,
                    source: None,
                    request_id: Some("logical-request".into()),
                    intent: report(workspace.path(), &format!("attempt {index} admitted"))?,
                    token_reservation: None,
                },
            },
        };
        goals
            .workflow_change(actor(), &id, &format!("{key}-begin"), 1, begin, 100)
            .await?;
        let observed = Change::Accounting {
            event: Event::Observe {
                id: key.clone(),
                report: report(workspace.path(), &format!("attempt {index} observed"))?,
                estimated_model_tokens_micro_usd: estimate,
                reservation_verified: false,
            },
        };
        let first = goals
            .workflow_change(
                actor(),
                &id,
                &format!("{key}-outcome"),
                1,
                observed.clone(),
                110,
            )
            .await?;
        let duplicate = goals
            .workflow_change(actor(), &id, &format!("{key}-outcome"), 1, observed, 120)
            .await?;
        assert_eq!(first, duplicate);
    }
    let head = snapshot(&goals, &id).await?;
    assert_eq!(head.accounting.provider_attempts, 2);
    assert_eq!(head.accounting.estimated_model_tokens_micro_usd, 100);
    assert_eq!(head.accounting.unpriced_outcomes, 1);
    assert_eq!(head.accounting.work_started, head.accounting.work_observed);
    assert!(head.accounting.in_flight.is_empty());
    Ok(())
}
