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
    let fields: Vec<(String, Value)> = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/resource_import/unknown-route-fields.json"
    ))?;
    for (field, value) in fields {
        let mut route = route("tenant", "subject", "model")?;
        route["definition"][&field] = value;
        let error = import(&resources, &db, vec![credential("tenant")?, route])
            .await
            .err()
            .context("unknown route field accepted")?;
        ensure!(
            format!("{error:#}").contains(&format!("unknown field `{field}`")),
            "{error:#}"
        );
        assert!(storage::references(&db).await?.is_empty());
    }
    Ok(())
}
