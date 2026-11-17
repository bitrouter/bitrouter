use super::*;

#[test]
fn capture_reservations_remain_valid_while_route_experiment_v4_is_rejected() -> Result<()> {
    let current = serde_json::json!({
        "version":2,"job":"job","session":"session","configuration":"configuration",
        "route":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"route",
            "resource_id":"judge","revision_id":"r1"},
        "checkpoint":"checkpoint","prefix_digest":"prefix","input_digest":"input",
        "expected_revision":null,"execution":"execution","status":"accepted",
        "accepted_at":"2026-10-26T00:00:00Z","output":null,"assessment":null,
        "history_reservation":{"checkpoint":"checkpoint","rows":1,
            "bytes":history::REVISION_BYTES,"state":{"state":"held"}}
    });
    let capture: Record = serde_json::from_value(current.clone())?;
    ensure!(valid_record(&capture));
    let mut retired = current.clone();
    retired["version"] = serde_json::json!(1);
    ensure!(!valid_record(&serde_json::from_value(retired.clone())?));
    let mut retired = current;
    retired["version"] = serde_json::json!(4);
    ensure!(!valid_record(&serde_json::from_value(retired.clone())?));
    retired["native_evidence"] = serde_json::json!({});
    ensure!(serde_json::from_value::<Record>(retired).is_err());
    Ok(())
}
