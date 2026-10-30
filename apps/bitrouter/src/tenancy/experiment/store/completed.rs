//! Complete native source closure for the real Human/paid assessment consumers.
use super::*;
use crate::{
    core::eval::{compiler::EvalEvidenceRecord, store::EvalStore},
    harness::resources::managed::store::model::completed::CompletedRun,
    tenancy::execution::session::Binding,
    trajectory::store::owned::{OwnedStore, io::Budget},
};
use bitrouter_ai::tenancy::operation::ErrorCode;

pub(in crate::tenancy) enum Reader<'a> {
    Session(&'a Binding),
    // Constructed only for an existing admitted paid owner's internal finalizer.
    Finalization(&'a Binding),
    Learner(&'a crate::tenancy::learner::identity::ReadIdentity),
    PaidRecovery(&'a crate::tenancy::learning::recovery::RecoveryIdentity),
}
impl Reader<'_> {
    fn check(&self) -> Result<()> {
        match self {
            Self::Session(binding) => crate::tenancy::experiment::queue::check_reader(binding),
            Self::Finalization(binding) => {
                if binding
                    .authority()
                    .origin(&binding.authority().host().service().cleanup)
                {
                    Ok(())
                } else {
                    Err(ErrorCode::AuthorityUnavailable)
                }
            }
            Self::Learner(identity) => identity.check(&[]),
            Self::PaidRecovery(_) => Ok(()),
        }
        .map_err(Error::Rejected)?;
        Ok(())
    }
    fn scope(&self) -> Scope {
        match self {
            Self::Session(binding) | Self::Finalization(binding) => {
                Scope::for_session(binding.authority()).0
            }
            Self::Learner(identity) => Scope::for_learner(identity).0,
            Self::PaidRecovery(identity) => Scope::for_paid_recovery(identity).0,
        }
    }
    fn eval(&self) -> EvalStore {
        match self {
            Self::Session(binding) | Self::Finalization(binding) => {
                EvalStore::for_native_session(binding.authority())
            }
            Self::Learner(identity) => EvalStore::for_learner(identity),
            Self::PaidRecovery(identity) => EvalStore::for_paid_recovery(identity),
        }
    }
    fn trajectory(&self) -> OwnedStore {
        match self {
            Self::Session(binding) | Self::Finalization(binding) => {
                OwnedStore::for_native_session(binding.authority())
            }
            Self::Learner(identity) => OwnedStore::for_learner(identity),
            Self::PaidRecovery(identity) => OwnedStore::for_paid_recovery(identity),
        }
    }
}

/// Serializable source facts are commitments, never authorization handles.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct Edge {
    pub version: u32,
    pub unit: String,
    pub session: String,
    pub run: String,
    pub checkpoint: String,
    pub prefix: String,
    pub head: bitrouter_ai::core::checkpoint::DurableHead,
    pub closure: String,
}

impl Edge {
    pub(in crate::tenancy) fn matches(&self, current: &Self) -> bool {
        self.version == current.version
            && self.unit == current.unit
            && self.session == current.session
            && self.run == current.run
            && self.checkpoint == current.checkpoint
            && self.prefix == current.prefix
            && self.closure == current.closure
            && self.head.execution_epoch == current.head.execution_epoch
            && self.head.state_revision <= current.head.state_revision
            && self.head.event_seq <= current.head.event_seq
            && (self.head.state_revision != current.head.state_revision
                || self.head == current.head)
    }
}

struct RetainedAudit {
    edge: Edge,
    packet: crate::core::evolution::evidence::EvidencePacket,
    descriptor: crate::tenancy::execution::session::Descriptor,
}
enum CompleteSource {
    Journal(Box<CompletedRun>),
    Audit(Box<RetainedAudit>),
}
impl CompleteSource {
    fn run(&self) -> &str {
        match self {
            Self::Journal(v) => v.run(),
            Self::Audit(value) => &value.edge.run,
        }
    }
    fn harness(&self) -> &str {
        match self {
            Self::Journal(v) => v.harness(),
            Self::Audit(value) => &value.descriptor.harness,
        }
    }
    fn session(&self) -> &str {
        match self {
            Self::Journal(v) => v.session(),
            Self::Audit(value) => &value.descriptor.session,
        }
    }
    fn principal(&self) -> &str {
        match self {
            Self::Journal(v) => v.principal(),
            Self::Audit(value) => value.descriptor.principal.as_str(),
        }
    }
    fn checkpoint(&self) -> &str {
        match self {
            Self::Journal(v) => v.checkpoint(),
            Self::Audit(value) => &value.edge.checkpoint,
        }
    }
    fn head(&self) -> &bitrouter_ai::core::checkpoint::DurableHead {
        match self {
            Self::Journal(v) => v.head(),
            Self::Audit(value) => &value.edge.head,
        }
    }
    fn packet(&self) -> &crate::core::evolution::evidence::EvidencePacket {
        match self {
            Self::Journal(v) => v.packet(),
            Self::Audit(value) => &value.packet,
        }
    }
    fn into_packet(self) -> crate::core::evolution::evidence::EvidencePacket {
        match self {
            Self::Journal(v) => (*v).into_packet(),
            Self::Audit(value) => value.packet,
        }
    }
}

pub(in crate::tenancy) struct Completion {
    pub(super) unit: Unit,
    packet: crate::core::evolution::evidence::EvidencePacket,
    edge: Edge,
    records: Vec<EvalEvidenceRecord>,
    historical_audit: bool,
}
impl Completion {
    pub(in crate::tenancy) fn matches_source(&self, edge: &Edge) -> bool {
        if self.historical_audit {
            // No new publication or current-run capability uses this branch.
            // Audit may see a later Human source head from the same immutable
            // run; both directions still bind all content and the original epoch.
            edge.matches(&self.edge) || self.edge.matches(edge)
        } else {
            edge.matches(&self.edge)
        }
    }
    pub(in crate::tenancy) fn edge(&self) -> &Edge {
        &self.edge
    }
    pub(in crate::tenancy) fn packet(&self) -> &crate::core::evolution::evidence::EvidencePacket {
        &self.packet
    }
    pub(in crate::tenancy) fn records(&self) -> &[EvalEvidenceRecord] {
        &self.records
    }
    pub(in crate::tenancy) fn profile(&self) -> &Profile {
        &self.unit.profile
    }
    pub(in crate::tenancy) fn principal(&self) -> &str {
        &self.unit.principal
    }
}

pub(in crate::tenancy) async fn closed(
    tx: &DatabaseTransaction,
    binding: &Binding,
    run: &str,
    budget: &OperationBudget<'_>,
) -> Result<Completion> {
    let loaded = crate::managed_store::owned::load_for_model_in(tx, binding.authority(), budget)
        .await?
        .ok_or(Error::Rejected(ErrorCode::NotFound))?;
    let source = CompletedRun::stopped(&loaded, run, budget)?;
    let scope = Scope::for_session(binding.authority()).0;
    let descriptor = binding.descriptor();
    let unit = scope.physical_key(
        KeyDomain::NativeExperimentUnit,
        "",
        &[
            descriptor.principal.as_str(),
            &descriptor.harness,
            &descriptor.session,
            run,
        ],
    )?;
    verify(
        tx,
        Reader::Finalization(binding),
        CompleteSource::Journal(Box::new(source)),
        &unit,
        &scope_session(binding)?,
        budget,
    )
    .await
}

pub(in crate::tenancy) async fn finalizing(
    tx: &DatabaseTransaction,
    binding: &Binding,
    budget: &OperationBudget<'_>,
) -> Result<Completion> {
    current(tx, binding, Reader::Finalization(binding), budget).await
}

pub(in crate::tenancy) async fn live(
    tx: &DatabaseTransaction,
    binding: &Binding,
    budget: &OperationBudget<'_>,
) -> Result<Completion> {
    current(tx, binding, Reader::Session(binding), budget).await
}

async fn current<'a>(
    tx: &DatabaseTransaction,
    binding: &'a Binding,
    reader: Reader<'a>,
    budget: &OperationBudget<'_>,
) -> Result<Completion> {
    reader.check()?;
    let loaded = crate::managed_store::owned::load_for_model_in(tx, binding.authority(), budget)
        .await?
        .ok_or(Error::Rejected(ErrorCode::NotFound))?;
    let completed = CompletedRun::live(binding, &loaded, budget)?;
    let scope = Scope::for_session(binding.authority()).0;
    let descriptor = binding.descriptor();
    let key = scope.physical_key(
        KeyDomain::NativeExperimentUnit,
        "",
        &[
            descriptor.principal.as_str(),
            &descriptor.harness,
            &descriptor.session,
            completed.run(),
        ],
    )?;
    let session = scope.physical_key(
        KeyDomain::NativeSession,
        "",
        &[descriptor.principal.as_str(), &descriptor.name],
    )?;
    verify(
        tx,
        reader,
        CompleteSource::Journal(Box::new(completed)),
        &key,
        &session,
        budget,
    )
    .await
}

