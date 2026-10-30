//! Explicit revision selection prevents retry and checkpoint sample inflation.

use super::*;

async fn head(db: &impl ConnectionTrait, key: &str) -> Result<Option<String>> {
    Ok(heads::Entity::find_by_id(key)
        .filter(crate::storage::legacy::scope("acp_effective_assessments"))
        .one(db)
        .await?
        .and_then(|h| h.revision_id))
}

async fn revision(db: &impl ConnectionTrait, id: &str) -> Result<AssessmentRevision> {
    let row = revisions::Entity::find_by_id(id)
        .filter(crate::storage::legacy::scope("acp_assessment_revisions"))
        .one(db)
        .await?
        .context("assessment revision missing")?;
    Ok(serde_json::from_str(&row.revision_json)?)
}

fn validate_digest(value: &str) -> Result<()> {
    let hex = value.strip_prefix("sha256:").unwrap_or(value);
    ensure!(
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "assessment digests must be lowercase SHA-256 values"
    );
    Ok(())
}

fn validate(input: &RevisionInput, cp: &Checkpoint) -> Result<()> {
    ensure!(
        !input.submission_id.trim().is_empty()
            && !input.evaluator_id.trim().is_empty()
            && !input.evaluator_version.trim().is_empty(),
        "submission and evaluator identity are required"
    );
    if let Some(assessment) = &input.assessment {
        validate_digest(&assessment.pipeline_config_digest)?;
        validate_digest(&assessment.selection_digest)?;
        ensure!(
            !assessment.scores.is_empty(),
            "assessment score vector is empty"
        );
        for (criterion, score) in &assessment.scores {
            ensure!(!criterion.trim().is_empty(), "criterion ID is empty");
            if let CriterionScore::Scored { value_ppm } = score {
                ensure!(
                    *value_ppm <= 1_000_000,
                    "criterion score exceeds one million ppm"
                );
            }
        }
        let references: BTreeMap<_, _> = cp
            .segments
            .iter()
            .flat_map(|s| s.events.iter().chain(&s.setup))
            .map(|r| (r.node_id(), &r.digest))
            .collect();
        for cite in &assessment.evidence {
            ensure!(
                references
                    .get(&cite.node_id)
                    .is_some_and(|digest| **digest == cite.digest),
                "evidence citation is not in this checkpoint or its digest differs"
            );
        }
    } else {
        ensure!(
            !input.reason.trim().is_empty(),
            "retraction requires a reason"
        );
        ensure!(
            input.source == AssessmentSource::Human,
            "automatic evaluation cannot retract a human-selected history"
        );
    }
    Ok(())
}

impl CanonicalStore {
    /// A submission key is idempotent. Explicit compare-and-set prevents an old
    /// worker from overwriting a correction made while that worker was running.
    pub async fn submit_assessment(
        &self,
        identity: &SessionIdentity,
        input: RevisionInput,
    ) -> Result<AssessmentRevision> {
        self.submit_assessment_checked(identity, input, |_| Box::pin(async { Ok(()) }))
            .await
    }

