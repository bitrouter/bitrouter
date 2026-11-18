use super::*;
use anyhow::ensure;

fn rows() -> Result<Vec<Value>> {
    let mut parent = route("tenant", "parent", "generation")?;
    parent["definition"]["routing"]["providers"]["fixture"]["api_protocol"] =
        json!([{"*":"chat_completions"}]);
    parent["definition"]["core_choice"] = json!({"profile":"choice_v1","route":reference("tenant",ResourceKind::Route,"child")?,"policy":{}});
    let mut child = route("tenant", "child", "judge")?;
    child["definition"]["routing"]["providers"]["fixture"]["api_protocol"] =
        json!([{"*":"decisions"}]);
    child["definition"]["routing"]["providers"]["fixture"]["headers"]["x-large"] =
        json!("x".repeat(16 * 1024));
    Ok(vec![parent, credential("tenant")?, child])
}

#[tokio::test]
async fn choice_import_rejects_unsupported_child_graph_atomically() -> Result<()> {
    for mutation in [
        "missing",
        "self",
        "nested",
        "assessment",
        "generation",
        "exploration",
        "credential",
        "cross-tenant",
        "policy-bound",
    ] {
        let (db, resources) = fixture().await?;
        let mut input = rows()?;
        match mutation {
            "missing" => {
                input.pop();
            }
            "self" => {
                input[0]["definition"]["core_choice"]["route"] = input[0]["reference"].clone()
            }
            "nested" => {
                input[2]["definition"]["core_choice"] =
                    input[0]["definition"]["core_choice"].clone()
            }
            "assessment" => {
                input[2]["definition"]["assessment"] = json!({
                    "profile": "assessment_v1", "route": input[0]["reference"].clone()
                });
            }
            "generation" => {
                input[2]["definition"]["routing"]["providers"]["fixture"]["api_protocol"] =
                    json!([{"*":"chat_completions"}])
            }
            "exploration" => input[2]["definition"]["exploration"] = json!(["fixture:other"]),
            "credential" => input[1]["definition"]["oauth_generation"] = json!(1),
            "cross-tenant" => {
                input[0]["definition"]["core_choice"]["route"]["owner_id"] = json!("other")
            }
            "policy-bound" => {
                input[0]["definition"]["core_choice"]["policy"]["max_candidates"] = json!(33)
            }
            _ => anyhow::bail!("unknown graph mutation"),
        }
        ensure!(
            import(&resources, &db, input).await.is_err(),
            "invalid {mutation} accepted"
        );
        ensure!(
            !storage::has_resources(&db).await?,
            "failed import published a partial closure"
        );
        let current = import(&resources, &db, rows()?).await;

        ensure!(current.is_err(), "minimal profile imported native Choice");
        ensure!(!storage::has_resources(&db).await?);
    }
    Ok(())
}
