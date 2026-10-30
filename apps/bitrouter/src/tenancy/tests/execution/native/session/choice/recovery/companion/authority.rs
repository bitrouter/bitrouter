//! Authorization changes while the actual repaired row is committed but Core
//! has not received its first checkpoint ACK. The gate changes scheduling only.
use super::*;
use crate::tenancy::transaction::Disposition;

#[tokio::test]
#[ignore = "isolated native repair commit and authority campaign; select this parent"]
async fn native_recovery_committed_repair_survives_revocation_expiry_and_lost_waiter() -> Result<()>
{
    campaign(&["late-revoke", "late-expiry", "late-lost-waiter"]).await
}

pub(super) async fn case(root: &Path, mode: &str) -> Result<()> {
    let mut stopped = Stopped::produce(root, mode).await?;
    let result = async {
        let expiry = if mode == "late-expiry" {
            let expires = Utc::now() + chrono::Duration::seconds(8);
            let mut input = stopped.bound.fixture.issuance("recovery-expiring-reader")?;
            input.expires_at = expires
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                .try_into()?;
            let key = crate::tenancy::tests::issued(
                stopped
                    .bound
                    .fixture
                    .service
                    .execute(
                        stopped.bound.fixture.operator.clone(),
                        Command::Issue(stopped.bound.fixture.tenant.clone(), input),
                    )
                    .await
                    .map_err(failure)?,
            )?;
            stopped.bound.headers.insert(
                http::header::AUTHORIZATION,
                format!(
                    "Bearer {}",
                    key.secret
                        .as_deref()
                        .context("issued recovery credential")?
                )
                .parse()?,
            );
            stopped.bound.key = key;
            Some(expires)
        } else {
            None
        };
        let before = fixture::journal(&stopped.bound).await?;
        let link = before["owned_choices"]
            .as_object()
            .context("actual pending Choice")?
            .values()
            .next()
            .context("real admission link")?;
        let expected: BTreeSet<String> = ["parent_execution", "child_execution"]
            .iter()
            .map(|field| {
                link[*field]
                    .as_str()
                    .map(str::to_owned)
                    .context("admitted execution")
            })
            .collect::<Result<_>>()?;
        let receipt_key = Key {
            installation: stopped.bound.fixture.installation.clone(),
            tenant: stopped.bound.fixture.tenant.clone(),
            execution: link["child_execution"]
                .as_str()
                .context("admitted child execution")?
                .try_into()?,
        };
        stopped
            .bound
            .fixture
            .service
            .cleanup
            .observe_receipt_for_test(receipt_key.clone())?;
        let gate = stopped
            .bound
            .fixture
            .service
            .hold_host(crate::tenancy::hosts::HostBoundary::RecoveryCommitted)?;
        let app = Arc::new(
            host_app(
                &stopped.bound.fixture,
                &stopped.generation.url,
                Arc::new(HostSpies::default()),
            )?
            .with_tenancy(stopped.bound.fixture.service.clone()),
        );
        let headers = stopped.bound.headers.clone();
        let workspace = root.join("workspace");
        let opening = AbortOnDropHandle::new(tokio::spawn(async move {
            NativeSession::open_authenticated(
                app,
                headers,
                "choice-session",
                &workspace,
                ToolMode::ReadOnly,
                &HarnessConfig::default(),
            )
            .await
        }));
        let checked = async {
            tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
            // This host gate follows the repair reply; it does not pause H
            // maintenance. Read the retained outcome of this exact repair.
            let trace = stopped
                .bound
                .fixture
                .service
                .cleanup
                .receipt_trace_for_test()?;
            ensure!(trace.key == receipt_key && !trace.overflow && trace.facts[1].is_none());
            let fact = trace.facts[0].as_ref().context("actual repair receipt")?;
            ensure!(
                fact.inserted == Some(true)
                    && fact.store.is_some_and(|outcome| outcome.ok)
                    && fact.native_checkpoint.is_some_and(|outcome| outcome.ok)
                    && fact.commit.is_some_and(|outcome| outcome.ok)
                    && fact.rollback.is_none()
                    && !fact.check_failed
                    && fact.disposition == Disposition::Committed
            );
            ensure!(stopped.bound.fixture.service.cleanup.disposition() != Disposition::Ambiguous);
            let durable = terminals(&stopped.bound.fixture.db).await?;
            ensure!(
                durable.len() == 2
                    && durable.iter().map(|r| r.0.clone()).collect::<BTreeSet<_>>() == expected
            );
            let row: Value = serde_json::from_str(&payload(&stopped.bound.fixture.db).await?)?;
            ensure!(row["recovery"]["phase"] == "retiring");
            ensure!(
                !opening.is_finished() && stopped.bound.fixture.service.host_slots_for_test() == 0
            );
            ensure!(
                stopped
                    .bound
                    .fixture
                    .service
                    .admission
                    .receipt_capacity_for_test()
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?
                    .0
                    == 0,
                "known commit did not publish repair accounting before ACK"
            );
            match mode {
                "late-revoke" => {
                    change_authority(
                        &stopped.bound.fixture,
                        &stopped.bound.key,
                        &stopped.bound.route,
                        "revoke",
                    )
                    .await?
                }
                "late-expiry" => {
                    let expires = expiry.context("actual expiring credential")?;
                    ensure!(
                        Utc::now() < expires,
                        "fixture expired before the commit boundary"
                    );
                    tokio::time::timeout(Duration::from_secs(10), async {
                        while Utc::now() <= expires {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    })
                    .await?;
                }
                "late-lost-waiter" => opening.abort(),
                _ => anyhow::bail!("unknown authority boundary"),
            }
            Ok::<_, anyhow::Error>(durable)
        }
        .await;
        gate.release();
        let opened = tokio::time::timeout(TIMEOUT, opening).await?;
        let durable = checked?;
        if mode == "late-lost-waiter" {
            ensure!(
                opened.is_err_and(|error| error.is_cancelled()),
                "outer waiter was not cancelled"
            );
        } else {
            if let Ok(session) = opened? {
                session.close().await?;
                anyhow::bail!("changed authority published native handle");
            }
        }
        tokio::time::timeout(TIMEOUT, async {
            while stopped.bound.fixture.service.host_slots_for_test() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        ensure!(
            terminals(&stopped.bound.fixture.db).await? == durable,
            "retirement rewrote admitted terminals after lost authority or waiter"
        );
        ensure!(stopped.generation.calls() == 0 && stopped.choice.calls() == 1);
        locked(root)?;
        Ok(())
    }
    .await;
    stopped.close(result).await
}
