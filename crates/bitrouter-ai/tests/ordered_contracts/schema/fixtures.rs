use super::*;

#[derive(Serialize)]
pub(super) struct Case {
    name: String,
    schema: String,
    document: Value,
    schema_valid: bool,
    semantic_valid: bool,
}

fn push<T: DeserializeOwned + Validate + JsonSchema>(
    cases: &mut Vec<Case>,
    name: &str,
    document: Value,
    schema_valid: bool,
    semantic_valid: bool,
) {
    assert_eq!(valid::<T>(document.clone()), semantic_valid, "{name}");
    cases.push(Case {
        name: name.into(),
        schema: T::schema_name().into_owned(),
        document,
        schema_valid,
        semantic_valid,
    });
}

fn strict<T: DeserializeOwned + Serialize + Validate + JsonSchema>(
    cases: &mut Vec<Case>,
    value: &T,
) -> TestResult {
    let value = serde_json::to_value(value)?;
    let name = T::schema_name();
    push::<T>(cases, &format!("{name}:valid"), value.clone(), true, true);
    for (key, entry) in value.as_object().ok_or("object")? {
        let mut missing = value.clone();
        missing.as_object_mut().ok_or("object")?.remove(key);
        push::<T>(
            cases,
            &format!("{name}:missing:{key}"),
            missing,
            false,
            false,
        );
        if !entry.is_null() {
            let mut null = value.clone();
            null[key] = Value::Null;
            push::<T>(cases, &format!("{name}:null:{key}"), null, false, false);
        }
    }
    let mut extra = value;
    extra["unexpected"] = json!(true);
    push::<T>(cases, &format!("{name}:unknown"), extra, false, false);
    Ok(())
}

fn mutated<T: DeserializeOwned + Validate + JsonSchema>(
    cases: &mut Vec<Case>,
    base: &Value,
    pointer: &str,
    replacement: Value,
    schema_valid: bool,
    semantic_valid: bool,
) -> TestResult {
    let mut value = base.clone();
    *value.pointer_mut(pointer).ok_or("pointer")? = replacement.clone();
    push::<T>(
        cases,
        &format!("{}:{pointer}:{replacement}", T::schema_name()),
        value,
        schema_valid,
        semantic_valid,
    );
    Ok(())
}

pub(super) fn cases() -> Result<Vec<Case>, Box<dyn std::error::Error>> {
    let mut cases = Vec::new();
    let selection = selected(vec![0])?;
    strict(&mut cases, &selection)?;
    let accepted = selection.accept(GenerationProtocol::Messages)?;
    strict(&mut cases, &accepted)?;
    let manifest = selection::manifest(1)?;
    strict(&mut cases, &manifest)?;
    strict(&mut cases, &manifest.candidates[0])?;
    let caps = selection::capabilities()?;
    strict(&mut cases, &caps)?;
    strict(
        &mut cases,
        &caps.bind(
            "revision".try_into()?,
            "correlation".try_into()?,
            selection.clone(),
        )?,
    )?;
    let mut receipt = receipt(1)?;
    receipt.attempts[0].observed_http_status = None;
    strict(&mut cases, &receipt.attempts[0])?;
    strict(&mut cases, &receipt)?;
    strict(&mut cases, &evidence(&receipt))?;
    selection_cases(&mut cases)?;
    receipt_cases(&mut cases)?;
    evidence_cases(&mut cases)?;
    Ok(cases)
}