pub(in crate::tenancy) async fn retained(
    tx: &DatabaseTransaction,
    reader: Reader<'_>,
    edge: &Edge,
    budget: &OperationBudget<'_>,
) -> Result<Completion> {
    ensure!(
        edge.version == 1,
        "native assessment source version changed"
    );
    let completed = match &reader {
        Reader::Session(binding) => {
            let loaded =
                crate::managed_store::owned::load_for_review_in(tx, binding, &edge.session, budget)
                    .await?
                    .ok_or(Error::Rejected(ErrorCode::NotFound))?;
            if loaded.descriptor == *binding.descriptor() {
                CompletedRun::live(binding, &loaded, budget)?
            } else {
                CompletedRun::stopped(&loaded, &edge.run, budget)?
            }
        }
        Reader::Finalization(binding) => {
            let loaded =
                crate::managed_store::owned::load_for_model_in(tx, binding.authority(), budget)
                    .await?
                    .ok_or(Error::Rejected(ErrorCode::NotFound))?;
            ensure!(
                scope_session(binding)? == edge.session,
                "native finalization changed source session"
            );
            CompletedRun::live(binding, &loaded, budget)?
        }
        Reader::Learner(identity) => {
            let loaded = crate::managed_store::owned::load_for_learner_in(
                tx,
                identity,
                &edge.session,
                budget,
            )
            .await?
            .ok_or(Error::Rejected(ErrorCode::NotFound))?;
            identity.check_source(&loaded).map_err(Error::Rejected)?;
            CompletedRun::stopped(&loaded, &edge.run, budget)?
        }
        Reader::PaidRecovery(_) => anyhow::bail!("paid audit must use its retained source path"),
    };
    let mut completion = verify(
        tx,
        reader,
        CompleteSource::Journal(Box::new(completed)),
        &edge.unit,
        &edge.session,
        budget,
    )
    .await?;
    ensure!(
        edge.matches(&completion.edge),
        Error::Rejected(ErrorCode::RevisionConflict)
    );
    completion.edge = edge.clone();
    Ok(completion)
}

