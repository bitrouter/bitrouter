use super::*;
use anyhow::ensure;

pub(super) enum Change {
    Routing,
    Policy,
    ReservationLost,
}

pub(super) struct ChangeAfterAdmission {
    pub(super) change: Change,
    pub(super) db: DatabaseConnection,
    pub(super) routing: Arc<ConfigRoutingTable>,
    pub(super) policies: Arc<PolicyRuntime>,
}

#[async_trait]
impl PreRequestHook for ChangeAfterAdmission {
    async fn check(
        &self,
        ctx: &mut PipelineContext,
    ) -> crate::gateway::error::Result<HookDecision> {
        let result: Result<()> = async {
            match self.change {
                Change::Routing => {
                    self.routing
                        .replace_prepared_config(self.routing.snapshot_config())
                        .await?;
                }
                Change::Policy => {
                    self.policies
                        .reload_for_config(&self.routing.snapshot_config(), None)
                        .await?;
                }
                Change::ReservationLost => {
                    let observed = &ctx
                        .get_event::<GatewayRequestObserved>()
                        .context("missing observation")?
                        .request;
                    let key = crate::storage::gateway_records::digest(&observed.request_id)?;
                    let store = GatewayRecords::new(self.db.clone(), "gateway-request-admission")?;
                    ensure!(
                        records::Entity::delete_by_id(store.id("gateway_request", &key)?)
                            .exec(&self.db)
                            .await?
                            .rows_affected
                            == 1
                    );
                }
            }
            Ok(())
        }
        .await;
        result.map_err(internal)?;
        Ok(HookDecision::Allow)
    }
}

#[tokio::test]
async fn dispatch_rejects_changed_routing_and_policy_before_upstream_send() -> Result<()> {
    for change in [Change::Routing, Change::Policy] {
        let fixture = fixture(Failure::Change(change)).await?;
        ensure!(
            fixture
                .pipeline
                .execute(request("changed", true)?)
                .await
                .is_err()
        );
        fixture
            .pipeline
            .drain_required_pending_settlements()
            .await?;
        let row = saved(&fixture, "changed").await?;
        let terminal = row.settlement.context("missing settlement")?;
        assert_eq!(terminal.outcome, Some(ExecutionOutcome::Failed));
        assert_eq!(terminal.total_cost_micro_usd, Some(0));
        ensure!(
            terminal.hops.is_empty()
                && fixture
                    .upstream
                    .received_requests()
                    .await
                    .context("mock")?
                    .is_empty()
        );
    }
    Ok(())
}

#[tokio::test]
async fn context_revalidation_rejects_stale_fences_without_rewriting_execution() -> Result<()> {
    for change in [Change::Routing, Change::Policy, Change::ReservationLost] {
        let fixture = fixture(Failure::None).await?;
        let mut ctx = PipelineContext::new(request("stale", true)?);
        let retained = ctx.require_generation_prompt()?.messages.clone();
        let mut initial = ctx.require_generation_prompt()?.clone();
        initial
            .messages
            .insert(0, Message::text(Role::User, "old history"));
        ctx.apply_routing_prompt(initial)?;
        NormalizedScope.check(&mut ctx).await?;
        fixture.tracker.check(&mut ctx).await?;
        fixture.binding.check(&mut ctx).await?;
        ctx.replace_managed_messages(retained)?;
        ChangeAfterAdmission {
            change,
            db: fixture.db.clone(),
            routing: fixture.routing.clone(),
            policies: fixture.policies.clone(),
        }
        .check(&mut ctx)
        .await?;
        let inventory = fixture.tracker.inventory();
        let before = serde_json::to_value(inventory.request("scope", "stale").await?)?;
        ensure!(
            PreRequestHook::revalidate_context(&fixture.binding, &ctx)
                .await
                .is_err()
        );
        ensure!(
            RouteHook::revalidate_context(&fixture.binding, &[], &ctx)
                .await
                .is_err()
        );
        assert_eq!(
            serde_json::to_value(inventory.request("scope", "stale").await?)?,
            before
        );
        ensure!(
            fixture
                .upstream
                .received_requests()
                .await
                .context("mock")?
                .is_empty()
        );
    }
    Ok(())
}

pub(super) fn second_request(id: &str) -> Result<PipelineRequest> {
    let mut req = request(id, true)?;
    req.headers
        .insert("x-bitrouter-controller-id", "other-controller".parse()?);
    req.headers.insert(
        "x-bitrouter-acp-session-id",
        "another-claimed-session".parse()?,
    );
    Ok(req)
}

#[tokio::test]
async fn another_controller_cannot_repeat_a_completed_execution_or_replace_its_charge() -> Result<()>
{
    let fixture = fixture(Failure::None).await?;
    fixture.pipeline.execute(request("paid", true)?).await?;
    fixture
        .pipeline
        .drain_required_pending_settlements()
        .await?;
    let original = serde_json::to_value(saved(&fixture, "paid").await?)?;
    let charges = crate::metering::entities::requests::Entity::find_by_id("paid");
    let before = charges
        .clone()
        .one(&fixture.db)
        .await?
        .context("missing paid charge")?;
    ensure!(
        fixture
            .pipeline
            .execute(second_request("paid")?)
            .await
            .is_err()
    );
    fixture
        .pipeline
        .drain_required_pending_settlements()
        .await?;
    assert_eq!(
        serde_json::to_value(saved(&fixture, "paid").await?)?,
        original
    );
    assert_eq!(charges.one(&fixture.db).await?, Some(before));
    ensure!(
        fixture
            .tracker
            .inventory()
            .request("different-principal", "paid")
            .await?
            .is_none()
    );
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("mock")?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn competing_controller_admissions_cannot_initialize_two_attempts() -> Result<()> {
    let fixture = fixture(Failure::None).await?;
    let (a, b) = tokio::join!(
        fixture.pipeline.execute(request("race", false)?),
        fixture.pipeline.execute(second_request("race")?)
    );
    ensure!(
        a.is_ok() ^ b.is_ok(),
        "expected exactly one accepted request"
    );
    fixture
        .pipeline
        .drain_required_pending_settlements()
        .await?;
    let row = saved(&fixture, "race").await?;
    assert_eq!(
        row.controller_instance_id,
        if a.is_ok() {
            "controller"
        } else {
            "other-controller"
        }
    );
    assert_eq!(
        row.settlement.context("winning settlement")?.outcome,
        Some(ExecutionOutcome::Completed)
    );
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("mock")?
            .len(),
        1
    );
    assert_eq!(
        records::Entity::find()
            .filter(records::Column::Kind.eq("gateway_request"))
            .count(&fixture.db)
            .await?,
        1
    );
    Ok(())
}