fn selection_cases(cases: &mut Vec<Case>) -> TestResult {
    for count in [0, 1, 8, 9, 64, 65] {
        let value = serde_json::to_value(selected((0..count).collect())?)?;
        let expected = (1..=64).contains(&count);
        push::<OrderedCandidateSelectionV1>(
            cases,
            &format!("selection count {count}"),
            value,
            expected,
            expected,
        );
    }
    let selection = serde_json::to_value(selected(vec![0])?)?;
    for replacement in [
        json!([256]),
        json!([-1]),
        json!([0.1]),
        json!([0, 0]),
        json!([null]),
        json!(["0"]),
    ] {
        mutated::<OrderedCandidateSelectionV1>(
            cases,
            &selection,
            "/candidate_indices",
            replacement,
            false,
            false,
        )?;
    }
    for replacement in [json!(0), json!(2), json!(1.5), json!("1")] {
        mutated::<OrderedCandidateSelectionV1>(
            cases,
            &selection,
            "/schema_version",
            replacement,
            false,
            false,
        )?;
    }
    mutated::<OrderedCandidateSelectionV1>(
        cases,
        &selection,
        "/route/resource_kind",
        json!("credential"),
        false,
        false,
    )?;
    let accepted = serde_json::to_value(selected(vec![0])?.accept(GenerationProtocol::Messages)?)?;
    for protocol in [
        "chat_completions",
        "messages",
        "responses",
        "decisions",
        "custom",
    ] {
        let expected = !matches!(protocol, "decisions" | "custom");
        mutated::<AcceptedSelectionV2>(
            cases,
            &accepted,
            "/inbound_protocol",
            json!(protocol),
            expected,
            expected,
        )?;
    }
    for count in [0, 1, 256, 257] {
        push::<CandidateManifestV1>(
            cases,
            &format!("manifest count {count}"),
            serde_json::to_value(selection::manifest(count)?)?,
            count <= 256,
            count <= 256,
        );
    }
    let manifest = serde_json::to_value(selection::manifest(1)?)?;
    for (pointer, replacement) in [
        ("/schema_version", json!(2)),
        ("/candidates/0/candidate_index", json!(256)),
        ("/candidates/0/credential/resource_kind", json!("route")),
        ("/route/resource_kind", json!("credential")),
        ("/authority_revision", json!(9007199254740992_u64)),
    ] {
        mutated::<CandidateManifestV1>(cases, &manifest, pointer, replacement, false, false)?;
    }
    for pointer in ["/route/owner_id", "/candidates/0/credential/owner_id"] {
        mutated::<CandidateManifestV1>(cases, &manifest, pointer, json!("foreign"), true, false)?;
    }
    let caps = serde_json::to_value(selection::capabilities()?)?;
    for limit in [0, 1, 8, 9, 64, 65] {
        let expected = (1..=64).contains(&limit);
        mutated::<OrderedCapabilitiesV2>(
            cases,
            &caps,
            "/attempt_limit",
            json!(limit),
            expected,
            expected,
        )?;
    }
    mutated::<OrderedCapabilitiesV2>(cases, &caps, "/contract_version", json!(1), false, false)?;
    for guarantee in [
        "revision_bound_dispatch",
        "strict_receipts",
        "dispatch_enabled",
    ] {
        mutated::<OrderedCapabilitiesV2>(
            cases,
            &caps,
            &format!("/{guarantee}"),
            json!(false),
            true,
            true,
        )?;
    }
    let binding = serde_json::to_value(selection::capabilities()?.bind(
        "revision".try_into()?,
        "correlation".try_into()?,
        selected(vec![0])?,
    )?)?;
    mutated::<OrderedDispatchBindingV2>(
        cases,
        &binding,
        "/Bitrouter-Tenancy-Version",
        json!("1"),
        false,
        false,
    )?;
    mutated::<OrderedDispatchBindingV2>(
        cases,
        &binding,
        "/Bitrouter-Receipt-Mode",
        json!("best_effort"),
        false,
        false,
    )?;
    Ok(())
}

fn receipt_cases(cases: &mut Vec<Case>) -> TestResult {
    for count in [1, 8, 9, 64] {
        push::<ExecutionReceiptV2>(
            cases,
            &format!("receipt {count}"),
            serde_json::to_value(receipt(count)?)?,
            true,
            true,
        );
    }
    let mut receipt = receipt(1)?;
    let base = serde_json::to_value(&receipt)?;
    for limit in [0, 65] {
        mutated::<ExecutionReceiptV2>(cases, &base, "/attempt_limit", json!(limit), false, false)?;
    }
    for version in [0, 1, 3] {
        mutated::<ExecutionReceiptV2>(
            cases,
            &base,
            "/schema_version",
            json!(version),
            false,
            false,
        )?;
    }
    mutated::<ExecutionReceiptV2>(
        cases,
        &base,
        "/accepted_selection/route/owner_id",
        json!("foreign"),
        true,
        false,
    )?;
    mutated::<ExecutionReceiptV2>(
        cases,
        &base,
        "/attempts",
        json!(vec![base["attempts"][0].clone(); 65]),
        false,
        false,
    )?;
    receipt.attempts[0].target = Some(serde_json::from_value(target())?);
    receipt.attempts[0].outcome = Outcome::Completed;
    receipt.outcome = Outcome::Completed;
    receipt.state = ReceiptState::Incomplete;
    for status in [
        None,
        Some(199),
        Some(200),
        Some(299),
        Some(300),
        Some(503),
        Some(599),
        Some(600),
    ] {
        receipt.attempts[0].observed_http_status = status;
        let expected = status.is_none_or(|status| (200..=299).contains(&status));
        push::<ExecutionReceiptV2>(
            cases,
            &format!("completed status {status:?}"),
            serde_json::to_value(&receipt)?,
            expected,
            expected,
        );
    }
    receipt.attempts[0].outcome = Outcome::Unknown;
    receipt.outcome = Outcome::Unknown;
    receipt.attempts[0].observed_http_status = Some(200);
    for state in [
        ReceiptState::Committed,
        ReceiptState::Incomplete,
        ReceiptState::Unknown,
    ] {
        receipt.state = state;
        push::<ExecutionReceiptV2>(
            cases,
            &format!("unknown state {state:?}"),
            serde_json::to_value(&receipt)?,
            state == ReceiptState::Unknown,
            state == ReceiptState::Unknown,
        );
    }
    Ok(())
}

