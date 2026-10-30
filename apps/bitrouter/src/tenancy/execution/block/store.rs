//! Borrowed H transactions. No pool, commit, timer or reconstructed authority.
mod cohort;
use super::SelectionRequest;
use crate::{
    gateway::routing::blocks::{
        control::{ControlState, EvolutionMode},
        store::{RoutingDependencies, SessionEnrollment, selection},
    },
    harness::trajectory::owned::{
        Append, Association, Descriptor, MAX_CAPTURE_BYTES, MAX_EVENTS,
        entities::{events, records},
        io,
        protocol::State,
    },
    storage::{
        capture::{CaptureDirection, CaptureKind},
        evolution::digest,
        execution::RouteIntent,
        operation_budget::{Error, OperationBudget},
        scope::KeyDomain,
        session_identity::SessionIdentity,
    },
    tenancy::{capture::Binding, resources::block::Profile},
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    configuration::ResourceRef, identity::CorrelationId, operation::ErrorCode,
};
use sea_orm::{
    ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
    sea_query::Expr,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;

const CONTROL: &str = "owned_block_control_v1";
const ENROLLMENT: &str = "owned_block_enrollment_v1";
const INTENT: &str = "owned_block_intent_v1";
const MAX_RECORD: usize = 2 * 1024 * 1024;
const MAX_ENROLLMENTS: usize = 64;
const MAX_INTENTS: usize = 512;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {
    version: u32,
    route: ResourceRef,
    profile_digest: String,
    dependency: String,
    state: ControlState,
    report: Option<crate::core::evolution::learning::LearningReport>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct Selection {
    pub capture_head: i64,
    pub descriptor: Descriptor,
    pub correlation: CorrelationId,
    pub session: String,
    pub native: String,
    pub call: u64,
    pub token_pricing_eligible: bool,
    pub intent: RouteIntent,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Enrollment {
    descriptor: Descriptor,
    native: String,
    value: SessionEnrollment,
}

struct Namespace<'a> {
    binding: &'a Binding,
    route: ResourceRef,
    scope: String,
}
impl<'a> Namespace<'a> {
    fn new(binding: &'a Binding) -> Result<Self> {
        let route = binding.block_route().context("Block route absent")?.clone();
        let a = binding.association();
        let d = a.descriptor();
        let scope = a.key(
            KeyDomain::Owner,
            &[
                "owned-block/v1",
                &d.principal,
                &d.revision,
                &digest(&route)?,
            ],
        )?;
        Ok(Self {
            binding,
            route,
            scope,
        })
    }
    fn id(&self, kind: &str, logical: &str) -> Result<String> {
        self.binding
            .association()
            .key(KeyDomain::Owner, &[&self.scope, kind, logical])
    }
    fn query(&self, kind: &str) -> sea_orm::Select<records::Entity> {
        records::Entity::find()
            .filter(self.binding.association().predicate("checkpoint_evolution"))
            .filter(records::Column::ScopeId.eq(&self.scope))
            .filter(records::Column::Kind.eq(kind))
    }
    fn decode<T: DeserializeOwned>(
        &self,
        row: &records::Model,
        budget: &OperationBudget<'_>,
    ) -> Result<T> {
        budget.charge(
            0,
            row.body
                .len()
                .checked_mul(2)
                .context("Block decode budget overflow")?,
        )?;
        let a = self.binding.association();
        a.validate(
            row.installation_id.as_deref(),
            row.tenant_id.as_deref(),
            row.creator_principal_id.as_deref(),
        )?;
        ensure!(
            row.scope_id == self.scope && row.revision >= 0,
            "Block row namespace mismatch"
        );
        let sealed = serde_json::from_str(&row.body)?;
        self.binding.service().resources.open_block(
            &(
                &self.scope,
                &row.record_id,
                &row.kind,
                &row.session_key,
                row.revision,
            ),
            &sealed,
        )
    }
    async fn get<T: DeserializeOwned>(
        &self,
        tx: &DatabaseTransaction,
        kind: &str,
        key: &str,
        budget: &OperationBudget<'_>,
    ) -> Result<Option<(records::Model, T)>> {
        let row = io::one(
            tx,
            self.query(kind)
                .filter(records::Column::RecordId.eq(self.id(kind, key)?))
                .lock_exclusive(),
            MAX_RECORD + 4096,
            budget,
        )
        .await?;
        row.map(|row| {
            ensure!(
                row.record_id == self.id(kind, key)? && row.kind == kind,
                "Block record identity changed"
            );
            let value = self.decode(&row, budget)?;
            Ok((row, value))
        })
        .transpose()
    }
    async fn put<T: Serialize>(
        &self,
        tx: &DatabaseTransaction,
        record: (&str, &str),
        session: Option<&str>,
        value: &T,
        old: Option<&records::Model>,
        budget: &OperationBudget<'_>,
    ) -> Result<()> {
        let (kind, key) = record;
        let id = self.id(kind, key)?;
        let revision = old.map_or(Ok(0), |row| {
            row.revision
                .checked_add(1)
                .context("Block revision overflow")
        })?;
        let sealed = self
            .binding
            .service()
            .resources
            .seal_block(&(&self.scope, &id, kind, session, revision), value)?;
        let body = io::json(&sealed, MAX_RECORD).map_err(|_| Error::Capacity)?;
        budget.charge(1, body.len() + 4096)?;
        let changed = if let Some(old) = old {
            records::Entity::update_many()
                .filter(self.binding.association().predicate("checkpoint_evolution"))
                .filter(records::Column::RecordId.eq(&id))
                .filter(records::Column::ScopeId.eq(&self.scope))
                .filter(records::Column::Kind.eq(kind))
                .filter(records::Column::Revision.eq(old.revision))
                .col_expr(records::Column::Body, Expr::value(body))
                .col_expr(records::Column::Revision, Expr::value(revision))
                .exec(tx)
                .await?
                .rows_affected
        } else {
            let metadata = self.binding.association().metadata()?;
            records::Entity::insert(records::ActiveModel {
                record_id: Set(id),
                scope_id: Set(self.scope.clone()),
                kind: Set(kind.into()),
                session_key: Set(session.map(str::to_owned)),
                revision: Set(revision),
                body: Set(body),
                installation_id: Set(metadata.installation),
                tenant_id: Set(metadata.tenant),
                creator_principal_id: Set(metadata.creator),
            })
            .exec_without_returning(tx)
            .await?
        };
        ensure!(changed == 1, "Block record CAS failed");
        Ok(())
    }
}

fn validate(control: &Control, profile: &Profile, ns: &Namespace<'_>) -> Result<()> {
    ensure!(
        control.version == 1
            && control.route == ns.route
            && control.profile_digest == digest(profile)?
            && control.dependency == digest(&(&ns.route, &ns.scope, "immutable-block-routing-v1"))?
            && control.state.blocks.len() == 1
            && control.state.archived_experiments.is_empty()
            && control.state.publications.len() <= 128,
        "Block control identity mismatch"
    );
    let block = control
        .state
        .blocks
        .get(&profile.definition.block_id)
        .context("Block control absent")?;
    ensure!(
        digest(&block.definition)? == digest(&profile.definition)?
            && block.routing_config_digest == control.dependency,
        "Block definition changed"
    );
    Ok(())
}

pub(in crate::tenancy) async fn initialize(
    tx: &DatabaseTransaction,
    binding: &Binding,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    let Some(profile) = binding.block_profile() else {
        return Ok(());
    };
    let ns = Namespace::new(binding)?;
    if let Some((_, current)) = ns.get::<Control>(tx, CONTROL, "control", budget).await? {
        return validate(&current, profile, &ns);
    }
    let dependency = digest(&(&ns.route, &ns.scope, "immutable-block-routing-v1"))?;
    let mut state = ControlState::default();
    state.register(profile.definition.clone(), dependency.clone())?;
    state.set_mode(EvolutionMode::Manual, None)?;
    let current = Control {
        version: 1,
        route: ns.route.clone(),
        profile_digest: digest(profile)?,
        dependency,
        state,
        report: None,
    };
    ns.put(tx, (CONTROL, "control"), None, &current, None, budget)
        .await
}

/// Reconstruct the original correlation state and child edges, not just its label.
async fn live_prompt(
    tx: &DatabaseTransaction,
    a: &Association,
    budget: &OperationBudget<'_>,
) -> Result<(State, String, u64, bool)> {
    let (row, expected) = a.connection_in(tx, budget).await?;
    let rows = io::rows(
        tx,
        a.events().order_by_asc(events::Column::Sequence),
        MAX_EVENTS,
        MAX_CAPTURE_BYTES + 4096 * MAX_EVENTS,
        budget,
    )
    .await?;
    ensure!(
        rows.len() == usize::try_from(row.head)?,
        "Block capture has a gap"
    );
    let mut state = State::initial(a);
    let mut prompts = BTreeMap::<String, usize>::new();
    let mut observed_results = std::collections::BTreeSet::new();
    for row in rows {
        a.validate(
            row.installation_id.as_deref(),
            row.tenant_id.as_deref(),
            row.creator_principal_id.as_deref(),
        )?;
        ensure!(!row.replay, "Block cannot adopt replayed capture");
        let append = Append::new(
            row.sequence,
            row.captured_at,
            serde_json::from_str(&row.event_json)?,
        )?;
        ensure!(
            io::json(
                &append.event,
                crate::harness::trajectory::owned::MAX_EVENT_BYTES
            )? == row.event_json,
            "Block capture encoding changed"
        );
        let (next, native, _) = state.next(&append)?;
        let key = native
            .as_deref()
            .map(|native| a.session_key(native))
            .transpose()?;
        ensure!(
            row.session_key == key
                && row.session_sequence
                    == native
                        .as_ref()
                        .and_then(|native| next.sessions.get(native))
                        .copied(),
            "Block capture session edge changed"
        );
        if let Some(native) = native {
            if append.event.method == "session/prompt" {
                if append.event.kind == CaptureKind::Request {
                    *prompts.entry(native.clone()).or_default() += 1;
                } else if append.event.kind == CaptureKind::Response {
                    observed_results.insert(native.clone());
                }
            }
            if append.event.method.starts_with("fs/")
                || append.event.method.starts_with("terminal/")
                || (append.event.method == "session/update"
                    && !crate::storage::acp_history::is_administrative_update(
                        &append.event.payload,
                    ))
            {
                observed_results.insert(native);
            }
        }
        state = next;
    }
    ensure!(state == expected, "Block capture state changed");
    ensure!(
        state.connected && !state.closed,
        Error::Rejected(ErrorCode::Forbidden)
    );
    let mut pending = state.calls.iter().filter(|(_, call)| {
        call.method == "session/prompt" && call.direction == CaptureDirection::Client
    });
    let (call, request) = pending
        .next()
        .ok_or(Error::Rejected(ErrorCode::Forbidden))?;
    ensure!(
        pending.next().is_none(),
        Error::Rejected(ErrorCode::InvalidRequest)
    );
    let native = request
        .session
        .as_ref()
        .context("Block prompt session absent")?
        .clone();
    let call = *call;
    ensure!(
        state.pinned.contains(&native),
        Error::Rejected(ErrorCode::InvalidRequest)
    );
    let already_started =
        prompts.get(&native).copied().unwrap_or(0) != 1 || observed_results.contains(&native);
    Ok((state, native, call, already_started))
}

pub(in crate::tenancy) async fn select(
    tx: &DatabaseTransaction,
    binding: &Binding,
    request: &SelectionRequest,
    budget: &OperationBudget<'_>,
) -> Result<Selection> {
    request.execution.revalidate().map_err(Error::Rejected)?;
    let profile = binding.block_profile().context("Block profile absent")?;
    let ns = Namespace::new(binding)?;
    let (control_row, mut control) = ns
        .get::<Control>(tx, CONTROL, "control", budget)
        .await?
        .context("Block control was not initialized by opener")?;
    validate(&control, profile, &ns)?;
    cohort::refresh(tx, &ns, profile, &mut control, budget).await?;
    let (capture, native, call, started) = live_prompt(tx, binding.association(), budget).await?;
    let session = binding
        .association()
        .session_in(tx, &native, budget)
        .await?;
    ensure!(
        capture.sessions.get(&native) == Some(&session.head),
        "Block session head mismatch"
    );
    let identity = SessionIdentity {
        owner: session.owner,
        source: session.source,
        native_session_id: native.clone(),
    };
    let id = request.execution.id().as_str();
    // An actual execution invokes Entry once. A lost reply never authorizes a draw.
    ensure!(
        ns.get::<Selection>(tx, INTENT, id, budget).await?.is_none(),
        "duplicate Block execution intent"
    );
    let mut enrolled = ns
        .get::<Enrollment>(tx, ENROLLMENT, &session.session_key, budget)
        .await?;
    let is_new = enrolled.is_none();
    let old = enrolled.as_ref().map(|(row, _)| row.clone());
    let enrollment = if let Some((_, value)) = enrolled.take() {
        ensure!(
            value.descriptor == *binding.association().descriptor() && value.native == native,
            "Block enrollment capture changed"
        );
        value.value
    } else {
        let all = io::rows(
            tx,
            ns.query(ENROLLMENT),
            MAX_ENROLLMENTS,
            8 * 1024 * 1024,
            budget,
        )
        .await?;
        ensure!(all.len() < MAX_ENROLLMENTS, Error::Capacity);
        SessionEnrollment {
            identity: identity.clone(),
            first_watermark: session.head,
            first_request_id: id.into(),
            context: request.context.clone(),
            assignments: BTreeMap::new(),
            block_experiments: Some(
                control
                    .state
                    .blocks
                    .iter()
                    .map(|(id, block)| (id.clone(), block.experiment_id.clone()))
                    .collect(),
            ),
            adopted_blocks: if started {
                BTreeMap::new()
            } else {
                control
                    .state
                    .blocks
                    .iter()
                    .filter(|(_, block)| {
                        block.status
                            == crate::gateway::routing::blocks::control::BlockStatus::Adopted
                    })
                    .map(|(id, block)| (id.clone(), block.experiment_id.clone()))
                    .collect()
            },
            monitoring: BTreeMap::new(),
            admission_reason: if started {
                "session_already_started"
            } else {
                "new_session"
            }
            .into(),
        }
    };
    ensure!(
        enrollment.identity == identity,
        "Block enrollment belongs to another session"
    );
    let count = io::rows(tx, ns.query(INTENT), MAX_INTENTS, 8 * 1024 * 1024, budget).await?;
    ensure!(count.len() < MAX_INTENTS, Error::Capacity);
    let dependencies = RoutingDependencies {
        current: BTreeMap::from([(
            profile.definition.block_id.clone(),
            control.dependency.clone(),
        )]),
        archived: BTreeMap::new(),
        trial_ready: true,
        expected_control_generation: Some(control.state.generation),
    };
    let (enrollment, intent) = selection::select(
        &mut control.state,
        selection::Input {
            session_key: &session.session_key,
            identity: &identity,
            request_id: id,
            context: request.context.clone(),
            dependencies: &dependencies,
            bypass: None,
            watermark: session.head,
            enrollment,
            is_new,
        },
        || {
            #[cfg(all(test, unix))]
            if let Some(seed) = binding.service().next_block_seed_for_test() {
                return seed;
            }
            rand::random()
        },
        || chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )?;
    let selected = Selection {
        capture_head: capture.head,
        descriptor: binding.association().descriptor().clone(),
        correlation: request.execution.submission.binding.correlation_id.clone(),
        session: session.session_key,
        native,
        call,
        token_pricing_eligible: request.token_pricing_eligible,
        intent,
    };
    budget.check()?;
    if is_new {
        let enrollment = Enrollment {
            descriptor: selected.descriptor.clone(),
            native: selected.native.clone(),
            value: enrollment,
        };
        ns.put(
            tx,
            (ENROLLMENT, &selected.session),
            Some(&selected.session),
            &enrollment,
            old.as_ref(),
            budget,
        )
        .await?;
    }
    ns.put(
        tx,
        (INTENT, id),
        Some(&selected.session),
        &selected,
        None,
        budget,
    )
    .await?;
    ns.put(
        tx,
        (CONTROL, "control"),
        None,
        &control,
        Some(&control_row),
        budget,
    )
    .await?;
    Ok(selected)
}

/// The actual checked checkpoint consumer refreshes the same bounded control.
/// No timer or paid job is created here; missing/unknown evidence remains unknown.
pub(in crate::tenancy) async fn checkpoint(
    tx: &DatabaseTransaction,
    binding: &Binding,
    budget: &OperationBudget<'_>,
) -> Result<Option<u64>> {
    let Some(profile) = binding.block_profile() else {
        return Ok(None);
    };
    let ns = Namespace::new(binding)?;
    let (row, mut control) = ns
        .get::<Control>(tx, CONTROL, "control", budget)
        .await?
        .context("Block control absent")?;
    validate(&control, profile, &ns)?;
    if cohort::refresh(tx, &ns, profile, &mut control, budget).await? {
        ns.put(tx, (CONTROL, "control"), None, &control, Some(&row), budget)
            .await?;
    }
    Ok(Some(control.state.generation))
}
