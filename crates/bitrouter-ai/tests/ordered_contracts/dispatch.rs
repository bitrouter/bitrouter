use super::*;

fn documents_with_dispatch(
    receipt: &ExecutionReceipt,
    raw: &str,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let original = format!("\"dispatch\":{}", serde_json::to_string(&receipt.dispatch)?);
    let replacement = format!("\"dispatch\":{raw}");
    let receipt_json = serde_json::to_string(receipt)?;
    let evidence_json = serde_json::to_string(&evidence(receipt))?;
    assert!(receipt_json.contains(&original) && evidence_json.contains(&original));
    Ok((
        receipt_json.replacen(&original, &replacement, 1),
        evidence_json.replacen(&original, &replacement, 1),
    ))
}

fn rejects_dispatch_everywhere(receipt: &ExecutionReceipt, raw: &str) -> TestResult {
    assert!(
        serde_json::from_str::<Dispatch>(raw).is_err(),
        "Dispatch accepted {raw}"
    );
    let (receipt_json, evidence_json) = documents_with_dispatch(receipt, raw)?;
    assert!(
        serde_json::from_str::<ExecutionReceipt>(&receipt_json).is_err(),
        "receipt accepted {raw}"
    );
    assert!(
        serde_json::from_str::<ExecutionUsageEvidence>(&evidence_json).is_err(),
        "evidence accepted {raw}"
    );
    Ok(())
}

#[test]
fn dispatch_requires_an_object_and_a_string_mode() -> TestResult {
    let ordered = receipt(1)?;
    for receipt in [strict_receipt(&ordered)?, ordered] {
        receipt.validate()?;
        evidence(&receipt).validate_against(&receipt)?;
        strict_object(&receipt.dispatch)?;
        let dispatch = serde_json::to_value(&receipt.dispatch)?;
        let mode = dispatch["mode"].as_str().ok_or("mode")?;
        let positional = if mode == "strict" {
            json!([mode])
        } else {
            json!([
                mode,
                dispatch["accepted_selection"],
                dispatch["attempt_limit"]
            ])
        };
        rejects_dispatch_everywhere(&receipt, &serde_json::to_string(&positional)?)?;
        for invalid_mode in [
            json!({(mode): null}),
            json!([mode]),
            Value::Null,
            json!(1),
            json!(true),
            json!("unknown"),
        ] {
            let mut invalid = dispatch.clone();
            invalid["mode"] = invalid_mode;
            rejects_dispatch_everywhere(&receipt, &serde_json::to_string(&invalid)?)?;
        }
        for raw in ["null", "[]", "\"strict\"", "1", "true"] {
            rejects_dispatch_everywhere(&receipt, raw)?;
        }
        let raw = serde_json::to_string(&dispatch)?;
        let duplicate = raw.replacen(
            &format!("\"mode\":\"{mode}\""),
            &format!("\"mode\":\"{mode}\",\"mode\":\"{mode}\""),
            1,
        );
        rejects_dispatch_everywhere(&receipt, &duplicate)?;
        assert_eq!(
            serde_json::from_str::<Dispatch>(&format!(" \n\t{raw}\r "))?,
            receipt.dispatch
        );
    }
    Ok(())
}

#[test]
fn dispatch_keeps_exact_numeric_tokens_in_all_three_contracts() -> TestResult {
    let original = receipt(1)?;
    let raw = serde_json::to_string(&original.dispatch)?;
    for (limit, ordinal) in [("6.4e1", "0e1000"), ("64.0", "0.0")] {
        let exact = raw
            .replace(
                "\"attempt_limit\":64",
                &format!("\"attempt_limit\":{limit}"),
            )
            .replace(
                "\"candidate_indices\":[0]",
                &format!("\"candidate_indices\":[{ordinal}]"),
            );
        assert_eq!(serde_json::from_str::<Dispatch>(&exact)?, original.dispatch);
        let (receipt_json, evidence_json) = documents_with_dispatch(&original, &exact)?;
        let receipt_json = receipt_json.replace(
            "\"observed_http_status\":503",
            "\"observed_http_status\":5.03e2",
        );
        let evidence_json = evidence_json
            .replace("\"extractor_version\":1", "\"extractor_version\":1.0")
            .replace("\"dropped_frames\":0", "\"dropped_frames\":0e1000");
        let decoded: ExecutionReceipt = serde_json::from_str(&receipt_json)?;
        decoded.validate()?;
        assert_eq!(decoded.dispatch, original.dispatch);
        serde_json::from_str::<ExecutionUsageEvidence>(&evidence_json)?
            .validate_against(&decoded)?;
    }
    for invalid in [
        raw.replace(
            "\"attempt_limit\":64",
            "\"attempt_limit\":64.000000000000000001",
        ),
        raw.replace(
            "\"candidate_indices\":[0]",
            "\"candidate_indices\":[0.000000000000000001]",
        ),
        raw.replace(
            "\"attempt_limit\":64",
            "\"attempt_limit\":64,\"attempt_limit\":1",
        ),
        raw.replace(
            "\"candidate_indices\":[0]",
            "\"candidate_indices\":[0],\"candidate_indices\":[1]",
        ),
    ] {
        rejects_dispatch_everywhere(&original, &invalid)?;
    }
    Ok(())
}

#[test]
fn receipt_rejects_positional_root() -> TestResult {
    let ordered = receipt(1)?;
    for receipt in [strict_receipt(&ordered)?, ordered] {
        strict_object(&receipt)?;
        serde_json::from_str::<ExecutionReceipt>(&serde_json::to_string(&receipt)?)?.validate()?;
        let object = serde_json::to_value(&receipt)?;
        let positional = json!([
            object["dispatch"],
            object["installation_id"],
            object["tenant_id"],
            object["principal_id"],
            object["execution_id"],
            object["correlation_id"],
            object["execution_revision"],
            object["state"],
            object["outcome"],
            object["attempts"],
            object["usage"],
            object["recorded_at"]
        ]);
        assert!(serde_json::from_str::<ExecutionReceipt>(&positional.to_string()).is_err());
        assert!(serde_json::from_value::<ExecutionReceipt>(positional).is_err());
    }
    Ok(())
}

#[test]
fn evidence_rejects_positional_root() -> TestResult {
    let ordered = receipt(1)?;
    for receipt in [strict_receipt(&ordered)?, ordered] {
        let document = evidence(&receipt);
        strict_object(&document)?;
        serde_json::from_str::<ExecutionUsageEvidence>(&serde_json::to_string(&document)?)?
            .validate_against(&receipt)?;
        let object = serde_json::to_value(&document)?;
        let positional = json!([
            object["dispatch"],
            object["installation_id"],
            object["tenant_id"],
            object["execution_id"],
            object["capture"],
            object["extractor_version"],
            object["estimation"],
            object["estimate"],
            object["parent_execution_id"],
            object["child_execution_ids"],
            object["attempts"],
            object["frames"],
            object["truncated"],
            object["dropped_frames"]
        ]);
        assert!(serde_json::from_str::<ExecutionUsageEvidence>(&positional.to_string()).is_err());
        assert!(serde_json::from_value::<ExecutionUsageEvidence>(positional).is_err());
    }
    Ok(())
}
