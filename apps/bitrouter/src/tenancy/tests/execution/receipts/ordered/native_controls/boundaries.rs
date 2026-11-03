use super::*;
use bitrouter_ai::tenancy::{admission::AdmissionProfileV1, operation::DispatchState};

mod authority;

async fn raw(h: &OrderedHarness, headers: HeaderMap, body: Value) -> Result<reqwest::Response> {
    Ok(reqwest::Client::new()
        .post(format!("{}/v1/responses", h.endpoint))
        .headers(headers)
        .json(&body)
        .send()
        .await?)
}

#[tokio::test]
async fn native_mode_requires_operator_policy_exact_header_current_binding_and_supported_input()
-> Result<()> {
    for enabled in [false, true] {
        let h = harness(&["responses"], enabled, None).await?;
        let caps = h
            .control(false, HttpVersion::Http1Only)?
            .ordered_native_controls_capabilities()
            .await?;
        ensure!(caps.enabled == enabled);
        let binding = h.binding(vec![0], GenerationProtocol::Responses).await?;
        let normal = caller_timeout::ordered_headers(&h, &binding)?;
        for case in [
            "absent",
            "unknown",
            "duplicate",
            "v1",
            "stale",
            "cached",
            "foreign",
        ] {
            let mut headers = normal.clone();
            let mut body = json!({"model":MODEL,"input":"next","previous_response_id":"opaque"});
            if case != "absent" {
                headers.insert(HEADER, PROVIDER_NATIVE_V1.parse()?);
            }
            match case {
                "unknown" => {
                    headers.insert(HEADER, "provider_native_v2".parse()?);
                }
                "duplicate" => {
                    headers.append(HEADER, PROVIDER_NATIVE_V1.parse()?);
                }
                "v1" => {
                    headers.insert("bitrouter-tenancy-version", "1".parse()?);
                    headers.remove("bitrouter-candidate-selection");
                }
                "stale" => {
                    headers.insert("bitrouter-instance-id", "old-instance".parse()?);
                }
                "cached" => {
                    body["cachedContent"] = json!("cachedContents/unsupported");
                }
                "foreign" => {
                    body["context_management"] = json!({"edits":[]});
                }
                _ => {}
            }
            let response = raw(&h, headers, body).await?;
            ensure!(!response.status().is_success(), "{case}");
            ensure!(!response.headers().contains_key("x-bitrouter-request-id"));
            let failure: bitrouter_ai::tenancy::operation::Error = response.json().await?;
            ensure!(
                failure.dispatch_state == DispatchState::NotStarted
                    && failure.execution_id.is_none()
            );
        }
        if !enabled {
            let mut headers = normal;
            headers.insert(HEADER, PROVIDER_NATIVE_V1.parse()?);
            let response = raw(
                &h,
                headers,
                json!({"model":MODEL,"input":"next","previous_response_id":"opaque"}),
            )
            .await?;
            ensure!(!response.status().is_success());
        }
        ensure!(h.observed()?.is_empty());
        h.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn native_controls_do_not_create_compatible_targets_or_retry_unknown_provider_outcomes()
-> Result<()> {
    for malformed in [None, Some(1)] {
        let protocols: &[&str] = if malformed.is_some() {
            &["responses", "responses"]
        } else {
            &["messages", "messages"]
        };
        let h = harness(protocols, true, malformed).await?;
        let binding = h.binding(vec![1, 0], GenerationProtocol::Responses).await?;
        let result = invoke(
            &h,
            &binding,
            GenerationProtocol::Responses,
            json!({"input":"next","previous_response_id":"opaque"}),
            None,
        )
        .await;
        ensure!(result.is_err());
        let expected: &[&str] = if malformed.is_some() {
            &["model-1"]
        } else {
            &[]
        };
        ensure!(h.observed()? == expected);
        let page = h
            .control(true, HttpVersion::Http1Only)?
            .executions(
                &h.fixture.installation,
                &h.fixture.tenant,
                &binding.correlation_id,
                None,
                100,
            )
            .await?;
        ensure!(page.execution_ids.len() == 1);
        let receipt = h.receipt(&page.execution_ids[0]).await?;
        ensure!(receipt.attempts.len() == expected.len() && receipt.outcome != Outcome::Completed);
        h.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn native_profiled_duplicate_projections_keep_original_ordinals_and_skip_without_charge()
-> Result<()> {
    let profiles = (0..4)
        .map(|index| {
            serde_json::from_value::<AdmissionProfileV1>(json!({
        "schema_version":1,"rate":{"rate_group_id":format!("native-group-{index}"),
        "member_rpm":if index == 2 {0} else {100}},"health":null,
        "fallback":"ordered_terminal_rejections_v1"}))
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let h = OrderedHarness::with_native_mode(
        4,
        8,
        None,
        Value::Null,
        Some(&profiles),
        true,
        |resources| {
            set_protocols(
                resources,
                &["responses", "messages", "responses", "responses"],
            )?;
            let route = resources
                .iter_mut()
                .find(|r| r["reference"]["resource_kind"] == "route")
                .context("route")?;
            let definition = &mut route["definition"];
            definition["routing"]["models"]["ordered-root"]["endpoints"][3] =
                definition["routing"]["models"]["ordered-root"]["endpoints"][2].clone();
            let credential = definition["credentials"][2]["credential"].clone();
            for row in definition["attachments"]
                .as_array_mut()
                .context("attachments")?
            {
                if row["candidate_index"] == 3 {
                    row["credential"] = credential.clone();
                }
            }
            definition["credentials"]
                .as_array_mut()
                .context("credentials")?
                .truncate(3);
            definition["routing"]["providers"]
                .as_object_mut()
                .context("providers")?
                .remove("provider-3");
            Ok(())
        },
    )
    .await?;
    let control = h.control(true, HttpVersion::Http1Only)?;
    let manifest = control
        .candidate_manifest(
            &h.fixture.installation,
            &h.fixture.tenant,
            &h.revision.revision_id,
            &h.route,
            GenerationProtocol::Responses,
        )
        .await?;
    ensure!(manifest.candidates[2].credential == manifest.candidates[3].credential);
    ensure!(
        serde_json::to_value(&manifest.candidates[2].target)?
            == serde_json::to_value(&manifest.candidates[3].target)?
    );
    let binding = control.ordered_capabilities().await?.bind(
        h.revision.revision_id.clone(),
        "native-ordinals".try_into()?,
        OrderedCandidateSelectionV1 {
            schema_version: 1,
            admission_mode: manifest.admission_mode,
            route: h.route.clone(),
            candidate_indices: vec![1, 3, 2, 0],
            generation_defaults_digest: None,
        },
    )?;
    let (id, _) = invoke(
        &h,
        &binding,
        GenerationProtocol::Responses,
        json!({"input":"next","previous_response_id":"opaque"}),
        None,
    )
    .await?;
    let receipt = h.receipt(&id).await?;
    ensure!(receipt.accepted_selection.candidate_indices == [1, 3, 2, 0]);
    ensure!(
        receipt
            .attempts
            .iter()
            .map(|a| a.candidate_index)
            .collect::<Vec<_>>()
            == [3, 0]
    );
    ensure!(h.observed()? == ["model-2", "model-0"]);
    for attempt in &receipt.attempts {
        ensure!(
            serde_json::to_value(attempt.target.as_ref().context("actual target")?)?
                == serde_json::to_value(
                    &manifest.candidates[usize::from(attempt.candidate_index)].target
                )?
        );
    }
    let observed = control
        .candidate_manifest(
            &h.fixture.installation,
            &h.fixture.tenant,
            &h.revision.revision_id,
            &h.route,
            GenerationProtocol::Responses,
        )
        .await?
        .runtime_observation
        .context("observation")?;
    let counts = observed
        .candidates
        .iter()
        .map(|c| c.rate.as_ref().context("rate").map(|r| r.shared_count))
        .collect::<Result<Vec<_>>>()?;
    ensure!(counts == [1, 0, 0, 1]);
    bodies(&h)?;
    h.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_control_envelope_digest_mutation_refuses_before_send() -> Result<()> {
    let h = harness(&["responses"], true, None).await?;
    let envelope = defaults(17, json!({"previous_response_id":"bound-id"}))?;
    let binding = h
        .binding(vec![0], GenerationProtocol::Responses)
        .await?
        .with_generation_defaults_envelope_v2(&envelope)?;
    let mut headers = caller_timeout::ordered_headers(&h, &binding)?;
    headers.insert(HEADER, PROVIDER_NATIVE_V1.parse()?);
    let mut body = json!({"model":MODEL,"input":"next",
        "bitrouter_generation_defaults_envelope":envelope});
    body["bitrouter_generation_defaults_envelope"]["supplemental"]["previous_response_id"] =
        json!("changed-id");
    let response = raw(&h, headers, body).await?;
    ensure!(
        !response.status().is_success()
            && !response.headers().contains_key("x-bitrouter-request-id")
    );
    ensure!(h.observed()?.is_empty());
    h.fixture.service.shutdown().await?;
    Ok(())
}
