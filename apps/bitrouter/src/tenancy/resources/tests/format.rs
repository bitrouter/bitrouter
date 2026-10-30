use super::*;

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct V1Envelope {
    document: Configuration,
    routes: Vec<compiler::RouteEnvelope>,
    artifacts: Vec<Artifact>,
}

#[test]
fn absent_profiles_keep_exact_v1_bytes_and_unknown_formats_fail() -> Result<()> {
    let old = V1Envelope {
        document: document(Vec::new(), None)?,
        routes: Vec::new(),
        artifacts: Vec::new(),
    };
    let bytes = serde_json::to_vec(&old)?;
    let decoded: Envelope = serde_json::from_slice(&bytes)?;
    decoded.validate_format()?;
    assert_eq!(serde_json::to_vec(&decoded)?, bytes);
    for version in [0, 2, 3, u32::MAX] {
        let mut value = serde_json::to_value(&old)?;
        value["format"] = json!(version);
        let envelope: Envelope = serde_json::from_value(value)?;
        assert!(envelope.validate_format().is_err());
    }
    Ok(())
}

#[test]
fn core_choice_requires_authenticated_v2_and_old_reader_rejects_it() -> Result<()> {
    let mut input = route("tenant", "subject", "base")?;
    input["definition"]["core_choice"] = json!({
        "profile": "choice_v1", "route": reference("tenant", ResourceKind::Route, "judge")?, "policy": {}
    });
    let input: manifest::Import = serde_json::from_value(input)?;
    input.validate()?;
    let mut envelope = Envelope {
        format: 2,
        document: document(vec![input.reference.clone()], None)?,
        routes: Vec::new(),
        artifacts: vec![Artifact {
            reference: input.reference,
            definition: input.definition,
            envelope: None,
        }],
    };
    envelope.validate_format()?;
    assert!(serde_json::from_slice::<V1Envelope>(&serde_json::to_vec(&envelope)?).is_err());
    envelope.format = 1;
    assert!(envelope.validate_format().is_err());
    envelope.format = 3;
    assert!(envelope.validate_format().is_err());
    let mut definition = serde_json::to_value(&envelope.artifacts[0].definition)?;
    definition["core_choice"]["profile"] = json!("choice_v99");
    assert!(serde_json::from_value::<Definition>(definition).is_err());
    Ok(())
}

#[test]
fn assessment_requires_authenticated_v2_and_old_reader_rejects_it() -> Result<()> {
    let mut input = route("tenant", "subject", "base")?;
    input["definition"]["assessment"] = json!({
        "profile": "assessment_v1", "route": reference("tenant", ResourceKind::Route, "judge")?
    });
    let input: manifest::Import = serde_json::from_value(input)?;
    input.validate()?;
    let mut envelope = Envelope {
        format: 2,
        document: document(vec![input.reference.clone()], None)?,
        routes: Vec::new(),
        artifacts: vec![Artifact {
            reference: input.reference,
            definition: input.definition,
            envelope: None,
        }],
    };
    envelope.validate_format()?;
    assert!(serde_json::from_slice::<V1Envelope>(&serde_json::to_vec(&envelope)?).is_err());
    envelope.format = 1;
    assert!(envelope.validate_format().is_err());
    envelope.format = 3;
    assert!(envelope.validate_format().is_err());
    let mut definition = serde_json::to_value(&envelope.artifacts[0].definition)?;
    definition["assessment"]["profile"] = json!("assessment_v99");
    assert!(serde_json::from_value::<Definition>(definition).is_err());
    Ok(())
}

#[cfg(all(feature = "core", feature = "harness"))]
#[tokio::test]
async fn actual_compiler_and_authenticated_reader_preserve_the_profile_version() -> Result<()> {
    let (db, resources) = fixture().await?;
    let mut subject = route("tenant", "subject", "base")?;
    let judge = reference("tenant", ResourceKind::Route, "judge")?;
    subject["definition"]["assessment"] = json!({"profile":"assessment_v1", "route":judge});
    import(
        &resources,
        &db,
        vec![
            credential("tenant")?,
            subject,
            route("tenant", "judge", "judge")?,
        ],
    )
    .await?;
    let doc = document(
        vec![reference("tenant", ResourceKind::Route, "subject")?, judge],
        None,
    )?;
    let compiled = compile(&resources, &db, &doc).await?;
    assert_eq!(compiled.compiler_version, COMPILER_VERSION);
    let envelope = decrypt(&resources, &doc, &compiled)?;
    assert_eq!(envelope.format, 2);
    resources
        .validate_compiled(&validation::stored(doc, compiled)?)
        .map_err(|code| anyhow::anyhow!("authenticated v2 failed: {code:?}"))?;
    db.close().await?;
    Ok(())
}