fn evidence_cases(cases: &mut Vec<Case>) -> TestResult {
    for (count, frames, schema_valid, semantic_valid) in [
        (1, 128, true, true),
        (1, 129, true, false),
        (8, 512, true, true),
        (8, 513, false, false),
        (9, 1024, true, true),
        (9, 1025, false, false),
        (64, 4096, true, true),
        (64, 4097, false, false),
    ] {
        let receipt = receipt(count)?;
        let mut value = evidence(&receipt);
        value.frames = (0..frames)
            .map(|index| frame(&receipt, index, index % usize::from(count)))
            .collect();
        push::<ExecutionUsageEvidenceV2>(
            cases,
            &format!("evidence {count}/{frames}"),
            serde_json::to_value(value)?,
            schema_valid,
            semantic_valid,
        );
    }
    let receipt = receipt(1)?;
    let mut value = evidence(&receipt);
    value.frames.push(frame(&receipt, 0, 0));
    let base = serde_json::to_value(&value)?;
    for (pointer, replacement) in [
        ("/schema_version", json!(1)),
        ("/extractor_version", json!(2)),
        ("/frames/0/protocol", json!("decisions")),
        ("/frames/0/source", json!("chat_chunk")),
        ("/frames/0/semantics", json!("delta")),
        ("/frames/0/quantities/0/value", Value::Null),
        ("/frames/0/quantities/0/value", json!(-1)),
        ("/frames/0/quantities/0/value", json!(0.5)),
        ("/frames/0/quantities/0/value", json!(9007199254740992_u64)),
        ("/dropped_frames", json!(9007199254740992_u64)),
        (
            "/frames/0/quantities",
            json!(vec![base["frames"][0]["quantities"][0].clone(); 33]),
        ),
    ] {
        mutated::<ExecutionUsageEvidenceV2>(cases, &base, pointer, replacement, false, false)?;
    }
    for pointer in [
        "/estimate",
        "/frames/0/quantities/0/value",
        "/attempts/0/target",
    ] {
        let (parent, key) = pointer.rsplit_once('/').ok_or("pointer")?;
        let mut missing = base.clone();
        missing
            .pointer_mut(parent)
            .ok_or("parent")?
            .as_object_mut()
            .ok_or("object")?
            .remove(key);
        push::<ExecutionUsageEvidenceV2>(
            cases,
            &format!("missing {pointer}"),
            missing,
            false,
            false,
        );
    }
    let mut invalid = base.clone();
    invalid["frames"][0]["invalid"] = json!(true);
    invalid["frames"][0]["quantities"][0]["value"] = Value::Null;
    push::<ExecutionUsageEvidenceV2>(cases, "invalid explicit null", invalid, true, true);
    mutated::<ExecutionUsageEvidenceV2>(
        cases,
        &base,
        "/frames/0/quantities",
        json!([]),
        true,
        true,
    )?;
    mutated::<ExecutionUsageEvidenceV2>(
        cases,
        &base,
        "/accepted_selection/route/owner_id",
        json!("foreign"),
        true,
        false,
    )?;
    for field in ["parent_execution_id", "child_execution_ids", "auxiliary"] {
        let mut extra = base.clone();
        extra[field] = Value::Null;
        push::<ExecutionUsageEvidenceV2>(cases, &format!("forbidden {field}"), extra, false, false);
    }
    value.truncated = true;
    value.dropped_frames = 1;
    push::<ExecutionUsageEvidenceV2>(
        cases,
        "truncated prefix",
        serde_json::to_value(value)?,
        true,
        true,
    );
    Ok(())
}
