//! External evaluation freezes facts before a separately authorized Policy update.
use super::*;
use bitrouter_ai::{
    eval::{
        EvalExecutionRef, EvalScope, EvalSubject, EvalSubjectIdentity, EvalVerdict,
        EvaluationResult, EvaluatorIdentity, EvaluatorKind, EvidenceItem, FreezeRequest,
    },
    tenancy::{
        configuration::Activation,
        resources::{Definition, ResourcePut},
    },
};
use bitrouter_sdk::tenancy::TenancyClient;

pub(super) struct Input<'a> {
    pub fixture: &'a Fixture,
    pub endpoint: &'a str,
    pub operator: &'a TenancyClient,
    pub policy: &'a ResourceRef,
    pub other_policy: &'a ResourceRef,
    pub route: &'a ResourceRef,
    pub static_route: &'a ResourceRef,
    pub route_definition: &'a Definition,
    pub revision: &'a ConfigurationRevision,
    pub output: &'a str,
}

pub(super) async fn run(input: Input<'_>) -> Result<()> {
    let Input {
        fixture,
        endpoint,
        operator,
        policy,
        other_policy,
        route,
        static_route,
        route_definition,
        revision,
        output,
    } = input;
    let issued = fixture.issue("eval-loop-key").await?;
    let secret = issued.secret.as_deref().context("eval secret")?;
    let client = TenancyClient::new(endpoint, secret)?;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        fixture.service.admission.wait_drained(),
    )
    .await?;
    let page = client
        .executions(
            &fixture.installation,
            &fixture.tenant,
            &"same-correlation".try_into()?,
            None,
            10,
        )
        .await?;
    let [execution_id] = page.execution_ids.as_slice() else {
        anyhow::bail!("expected one actual inference receipt");
    };
    let receipt = client
        .execution(&fixture.installation, &fixture.tenant, execution_id)
        .await?;
    ensure!(receipt.execution_revision == revision.revision_id);
    let evidence = vec![EvidenceItem {
        evidence_id: "actual-inference".into(),
        kind: "task.output".into(),
        digest: crate::storage::evaluation::canonical_digest(&(output, &receipt))?,
        redacted: true,
        attributes: Default::default(),
    }];
    let subject = EvalSubject {
        spec_version: bitrouter_ai::eval::SpecVersion::Beta1,
        identity: EvalSubjectIdentity::TenantModelPolicy {
            installation_id: fixture.installation.clone(),
            tenant_id: fixture.tenant.clone(),
            configuration_revision: revision.revision_id.clone(),
            policy: policy.clone(),
            name: "auto".into(),
            source_digest: None,
            classifier_digest: None,
            execution: Some(EvalExecutionRef {
                execution_id: execution_id.clone(),
                configuration_revision: revision.revision_id.clone(),
            }),
        },
        eval_id: "client-policy-eval".into(),
        scope: EvalScope::Task,
        subject_id: "actual-inference-task".into(),
        cohort: None,
        holdout: false,
        decisions: Vec::new(),
        requested_dimensions: Default::default(),
        evidence_digest: crate::storage::evaluation::evidence_digest(&evidence)?,
        evidence,
        observed_at: "2026-11-18T00:00:00Z".into(),
    };
    client
        .put_eval_subject(&fixture.installation, &fixture.tenant, &subject)
        .await?;
    let result = EvaluationResult {
        spec_version: bitrouter_ai::eval::SpecVersion::Beta1,
        eval_id: subject.eval_id.clone(),
        evidence_digest: subject.evidence_digest.clone(),
        evaluator: EvaluatorIdentity {
            authority_id: fixture.principal.to_string(),
            evaluator_id: "client-native-check".into(),
            kind: EvaluatorKind::Generic,
            version: "1".into(),
            config_digest: crate::storage::evaluation::canonical_digest(&"output criterion")?,
        },
        verdict: if output.contains("selected policy model") {
            EvalVerdict::Pass
        } else {
            EvalVerdict::Fail
        },
        metrics: Default::default(),
        hard_violations: Vec::new(),
        confidence_ppm: None,
        evidence_refs: vec!["actual-inference".into()],
        decision_credit: Default::default(),
        idempotency_key: "client-policy-result".into(),
        submitted_at: "2026-11-18T00:01:00Z".into(),
    };
    ensure!(
        client
            .submit_eval_result(&fixture.installation, &fixture.tenant, &result)
            .await?
            .status
            == bitrouter_ai::eval::AdmissionStatus::Admitted
    );
    let frozen = client
        .freeze_eval_snapshot(
            &fixture.installation,
            &fixture.tenant,
            &FreezeRequest::default(),
        )
        .await?;
    ensure!(
        frozen
            .entries
            .iter()
            .any(|entry| entry.eval_id == subject.eval_id)
    );
    ensure!(
        client
            .eval_subject(&fixture.installation, &fixture.tenant, &subject.eval_id)
            .await?
            .identity
            == subject.identity
    );
    ensure!(
        operator
            .active_revision(&fixture.installation, &fixture.tenant)
            .await?
            .revision_id
            .as_ref()
            == Some(&revision.revision_id)
    );

    // Wrong names, same-named foreign resources, and forged configuration or
    // execution ownership all fail before a subject can become durable evidence.
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    // The static route shares the exact upstream model and credential with the
    // Policy route. Only immutable route attribution distinguishes these calls.
    let response = http
        .post(format!("{endpoint}/v1/chat/completions"))
        .headers(headers(fixture, secret, &revision.revision_id)?)
        .json(&json!({"model":static_route.resource_id,"messages":[{"role":"user","content":"static attribution probe"}]}))
        .send()
        .await?;
    ensure!(response.status().is_success());
    let static_output = response.text().await?;
    ensure!(static_output.contains("static route output"));
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        fixture.service.admission.wait_drained(),
    )
    .await?;
    let executions = client
        .executions(
            &fixture.installation,
            &fixture.tenant,
            &"same-correlation".try_into()?,
            None,
            10,
        )
        .await?;
    ensure!(executions.execution_ids.len() == 2);
    let static_id = executions
        .execution_ids
        .iter()
        .find(|id| *id != execution_id)
        .context("static inference receipt")?;
    let static_receipt = client
        .execution(&fixture.installation, &fixture.tenant, static_id)
        .await?;
    ensure!(static_receipt.execution_revision == revision.revision_id);
    let target = receipt
        .attempts
        .iter()
        .find_map(|attempt| attempt.target.as_ref())
        .context("Policy target")?;
    let static_target = static_receipt
        .attempts
        .iter()
        .find_map(|attempt| attempt.target.as_ref())
        .context("static target")?;
    ensure!(
        target.provider_id == static_target.provider_id
            && target.model_id == static_target.model_id
    );
    ensure!(
        target.credential_id == static_target.credential_id
            && target.credential_version == static_target.credential_version
    );
    ensure!(target.account_id != static_target.account_id);
    let mut wrong_route = subject.clone();
    wrong_route.eval_id = "static-receipt-as-policy".into();
    if let EvalSubjectIdentity::TenantModelPolicy {
        execution: Some(execution),
        ..
    } = &mut wrong_route.identity
    {
        execution.execution_id = static_id.clone();
    }
    ensure!(
        client
            .put_eval_subject(&fixture.installation, &fixture.tenant, &wrong_route)
            .await
            .is_err()
    );
    // Generic attribution is valid for this actual static execution even though
    // its configuration also exposes a Policy route.
    let mut generic = subject.clone();
    generic.eval_id = "static-receipt-generic".into();
    generic.identity = EvalSubjectIdentity::TenantGeneric {
        installation_id: fixture.installation.clone(),
        tenant_id: fixture.tenant.clone(),
        execution: Some(EvalExecutionRef {
            execution_id: static_id.clone(),
            configuration_revision: revision.revision_id.clone(),
        }),
    };
    generic.evidence[0].digest =
        crate::storage::evaluation::canonical_digest(&(&static_output, &static_receipt))?;
    generic.evidence_digest = crate::storage::evaluation::evidence_digest(&generic.evidence)?;
    client
        .put_eval_subject(&fixture.installation, &fixture.tenant, &generic)
        .await?;
    generic.eval_id = "policy-receipt-as-generic".into();
    if let EvalSubjectIdentity::TenantGeneric {
        execution: Some(execution),
        ..
    } = &mut generic.identity
    {
        execution.execution_id = execution_id.clone();
    }
    ensure!(
        client
            .put_eval_subject(&fixture.installation, &fixture.tenant, &generic)
            .await
            .is_err()
    );
    for field in ["name", "policy", "configuration_revision", "execution"] {
        let mut forged = serde_json::to_value(&subject)?;
        forged["eval_id"] = format!("forged-{field}").into();
        match field {
            "name" => forged["identity"]["name"] = "auto:cost".into(),
            "policy" => forged["identity"]["policy"] = serde_json::to_value(other_policy)?,
            "configuration_revision" => {
                forged["identity"]["configuration_revision"] = "not-retained".into()
            }
            _ => forged["identity"]["execution"]["execution_id"] = "foreign-execution".into(),
        }
        let response = http
            .post(format!(
                "{endpoint}/control/v1/tenants/{}/evals/subjects",
                fixture.tenant
            ))
            .bearer_auth(secret)
            .header("bitrouter-installation-id", fixture.installation.as_str())
            .json(&forged)
            .send()
            .await?;
        ensure!(
            response.status() == reqwest::StatusCode::BAD_REQUEST,
            "forged {field}: {}",
            response.status()
        );
    }
    let second = super::super::eval::authority::second_tenant(fixture).await?;
    let issued = second.issue("eval-other-key").await?;
    let other = TenancyClient::new(endpoint, issued.secret.as_deref().context("other secret")?)?;
    let mut foreign_result = result.clone();
    foreign_result.evaluator.authority_id = second.principal.to_string();
    ensure!(
        other
            .submit_eval_result(&second.installation, &second.tenant, &foreign_result)
            .await
            .is_err()
    );
    ensure!(
        other
            .eval_subject(&second.installation, &second.tenant, &subject.eval_id)
            .await
            .is_err()
    );

    // The client writes a new public source after reading its frozen facts.
    // Neither the result nor freeze grants activation authority.
    let source: bitrouter_ai::policy::Document = serde_json::from_value(
        json!({"spec_version":"policy/1.0.0-beta.1","policies":{"auto":{"tiers":{"economy":{"model":"mock:model-a"}},"default_tier":"economy"}}}),
    )?;
    let put = ResourcePut {
        format: 1,
        operation_id: "eval-next-policy".try_into()?,
        installation_id: fixture.installation.clone(),
        key: policy.key(),
        expected_revision: Some(policy.revision_id.clone()),
        definition: Definition::Policy {
            document: source,
            name: "auto".into(),
            dependencies: Vec::new(),
        },
    };
    let mut next_policy = policy.clone();
    next_policy.revision_id = operator
        .put_resource(&fixture.tenant, &put)
        .await?
        .resource_revision
        .context("new policy revision")?;
    let mut definition = route_definition.clone();
    match &mut definition {
        Definition::Route { policy, .. } => *policy = Some(next_policy.clone()),
        _ => anyhow::bail!("expected explicit route definition"),
    }
    let put = ResourcePut {
        format: 1,
        operation_id: "eval-next-route".try_into()?,
        installation_id: fixture.installation.clone(),
        key: route.key(),
        expected_revision: Some(route.revision_id.clone()),
        definition,
    };
    let mut next_route = route.clone();
    next_route.revision_id = operator
        .put_resource(&fixture.tenant, &put)
        .await?
        .resource_revision
        .context("new route revision")?;
    let mut configuration = fixture.configuration_input("eval-next-configuration")?;
    configuration.document = revision.document.clone();
    configuration.document.routes = vec![next_route];
    configuration.document.policy = Some(next_policy.clone());
    let next = operator
        .put_configuration(&fixture.tenant, &configuration)
        .await?;
    ensure!(
        operator
            .active_revision(&fixture.installation, &fixture.tenant)
            .await?
            .revision_id
            .as_ref()
            == Some(&revision.revision_id)
    );
    let mut forged = subject.clone();
    forged.eval_id = "wrong-policy-revision".into();
    if let EvalSubjectIdentity::TenantModelPolicy { policy, .. } = &mut forged.identity {
        *policy = next_policy;
    }
    ensure!(
        client
            .put_eval_subject(&fixture.installation, &fixture.tenant, &forged)
            .await
            .is_err()
    );
    let activation = Activation {
        operation_id: "eval-explicit-activation".try_into()?,
        installation_id: fixture.installation.clone(),
        expected_active_revision: Some(revision.revision_id.clone()),
        revision_id: next.revision_id.clone(),
        expected_authority_revision: fixture.service.current().map_err(failure)?.revision,
    };
    operator.activate(&fixture.tenant, &activation).await?;
    let mut stale = activation.clone();
    stale.operation_id = "eval-stale-activation".try_into()?;
    stale.expected_authority_revision = fixture.service.current().map_err(failure)?.revision;
    ensure!(operator.activate(&fixture.tenant, &stale).await.is_err());
    ensure!(
        operator
            .active_revision(&fixture.installation, &fixture.tenant)
            .await?
            .revision_id
            .as_ref()
            == Some(&next.revision_id)
    );
    ensure!(
        client
            .eval_snapshot(
                &fixture.installation,
                &fixture.tenant,
                &frozen.evidence_root
            )
            .await?
            == frozen
    );
    ensure!(
        client
            .eval_subject(&fixture.installation, &fixture.tenant, &subject.eval_id)
            .await?
            .identity
            == subject.identity
    );
    let response = http.post(format!("{endpoint}/v1/chat/completions"))
        .headers(headers(fixture, secret, &next.revision_id)?)
        .json(&json!({"model":"auto","messages":[{"role":"user","content":"preserve this complete request"}]})).send().await?;
    ensure!(response.status().is_success());
    ensure!(response.text().await?.contains("updated policy model"));
    Ok(())
}