async fn verify(
    tx: &DatabaseTransaction,
    reader: Reader<'_>,
    source: CompleteSource,
    key: &str,
    session: &str,
    budget: &OperationBudget<'_>,
) -> Result<Completion> {
    reader.check()?;
    let scope = reader.scope();
    let row = records::point(tx, &scope, key, &[UNIT], budget)
        .await?
        .ok_or(Error::Rejected(ErrorCode::CapabilityMismatch))?;
    let partition = scope.physical_key(KeyDomain::NativeExperimentPending, "", &[])?;
    let unit: Unit = records::decode(&row, &partition, Some(session))?;
    ensure!(
        unit.pending == 0
            && unit.run == source.run()
            && unit.key == key
            && unit.harness == source.harness()
            && unit.session == source.session()
            && unit.principal == source.principal(),
        Error::Rejected(ErrorCode::InvalidRequest)
    );
    if let Reader::Session(binding) | Reader::Finalization(binding) = &reader {
        let descriptor = binding.descriptor();
        ensure!(
            unit.profile.revision == descriptor.revision,
            "native completed unit owner changed"
        );
    }
    let steps = records::steps(tx, &scope, key, session, budget).await?;
    ensure!(
        steps.len() == unit.steps as usize
            && !steps.is_empty()
            && match &source {
                CompleteSource::Journal(source) => steps.len() == source.executions().len(),
                CompleteSource::Audit(_) => true,
            },
        "native completed unit omitted executions"
    );
    let counter = records::point(tx, &scope, &unit.counter, &[COUNTER], budget)
        .await?
        .context("native completed unit counter absent")?;
    let value: Counter = records::decode(&counter, &partition, None)?;
    ensure!(
        value.version == 1
            && value.definition == unit.profile.definition_digest
            && value.experiment == unit.profile.treatment.experiment_id
            && value.accepted > 0
            && value.accepted <= MAX_UNITS
            && value.challenger <= value.accepted,
        "native completed counter changed"
    );
    let eval = reader.eval();
    let trajectory = reader.trajectory();
    let mut trajectory_budget = Budget::owned(budget);
    let mut closure = Vec::with_capacity(steps.len());
    let mut publications = Vec::with_capacity(steps.len());
    for (row, step) in steps {
        records::linked_unit(tx, &scope, &row, &step, budget).await?;
        ensure!(step.complete(), Error::Rejected(ErrorCode::InvalidRequest));
        let expected = step
            .ack
            .as_ref()
            .context("native completed step ACK absent")?;
        if let CompleteSource::Journal(source) = &source {
            let ack = source.settled(&step.facts, step.execution.as_str())?;
            ensure!(
                !ack.interrupted
                    && ack.planned
                    && ack.terminal_step == expected.terminal_step
                    && ack.head.execution_epoch == expected.head.execution_epoch
                    && ack.head.state_revision >= expected.head.state_revision
                    && ack.head.event_seq >= expected.head.event_seq,
                "native completed step ACK changed"
            );
        } else {
            // This path cannot issue/read a live session. The original complete
            // ACK is part of the exact immutable closure authenticated at Accept.
            ensure!(
                !expected.interrupted
                    && expected.planned
                    && expected.head.execution_epoch == source.head().execution_epoch
                    && expected.head.state_revision <= source.head().state_revision
                    && expected.head.event_seq <= source.head().event_seq,
                "retained native ACK changed"
            );
        }
        let candidate = match &reader {
            Reader::Session(binding) | Reader::Finalization(binding) => {
                crate::tenancy::receipts::store::native_candidate_in(
                    tx,
                    binding.authority(),
                    &step.execution,
                    budget,
                )
                .await?
            }
            Reader::Learner(identity) => {
                crate::tenancy::receipts::store::learner_candidate_in(
                    tx,
                    identity,
                    &step.execution,
                    budget,
                )
                .await?
            }
            Reader::PaidRecovery(identity) => {
                crate::tenancy::receipts::store::paid_recovery_candidate_in(
                    tx,
                    identity,
                    &step.execution,
                    budget,
                )
                .await?
            }
        }
        .context("native completed receipt absent")?;
        let (digest, dispatch) = candidate.experiment_disposition()?;
        let receipt = step
            .receipt
            .as_ref()
            .context("native completed receipt link absent")?;
        ensure!(
            digest == receipt.digest
                && dispatch == receipt.dispatch
                && candidate.receipt.state == receipt.state
                && candidate.receipt.execution_revision == unit.profile.revision
                && candidate.receipt.principal_id.as_str() == unit.principal,
            "native completed receipt link changed"
        );
        let Publication::Published {
            event,
            subject,
            result,
            outbox,
        } = &step.publication
        else {
            return Err(Error::Rejected(ErrorCode::InvalidRequest).into());
        };
        let record = eval
            .native_publication_in(tx, subject, result, budget)
            .await?;
        ensure!(
            record.subject.subject_id == unit.episode
                && record
                    .subject
                    .decisions
                    .iter()
                    .all(
                        |decision| decision.experiment.as_ref() == Some(&unit.assignment)
                            && decision.policy_digest == unit.profile.policy_digest
                    ),
            "native completed treatment changed"
        );
        let request = step
            .request
            .as_deref()
            .context("native completed request absent")?;
        trajectory
            .check_native_publication_in(
                tx,
                &crate::tenancy::experiment::PublicationRef {
                    request,
                    event,
                    outbox,
                },
                &record,
                &mut trajectory_budget,
            )
            .await?;
        closure.push(crate::storage::evaluation::canonical_digest(&step)?);
        publications.push(record);
    }
    closure.sort();
    let edge = Edge {
        version: 1,
        unit: key.into(),
        session: session.into(),
        run: source.run().into(),
        checkpoint: source.checkpoint().into(),
        prefix: source.packet().prefix_digest.clone(),
        head: source.head().clone(),
        // Counter totals may grow as other genuine runs enroll. They are checked
        // above but are not the immutable identity of this complete unit.
        closure: crate::storage::evaluation::canonical_digest(&(&unit, closure))?,
    };
    budget.check()?;
    reader.check()?;
    let historical_audit = matches!(&source, CompleteSource::Audit(_));
    Ok(Completion {
        historical_audit,
        unit,
        packet: source.into_packet(),
        edge,
        records: publications,
    })
}

