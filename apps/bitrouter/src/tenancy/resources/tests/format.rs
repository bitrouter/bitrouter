use super::*;

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct OrdinaryEnvelope {
    document: Configuration,
    routes: Vec<compiler::RouteEnvelope>,
    artifacts: Vec<Artifact>,
}

#[test]
fn current_policy_free_ordinary_bytes_keep_their_independent_format() -> Result<()> {
    let current = OrdinaryEnvelope {
        document: document(Vec::new(), None)?,
        routes: Vec::new(),
        artifacts: Vec::new(),
    };
    let bytes = serde_json::to_vec(&current)?;
    let decoded: Envelope = serde_json::from_slice(&bytes)?;
    decoded.validate_format()?;
    assert_eq!(serde_json::to_vec(&decoded)?, bytes);
    for format in [0, 2, 3, 7, 8, u32::MAX] {
        let mut value = serde_json::to_value(&current)?;
        value["format"] = json!(format);
        assert!(
            serde_json::from_value::<Envelope>(value)?
                .validate_format()
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn mature_import_rejects_policy_free_experiments_before_any_write() -> Result<()> {
    let (db, resources) = fixture().await?;
    for (field, value) in [
        ("exploration", json!(["fixture:other"])),
        (
            "core_choice",
            json!({"profile":"choice_v1","route":reference("tenant",ResourceKind::Route,"judge")?,"policy":{}}),
        ),
        (
            "assessment",
            json!({"profile":"assessment_v1","route":reference("tenant",ResourceKind::Route,"judge")?}),
        ),
        ("block", json!({"profile":"unsupported"})),
    ] {
        let mut route = route("tenant", "subject", "model")?;
        route["definition"][field] = value;
        assert!(
            import(&resources, &db, vec![credential("tenant")?, route])
                .await
                .is_err()
        );
        assert!(storage::references(&db).await?.is_empty());
    }
    Ok(())
}

#[test]
fn current_capture_profile_has_a_distinct_envelope_and_no_legacy_reader() -> Result<()> {
    let mut input = route("tenant", "subject", "model")?;
    input["definition"]["assessment"] =
        json!({"profile":"assessment_v1","route":reference("tenant",ResourceKind::Route,"judge")?});
    let input: manifest::Import = serde_json::from_value(input)?;
    input.validate()?;
    assert!(input.definition.require_service().is_err());
    let mut envelope = Envelope {
        format: 8,
        document: document(vec![input.reference.clone()], None)?,
        routes: Vec::new(),
        artifacts: vec![Artifact {
            reference: input.reference,
            definition: input.definition,
            envelope: None,
        }],
    };
    envelope.validate_format()?;
    envelope.format = 2;
    assert!(envelope.validate_format().is_err());
    envelope.format = 3;
    assert!(envelope.validate_format().is_err());
    Ok(())
}