#[test]
fn independent_profiles_use_v2_if_either_or_both_are_present() -> Result<()> {
    for (choice, assessment) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut value = route("tenant", "parent", "generation")?;
        if choice {
            value["definition"]["core_choice"] = json!({
                "profile": "choice_v1", "route": reference("tenant", ResourceKind::Route, "child")?, "policy": {}
            });
        }
        if assessment {
            value["definition"]["assessment"] = json!({
                "profile": "assessment_v1", "route": reference("tenant", ResourceKind::Route, "judge")?
            });
        }
        let input: manifest::Import = serde_json::from_value(value)?;
        input.validate()?;
        let artifacts = vec![Artifact {
            reference: input.reference.clone(),
            definition: input.definition,
            envelope: None,
        }];
        let format = Envelope::format(&artifacts);
        assert_eq!(format, if choice || assessment { 2 } else { 1 });
        let envelope = Envelope {
            format,
            document: document(vec![input.reference], None)?,
            routes: Vec::new(),
            artifacts,
        };
        envelope.validate_format()?;
        let bytes = serde_json::to_vec(&envelope)?;
        let decoded: Envelope = serde_json::from_slice(&bytes)?;
        decoded.validate_format()?;
        assert_eq!(serde_json::to_vec(&decoded)?, bytes);
        if choice || assessment {
            assert!(serde_json::from_slice::<V1Envelope>(&bytes).is_err());
            assert!(
                serde_json::from_value::<V1Route>(serde_json::to_value(
                    &envelope.artifacts[0].definition
                )?)
                .is_err()
            );
        }
    }
    Ok(())
}

#[cfg(all(feature = "core", feature = "harness"))]
#[tokio::test]
async fn assessment_child_cannot_itself_delegate_a_choice() -> Result<()> {
    let (db, resources) = fixture().await?;
    let mut subject = route("tenant", "subject", "generation")?;
    let judge_ref = reference("tenant", ResourceKind::Route, "judge")?;
    subject["definition"]["assessment"] = json!({"profile":"assessment_v1", "route":judge_ref});
    let mut judge = route("tenant", "judge", "judge")?;
    judge["definition"]["core_choice"] = json!({
        "profile":"choice_v1", "route":reference("tenant", ResourceKind::Route, "choice")?, "policy":{}
    });
    let mut choice = route("tenant", "choice", "choice")?;
    choice["definition"]["routing"]["providers"]["fixture"]["api_protocol"] =
        json!([{"*":"decisions"}]);
    // Each optional profile is valid independently; the composed assessment child
    // must be rejected by the authenticated retained execution decoder.
    import(
        &resources,
        &db,
        vec![credential("tenant")?, subject, judge, choice],
    )
    .await?;
    let doc = document(
        vec![
            reference("tenant", ResourceKind::Route, "subject")?,
            judge_ref,
        ],
        None,
    )?;
    assert!(compile(&resources, &db, &doc).await.is_err());
    db.close().await?;
    Ok(())
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct V1Route {
    kind: String,
    routing: Value,
    selector: String,
    policy: Option<ResourceRef>,
    credentials: Vec<manifest::CredentialBinding>,
    #[serde(default)]
    exploration: Vec<String>,
}

#[test]
fn absent_choice_preserves_nonempty_old_route_bytes_and_old_route_rejects_new_tag() -> Result<()> {
    let value = route("tenant", "parent", "generation")?;
    let old: V1Route = serde_json::from_value(value["definition"].clone())?;
    let bytes = serde_json::to_vec(&old)?;
    let current: Definition = serde_json::from_slice(&bytes)?;
    assert_eq!(serde_json::to_vec(&current)?, bytes);
    let mut with_choice = serde_json::to_value(current)?;
    with_choice["core_choice"] = json!({"profile":"choice_v1","route":reference("tenant",ResourceKind::Route,"child")?,"policy":{}});
    assert!(serde_json::from_value::<V1Route>(with_choice).is_err());
    Ok(())
}