/// Historical audit of an already accepted immutable packet. It is not a
/// public review source and cannot produce a current-run contribution.
pub(in crate::tenancy) async fn audit_paid(
    tx: &DatabaseTransaction,
    identity: &crate::tenancy::learning::recovery::RecoveryIdentity,
    native: &crate::tenancy::learning::native::Seed,
    packet: crate::core::evolution::evidence::EvidencePacket,
    budget: &OperationBudget<'_>,
) -> Result<Completion> {
    let (installation, tenant, principal) = identity.storage_identity();
    ensure!(
        &native.descriptor.installation == installation
            && &native.descriptor.tenant == tenant
            && &native.descriptor.principal == principal
            && packet.checkpoint_id == native.source.checkpoint
            && packet.prefix_digest == native.source.prefix
            && packet.projection_version
                == crate::core::evolution::evidence::NATIVE_EVIDENCE_VERSION,
        "paid native audit namespace changed"
    );
    ensure!(
        packet.gaps.is_empty()
            && !packet.items.is_empty()
            && packet.items.len() <= 4096
            && crate::storage::evaluation::canonical_digest(&(
                crate::core::evolution::evidence::NATIVE_EVIDENCE_VERSION,
                &packet.checkpoint_id,
                &packet.items
            ))? == packet.prefix_digest,
        "retained native projection differs from accepted prefix"
    );
    let scope = Scope::for_paid_recovery(identity).0;
    ensure!(
        scope.physical_key(
            KeyDomain::NativeSession,
            "",
            &[principal.as_str(), &native.descriptor.name]
        )? == native.source.session
            && scope.physical_key(
                KeyDomain::NativeExperimentUnit,
                "",
                &[
                    principal.as_str(),
                    &native.descriptor.harness,
                    &native.descriptor.session,
                    &native.source.run
                ]
            )? == native.source.unit,
        "paid native source keys changed"
    );
    budget.charge(
        0,
        io::json_len(native, 16 * 1024)?
            .checked_add(1024)
            .context("native audit clone bound")?,
    )?;
    let completion = verify(
        tx,
        Reader::PaidRecovery(identity),
        CompleteSource::Audit(Box::new(RetainedAudit {
            edge: native.source.clone(),
            packet,
            descriptor: native.descriptor.clone(),
        })),
        &native.source.unit,
        &native.source.session,
        budget,
    )
    .await?;
    ensure!(
        completion.edge == native.source
            && completion.profile().revision == native.descriptor.revision,
        "paid immutable native closure changed"
    );
    Ok(completion)
}

