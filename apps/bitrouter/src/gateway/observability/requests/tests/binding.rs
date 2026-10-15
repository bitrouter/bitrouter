use super::*;

pub(super) enum Change {
    Routing,
    Policy,
    SessionDeleted,
}

pub(super) struct ChangeAfterAdmission {
    pub(super) change: Change,
    pub(super) db: DatabaseConnection,
    pub(super) identity: SessionIdentity,
    pub(super) routing: Arc<ConfigRoutingTable>,
    pub(super) policies: Arc<PolicyRuntime>,
}

#[async_trait]
impl PreRequestHook for ChangeAfterAdmission {
    async fn check(
        &self,
        _ctx: &mut PipelineContext,
    ) -> crate::gateway::error::Result<HookDecision> {
        let change: Result<()> = async {
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
                Change::SessionDeleted => {
                    sessions::Entity::update_many()
                        .col_expr(
                            sessions::Column::Deleted,
                            sea_orm::sea_query::Expr::value(true),
                        )
                        .filter(sessions::Column::SessionKey.eq(self.identity.key()?))
                        .exec(&self.db)
                        .await?;
                }
            }
            Ok(())
        }
        .await;
        change.map_err(internal)?;
        Ok(HookDecision::Allow)
    }
}

#[tokio::test]
async fn dispatch_rejects_routing_policy_and_session_changes_without_learning() -> Result<()> {
    for change in [Change::Routing, Change::Policy, Change::SessionDeleted] {
        let fixture = fixture(Failure::Change(change)).await?;
        let error = fixture
            .pipeline
            .execute(request("changed", true)?)
            .await
            .err()
            .context("changed canonical request was dispatched")?;
        assert!(
            error
                .to_string()
                .contains("canonical evolution state could not be verified")
        );
        fixture
            .pipeline
            .drain_required_pending_settlements()
            .await?;
        let inventory = saved(&fixture, "changed").await?;
        let execution = execution(&fixture, "changed").await?;
        assert_eq!(execution.attempt_id, inventory.attempt_id);
        assert_eq!(execution.settlement, inventory.settlement);
        let settlement = execution.settlement.context("failed settlement missing")?;
        assert_eq!(settlement.outcome, Some(ExecutionOutcome::Failed));
        assert_eq!(settlement.total_cost_micro_usd, Some(0));
        assert!(settlement.hops.is_empty());
        assert!(
            fixture
                .upstream
                .received_requests()
                .await
                .context("requests missing")?
                .is_empty()
        );
    }
    Ok(())
}

async fn second_controller(fixture: &Fixture) -> Result<()> {
    connections::ActiveModel {
        connection_id: Set("second-capture".into()),
        owner: Set(fixture.identity.owner.clone()),
        source: Set(fixture.identity.source.clone()),
        controller_instance_id: Set(Some("second-controller".into())),
        route_scope_id: Set(Some("scope".into())),
        state: Set("recording".into()),
        head: Set(0),
        started_at: Set("fixture".into()),
        metadata_json: Set("{}".into()),
    }
    .insert(&fixture.db)
    .await?;
    fixture
        .tracker
        .inventory()
        .register_capture("second-capture", "scope", "second-controller")
        .await?;
    events::ActiveModel {
        connection_id: Set("second-capture".into()),
        sequence: Set(1),
        session_key: Set(Some(fixture.identity.key()?)),
        session_sequence: Set(Some(2)),
        replay: Set(false),
        event_json: Set("{}".into()),
        captured_at: Set("fixture".into()),
    }
    .insert(&fixture.db)
    .await?;
    sessions::Entity::update_many()
        .col_expr(sessions::Column::Head, sea_orm::sea_query::Expr::value(2))
        .filter(sessions::Column::SessionKey.eq(fixture.identity.key()?))
        .exec(&fixture.db)
        .await?;
    connections::Entity::update_many()
        .col_expr(
            connections::Column::Head,
            sea_orm::sea_query::Expr::value(1),
        )
        .filter(connections::Column::ConnectionId.eq("second-capture"))
        .exec(&fixture.db)
        .await?;
    Ok(())
}

fn second_request(id: &str) -> Result<PipelineRequest> {
    let mut request = request(id, true)?;
    request.headers.insert(
        "x-bitrouter-controller-instance-id",
        "second-controller".parse()?,
    );
    Ok(request)
}