    /// App-owned automatic workers may fence selection against their control
    /// epoch. The check runs in the same transaction, before canonical locks.
    /// It may only inspect/lock local state, never perform network/model work.
    pub(crate) async fn submit_assessment_checked<F>(
        &self,
        identity: &SessionIdentity,
        input: RevisionInput,
        precondition: F,
    ) -> Result<AssessmentRevision>
    where
        F: for<'a> FnOnce(&'a DatabaseTransaction) -> futures::future::BoxFuture<'a, Result<()>>
            + Send,
    {
        let cp = self
            .checkpoint_content(identity, &input.checkpoint_id)
            .await?
            .checkpoint;
        validate(&input, &cp)?;
        let key = identity.key()?;
        let revision_id = digest(&(&key, &input.submission_id))?;
        let input_digest = digest(&input)?;
        let mut keys: BTreeSet<_> = cp
            .segments
            .iter()
            .map(|s| s.identity.key())
            .collect::<Result<_>>()?;
        // A historical checkpoint can have a different fork ancestry. Discover
        // its session locks before starting the mutation, without making this
        // read authoritative or rejecting an otherwise idempotent submission.
        // Missing/corrupt hints are validated below only if the expected head
        // actually matches; discovery never grants access across native scope.
        if let Some(expected) = &input.expected_revision
            && let Some(row) = revisions::Entity::find_by_id(expected)
                .filter(crate::storage::legacy::scope("acp_assessment_revisions"))
                .one(&self.db)
                .await?
            && row.session_key == key
            && let Ok(previous) = serde_json::from_str::<AssessmentRevision>(&row.revision_json)
            && let Some(row) = checkpoints::Entity::find_by_id(&previous.input.checkpoint_id)
                .filter(crate::storage::legacy::scope("acp_checkpoints"))
                .one(&self.db)
                .await?
            && row.session_key == key
            && let Ok(previous) = serde_json::from_str::<Checkpoint>(&row.manifest_json)
        {
            for segment in previous.segments {
                if segment.identity.owner == identity.owner
                    && segment.identity.source == identity.source
                    && let Ok(key) = segment.identity.key()
                {
                    keys.insert(key);
                }
            }
        }
        let tx = self.db.begin().await?;
        precondition(&tx).await?;
        let mut locked_sessions = BTreeMap::new();
        for key in keys {
            // Validate deletion when the corresponding manifest is used. An
            // obsolete expected-head hint must not preempt duplicate handling.
            if let Some(row) = lock_session_metadata(&tx, &key).await? {
                locked_sessions.insert(key, row);
            }
        }
        locked_manifest(&tx, identity, &input.checkpoint_id, &locked_sessions).await?;
        if let Some(existing) = revisions::Entity::find_by_id(&revision_id)
            .filter(crate::storage::legacy::scope("acp_assessment_revisions"))
            .lock_exclusive()
            .one(&tx)
            .await?
        {
            let value: AssessmentRevision = serde_json::from_str(&existing.revision_json)?;
            ensure!(
                value.input_digest == input_digest,
                "submission ID already used for different input"
            );
            tx.commit().await?;
            return Ok(value);
        }
        // The session fence does not refresh an older MySQL read view. The
        // selected head and its revision must both be read at that fence.
        let current_head = heads::Entity::find_by_id(&key)
            .filter(crate::storage::legacy::scope("acp_effective_assessments"))
            .lock_exclusive()
            .one(&tx)
            .await?;
        let current_id = current_head
            .as_ref()
            .and_then(|head| head.revision_id.clone());
        ensure!(
            current_id == input.expected_revision,
            "effective revision changed; refresh before submitting"
        );
        let current = match &current_id {
            Some(id) => {
                let row = revisions::Entity::find_by_id(id)
                    .filter(crate::storage::legacy::scope("acp_assessment_revisions"))
                    .lock_exclusive()
                    .one(&tx)
                    .await?
                    .context("assessment revision missing")?;
                Some(serde_json::from_str::<AssessmentRevision>(
                    &row.revision_json,
                )?)
            }
            None => None,
        };
        let mut selected = true;
        let mut selection_reason = "selected";
        if let Some(current) = &current {
            let previous = locked_manifest(
                &tx,
                identity,
                &current.input.checkpoint_id,
                &locked_sessions,
            )
            .await?;
            if input.assessment.is_none() {
                ensure!(
                    current.input.assessment.is_some()
                        && current.input.checkpoint_id == input.checkpoint_id,
                    "retraction must target the current assessment"
                );
                selection_reason = "retracted";
            } else if cp.watermark < previous.watermark {
                selected = false;
                selection_reason = "historical_checkpoint";
            } else if cp.checkpoint_id == previous.checkpoint_id
                && current.input.source == AssessmentSource::Human
                && input.source == AssessmentSource::Agentic
            {
                selected = false;
                selection_reason = "human_revision_preserved";
            }
        } else {
            ensure!(
                input.assessment.is_some(),
                "no current assessment to retract"
            );
        }
        let value = AssessmentRevision {
            revision_id: revision_id.clone(),
            input_digest,
            input,
            supersedes: if selected { current_id } else { None },
            selected_on_submission: selected,
            selection_reason: selection_reason.into(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        let inserted = revisions::Entity::insert(revisions::ActiveModel {
            revision_id: Set(revision_id.clone()),
            session_key: Set(key.clone()),
            checkpoint_id: Set(cp.checkpoint_id),
            created_at: Set(value.created_at.clone()),
            revision_json: Set(serde_json::to_string(&value)?),
        })
        .exec_without_returning(&tx)
        .await?;
        ensure!(inserted == 1, "revisions row was not inserted");
        if selected {
            if current_head.is_some() {
                let updated = heads::Entity::update_many()
                    .filter(crate::storage::legacy::scope("acp_effective_assessments"))
                    .filter(heads::Column::SessionKey.eq(&key))
                    .col_expr(heads::Column::RevisionId, Expr::value(Some(revision_id)))
                    .exec(&tx)
                    .await?;
                ensure!(updated.rows_affected == 1, "assessment head changed");
            } else {
                // The locked session serializes Legacy creators. A foreign occupied
                // key is an error, never an upsert into that namespace.
                let inserted = heads::Entity::insert(heads::ActiveModel {
                    session_key: Set(key),
                    revision_id: Set(Some(revision_id)),
                })
                .exec_without_returning(&tx)
                .await?;
                ensure!(inserted == 1, "heads row was not inserted");
            }
        }
        tx.commit().await?;
        Ok(value)
    }

    pub async fn assessment_history(
        &self,
        identity: &SessionIdentity,
    ) -> Result<Vec<AssessmentRevision>> {
        session(&self.db, &identity.key()?).await?;
        let rows = revisions::Entity::find()
            .filter(crate::storage::legacy::scope("acp_assessment_revisions"))
            .filter(revisions::Column::SessionKey.eq(identity.key()?))
            .order_by_asc(revisions::Column::CreatedAt)
            .order_by_asc(revisions::Column::RevisionId)
            .all(&self.db)
            .await?;
        let mut result = Vec::new();
        for row in rows {
            manifest(&self.db, identity, &row.checkpoint_id).await?;
            result.push(serde_json::from_str(&row.revision_json)?);
        }
        Ok(result)
    }

    /// Exactly one selected assessment per native session. Stale labels remain
    /// visible, but are explicitly excluded from the current family label count.
    pub async fn effective_assessment(
        &self,
        identity: &SessionIdentity,
    ) -> Result<EffectiveAssessment> {
        let key = identity.key()?;
        let initial = session(&self.db, &key).await?;
        let current_revision = head(&self.db, &key).await?;
        let current = match &current_revision {
            Some(id) => Some(revision(&self.db, id).await?),
            None => None,
        };
        let checkpoint = if let Some(current) = &current {
            Some(
                self.checkpoint_content(identity, &current.input.checkpoint_id)
                    .await?
                    .checkpoint,
            )
        } else {
            self.checkpoints(identity).await?.pop()
        };
        let mut reasons = Vec::new();
        let mut stale = checkpoint
            .as_ref()
            .is_some_and(|cp| cp.watermark != initial.head);
        if stale {
            reasons.push("new_content_unassessed".into());
        }
        let mut resource = None;
        let mut source_states = BTreeMap::new();
        if let Some(cp) = &checkpoint {
            reasons.extend(cp.gaps.clone());
            // A recording failure can leave the canonical head unchanged. Keep
            // the old checkpoint intact, but do not present its prior label as
            // current when its own capture source subsequently became incomplete.
            // Later changes to inherited parents do not alter a child's prefix.
            if let Some(own) = cp.segments.last() {
                for captured in &own.connections {
                    let connection = connections::Entity::find_by_id(&captured.connection_id)
                        .filter(crate::storage::legacy::scope("acp_capture_connections"))
                        .one(&self.db)
                        .await?
                        .context("capture connection missing")?;
                    if connection.state == "interrupted" {
                        reasons.push(format!(
                            "source_capture_interrupted:{}",
                            connection.connection_id
                        ));
                        stale |= captured.state != "interrupted";
                    }
                    source_states.insert(connection.connection_id, connection.state);
                }
            }
            resource = self
                .checkpoint_resource_history(identity, &cp.checkpoint_id)
                .await?
                .pop();
        }
        let assessment = match current {
            Some(value) if value.input.assessment.is_some() => Some(value),
            Some(_) => {
                reasons.push("assessment_retracted".into());
                None
            }
            None => {
                reasons.push("no_assessment".into());
                None
            }
        };
        ensure!(
            session(&self.db, &key).await? == initial
                && head(&self.db, &key).await? == current_revision,
            "effective assessment changed while reading; retry"
        );
        for (id, state) in &source_states {
            ensure!(
                connections::Entity::find_by_id(id)
                    .filter(crate::storage::legacy::scope("acp_capture_connections"))
                    .one(&self.db)
                    .await?
                    .is_some_and(|row| row.state == *state),
                "capture health changed while reading; retry"
            );
        }
        Ok(EffectiveAssessment {
            identity: identity.clone(),
            current_watermark: initial.head,
            current_revision,
            assessment,
            checkpoint,
            stale,
            reasons,
            source_capture_states: source_states,
            resource,
        })
    }

    /// Related forks form one cluster. Costs are a request union, while labels
    /// remain separate native-session observations with explicit correlation.
    pub async fn checkpoint_family(&self, identity: &SessionIdentity) -> Result<FamilyView> {
        session(&self.db, &identity.key()?).await?;
        let family_id = self.family_id(identity).await?;
        let before = self.list(&identity.owner, &identity.source).await?;
        let mut sessions = Vec::new();
        let mut requests: BTreeMap<String, super::super::RequestAssociation> = BTreeMap::new();
        let mut conflicts = BTreeSet::new();
        let mut current_assessments = 0;
        for row in &before {
            let identity = native(row);
            if self.family_id(&identity).await? != family_id {
                continue;
            }
            let view = self.effective_assessment(&identity).await?;
            if view.assessment.is_some() && !view.stale {
                current_assessments += 1;
            }
            if let Some(resource) = &view.resource {
                for request in &resource.requests {
                    if let Some(existing) = requests.get(&request.request_id) {
                        if existing.charge_micro_usd != request.charge_micro_usd
                            || existing.model_id != request.model_id
                            || existing.provider_id != request.provider_id
                        {
                            conflicts.insert(request.request_id.clone());
                        }
                    } else {
                        requests.insert(request.request_id.clone(), request.clone());
                    }
                }
            }
            sessions.push(view);
        }
        for id in &conflicts {
            if let Some(request) = requests.get_mut(id) {
                request.charge_micro_usd = None;
                request
                    .basis
                    .push_str("; conflicting_resource_observations");
            }
        }
        ensure!(
            self.list(&identity.owner, &identity.source).await? == before,
            "family changed while reading; retry"
        );
        for view in &sessions {
            ensure!(
                head(&self.db, &view.identity.key()?).await? == view.current_revision,
                "family assessment changed while reading; retry"
            );
            for (id, state) in &view.source_capture_states {
                ensure!(
                    connections::Entity::find_by_id(id)
                        .filter(crate::storage::legacy::scope("acp_capture_connections"))
                        .one(&self.db)
                        .await?
                        .is_some_and(|row| row.state == *state),
                    "family capture health changed while reading; retry"
                );
            }
        }
        let requests: Vec<_> = requests.into_values().collect();
        let (known_cost_micro_usd, unpriced_requests) = super::resource::totals(&requests)?;
        Ok(FamilyView {
            family_id,
            sessions,
            current_assessments,
            requests,
            conflicting_request_ids: conflicts.into_iter().collect(),
            known_cost_micro_usd,
            unpriced_requests,
            metering_complete: false,
        })
    }
}