pub(in crate::tenancy) async fn stopped_paid(
    tx: &DatabaseTransaction,
    identity: &crate::tenancy::learning::recovery::RecoveryIdentity,
    native: &crate::tenancy::learning::native::Seed,
    budget: &OperationBudget<'_>,
) -> Result<(Completion, String)> {
    let loaded = crate::managed_store::owned::load_for_paid_recovery_in(
        tx,
        identity,
        &native.descriptor,
        budget,
    )
    .await?
    .context("stopped paid native journal absent")?;
    let source = CompletedRun::stopped(&loaded, &native.source.run, budget)?;
    let completion = verify(
        tx,
        Reader::PaidRecovery(identity),
        CompleteSource::Journal(Box::new(source)),
        &native.source.unit,
        &native.source.session,
        budget,
    )
    .await?;
    ensure!(
        native.source.matches(completion.edge()),
        "stopped native paid source changed"
    );
    let digest = crate::storage::evolution::digest(&loaded.journal)?;
    Ok((completion, digest))
}

/// Compare the source with its immutable retained definition. Human evidence can
/// read an authorized operator policy; only successor publication requires a
/// tenant-owned policy, which the learner checks separately.
pub(in crate::tenancy) fn check_retained(
    runtime: &crate::tenancy::resources::execution::RetainedExecutionRevision,
    profile: &Profile,
) -> Result<()> {
    use bitrouter_ai::tenancy::configuration::LearningMode;
    ensure!(
        runtime.document().learning.mode == LearningMode::Explore,
        Error::Rejected(ErrorCode::CapabilityMismatch)
    );
    let route = runtime
        .learning_route(&profile.route)
        .map_err(Error::Rejected)?;
    ensure!(
        route.choice_profile().is_none()
            && route.count_candidate().map_err(Error::Rejected)?.is_none(),
        Error::Rejected(ErrorCode::CapabilityMismatch)
    );
    if let Some(evaluator) = route.assessment_reference() {
        runtime
            .native_paid_recovery_credentials(profile.route.resource_id.as_str(), evaluator)
            .map_err(Error::Rejected)?;
    }
    let policy = route
        .policy()
        .ok_or(Error::Rejected(ErrorCode::CapabilityMismatch))?;
    let definition = policy
        .document()
        .policies
        .get(policy.name())
        .context("native source retained policy absent")?;
    let treatment = definition
        .optimization
        .as_ref()
        .and_then(|value| value.active.as_ref())
        .ok_or(Error::Rejected(ErrorCode::CapabilityMismatch))?;
    let config = policy.configuration().map_err(Error::Rejected)?;
    crate::tenancy::trajectory_profile::guarded(&config, policy.document(), policy.name())
        .map_err(|_| Error::Rejected(ErrorCode::CapabilityMismatch))?;
    ensure!(
        profile.revision == *runtime.revision_id()
            && profile.policy == *policy.reference()
            && profile.policy_name == policy.name()
            && profile.treatment == *treatment
            && profile.policy_digest == crate::policy_lock::semantic_digest(policy.document())?
            && profile.definition_digest
                == crate::storage::evaluation::canonical_digest(&(policy.name(), definition))?,
        "native assessment differs from its retained experiment profile"
    );
    Ok(())
}

fn scope_session(binding: &Binding) -> Result<String> {
    Scope::for_session(binding.authority()).0.physical_key(
        KeyDomain::NativeSession,
        "",
        &[
            binding.descriptor().principal.as_str(),
            &binding.descriptor().name,
        ],
    )
}