#[tokio::test]
async fn context_revalidation_rejects_stale_fences_without_rewriting_execution() -> Result<()> {
    for change in [Change::Routing, Change::Policy, Change::SessionDeleted] {
        let fixture = fixture(Failure::None).await?;
        let mut ctx = PipelineContext::new(request("rebuild-changed", true)?);
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
        let execution_before = serde_json::to_value(execution(&fixture, "rebuild-changed").await?)?;
        let inventory_before = serde_json::to_value(saved(&fixture, "rebuild-changed").await?)?;
        ChangeAfterAdmission {
            change,
            db: fixture.db.clone(),
            identity: fixture.identity.clone(),
            routing: fixture.routing.clone(),
            policies: fixture.policies.clone(),
        }
        .check(&mut ctx)
        .await?;
        assert!(
            PreRequestHook::revalidate_context(&fixture.binding, &ctx)
                .await
                .is_err()
        );
        assert!(
            RouteHook::revalidate_context(&fixture.binding, &[], &ctx)
                .await
                .is_err()
        );
        assert_eq!(
            serde_json::to_value(execution(&fixture, "rebuild-changed").await?)?,
            execution_before
        );
        assert_eq!(
            serde_json::to_value(saved(&fixture, "rebuild-changed").await?)?,
            inventory_before
        );
        assert!(
            fixture
                .upstream
                .received_requests()
                .await
                .context("requests missing")?
                .is_empty()
        );
    }
    Ok(())
}

#[tokio::test]
async fn another_controller_cannot_repeat_a_completed_execution_or_replace_its_charge() -> Result<()>
{
    let fixture = fixture(Failure::None).await?;
    second_controller(&fixture).await?;
    fixture
        .pipeline
        .execute(request("shared-id", true)?)
        .await?;
    fixture
        .pipeline
        .drain_required_pending_settlements()
        .await?;
    let original = serde_json::to_value(execution(&fixture, "shared-id").await?)?;
    let charge = crate::metering::entities::requests::Entity::find_by_id("shared-id")
        .one(&fixture.db)
        .await?
        .context("original charge missing")?;

    assert!(
        fixture
            .pipeline
            .execute(second_request("shared-id")?)
            .await
            .is_err()
    );
    fixture
        .pipeline
        .drain_required_pending_settlements()
        .await?;
    assert_eq!(
        serde_json::to_value(execution(&fixture, "shared-id").await?)?,
        original
    );
    assert_eq!(
        crate::metering::entities::requests::Entity::find_by_id("shared-id")
            .one(&fixture.db)
            .await?,
        Some(charge)
    );
    assert!(
        fixture
            .tracker
            .inventory()
            .scoped_store("scope", "second-controller")?
            .get::<GatewayRequest>(super::super::super::inventory::INVENTORY_KIND, "shared-id")
            .await?
            .is_none()
    );
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("requests missing")?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn competing_controller_admissions_cannot_initialize_two_attempts() -> Result<()> {
    let fixture = fixture(Failure::None).await?;
    second_controller(&fixture).await?;
    let mut first = PipelineContext::new(request("race", true)?);
    let mut second = PipelineContext::new(second_request("race")?);
    // Deterministically interleave admission: both controller-scoped inventory
    // checks finish before either creates the owner-scoped execution record.
    for ctx in [&mut first, &mut second] {
        NormalizedScope.check(ctx).await?;
        fixture.tracker.check(ctx).await?;
    }
    let first_attempt = first
        .get_event::<GatewayRequestObserved>()
        .context("first admission missing")?
        .request
        .attempt_id
        .clone();
    let second_attempt = second
        .get_event::<GatewayRequestObserved>()
        .context("second admission missing")?
        .request
        .attempt_id
        .clone();
    assert_ne!(first_attempt, second_attempt);
    fixture.binding.check(&mut first).await?;
    let original = serde_json::to_value(execution(&fixture, "race").await?)?;
    assert!(fixture.binding.check(&mut second).await.is_err());
    assert!(
        second
            .get_event::<CanonicalRequestReplayRejected>()
            .is_some()
    );
    assert_eq!(
        serde_json::to_value(execution(&fixture, "race").await?)?,
        original
    );
    assert_eq!(execution(&fixture, "race").await?.attempt_id, first_attempt);
    assert_eq!(
        records::Entity::find()
            .filter(records::Column::Kind.eq(EXECUTION_KIND))
            .count(&fixture.db)
            .await?,
        1
    );
    let losing = fixture
        .tracker
        .inventory()
        .scoped_store("scope", "second-controller")?
        .get::<GatewayRequest>(super::super::super::inventory::INVENTORY_KIND, "race")
        .await?
        .context("losing admission missing")?
        .1;
    assert_eq!(losing.attempt_id, second_attempt);
    assert!(losing.settlement.is_none());
    assert!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("requests missing")?
            .is_empty()
    );
    Ok(())
}
