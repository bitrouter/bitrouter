//! Native evidence is explicit, version-fenced and independent of old wire mappings.
use super::*;

#[test]
fn native_mapping_versions_bind_ingress_and_actual_wire_independently() -> TestResult {
    for inbound in [
        GenerationProtocol::Messages,
        GenerationProtocol::GenerateContent,
    ] {
        for native_frame in [false, true] {
            let mut receipt = receipt(1)?;
            receipt.accepted_selection.inbound_protocol = inbound;
            let mut document = evidence(&receipt);
            let mut observed = frame(&receipt, 0, 0);
            if native_frame {
                observed.protocol = Protocol::GenerateContent;
                observed.source = Source::GenerateContentChunk;
            }
            document.frames.push(observed);
            let required = if inbound == GenerationProtocol::GenerateContent || native_frame {
                2
            } else {
                1
            };
            assert_eq!(
                ExecutionUsageEvidenceV2::extractor_version_for(inbound, &document.frames),
                required
            );
            document.extractor_version = required;
            document.validate_against(&receipt)?;
            let raw = serde_json::to_string(&document)?;
            serde_json::from_str::<ExecutionUsageEvidenceV2>(&raw)?.validate_against(&receipt)?;
            document.extractor_version = if required == 1 { 2 } else { 1 };
            assert!(document.validate().is_err());
        }
    }
    Ok(())
}

#[test]
fn frameless_native_capture_keeps_new_discriminator_for_every_absence_state() -> TestResult {
    let mut receipt = receipt(1)?;
    receipt.accepted_selection.inbound_protocol = GenerationProtocol::GenerateContent;
    for capture in [
        Capture::Captured,
        Capture::NotCaptured,
        Capture::Unavailable,
    ] {
        let mut document = evidence(&receipt);
        document.capture = capture;
        if capture != Capture::Captured {
            document.attempts.clear();
        }
        assert!(document.validate().is_err());
        document.extractor_version = 2;
        document.validate_against(&receipt)?;
    }
    Ok(())
}

#[test]
fn native_source_int32_presence_and_v1_fence_are_exact() -> TestResult {
    let receipt = receipt(1)?;
    let mut document = evidence(&receipt);
    document.extractor_version = 2;
    let mut observed = frame(&receipt, 0, 0);
    observed.protocol = Protocol::GenerateContent;
    document.frames.push(observed);
    for source in [
        Source::Json,
        Source::HttpError,
        Source::GenerateContentChunk,
    ] {
        document.frames[0].source = source;
        document.validate_against(&receipt)?;
        let mut legacy = legacy_evidence(&document)?;
        legacy.extractor_version = 1;
        assert!(legacy.validate().is_err());
    }
    document.frames[0].source = Source::ChatChunk;
    assert!(document.validate().is_err());
    document.frames[0].source = Source::GenerateContentChunk;
    for value in [0, i32::MAX as u64] {
        document.frames[0].quantities[0].value = Some(value);
        document.validate()?;
    }
    document.frames[0].quantities[0].value = Some(i32::MAX as u64 + 1);
    assert!(document.validate().is_err());
    document.frames[0].quantities[0].value = None;
    assert!(document.validate().is_err());
    document.frames[0].invalid = true;
    document.validate()?;
    document.frames[0].quantities.clear();
    document.frames[0].invalid = false;
    document.validate()?;
    assert_eq!(
        serde_json::to_value(&document)?["frames"][0]["quantities"],
        json!([])
    );
    document.frames[0].protocol = Protocol::Messages;
    assert!(document.validate().is_err());
    assert_eq!(
        serde_json::to_string(&Protocol::GenerateContent)?,
        "\"generate_content\""
    );
    assert_eq!(
        serde_json::to_string(&Source::GenerateContentChunk)?,
        "\"generate_content_chunk\""
    );
    Ok(())
}

#[cfg(feature = "schema")]
#[test]
fn native_schemas_accept_exact_new_versions_and_keep_v1_closed() -> TestResult {
    let ordered = serde_json::to_value(schemars::schema_for!(ExecutionUsageEvidenceV2))?;
    let mut cases = Vec::new();
    let receipt = receipt(1)?;
    let mut document = evidence(&receipt);
    document.extractor_version = 2;
    let mut observed = frame(&receipt, 0, 0);
    observed.protocol = Protocol::GenerateContent;
    observed.source = Source::GenerateContentChunk;
    document.frames.push(observed);
    let raw = serde_json::to_value(&document)?;
    cases.push(json!({"valid":true,"schema":"ordered","document":raw}));
    let mut wrong = raw.clone();
    wrong["extractor_version"] = json!(1);
    cases.push(json!({"valid":false,"schema":"ordered","document":wrong}));
    wrong = raw.clone();
    wrong["frames"][0]["quantities"][0]["value"] = json!(2147483648_u64);
    cases.push(json!({"valid":false,"schema":"ordered","document":wrong}));
    wrong = raw;
    wrong["frames"][0]["source"] = json!("message_delta");
    cases.push(json!({"valid":false,"schema":"ordered","document":wrong}));
    let legacy = serde_json::to_value(schemars::schema_for!(ExecutionUsageEvidenceV1))?;
    let mut old = legacy_evidence(&document)?;
    old.extractor_version = 1;
    cases.push(json!({"valid":false,"schema":"legacy","document":old}));
    let native_rule = ordered["allOf"]
        .as_array()
        .ok_or("ordered constraints")?
        .last()
        .ok_or("native version rule")?;
    assert_eq!(
        native_rule["then"]["properties"]["extractor_version"]["const"],
        2
    );
    assert_eq!(
        native_rule["else"]["properties"]["extractor_version"]["const"],
        1
    );
    assert_eq!(legacy["properties"]["extractor_version"]["const"], 1);
    if let Ok(path) = std::env::var("BITROUTER_NATIVE_EVIDENCE_SCHEMA_FIXTURES") {
        std::fs::write(
            path,
            serde_json::to_vec(
                &json!({"schemas":{"ordered":ordered,"legacy":legacy},"cases":cases}),
            )?,
        )?;
    }
    Ok(())
}
