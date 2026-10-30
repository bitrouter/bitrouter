use super::*;

const V1: &str = r#"{"version":1,"job":"job","session":"session","configuration":"configuration","route":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"route","resource_id":"judge","revision_id":"r1"},"checkpoint":"checkpoint","prefix_digest":"prefix","input_digest":"input","expected_revision":null,"execution":"execution","status":"failed","accepted_at":"2026-10-26T00:00:00Z","output":null,"assessment":null}"#;

#[test]
fn terminal_v1_keeps_its_original_bytes_but_unreserved_acceptance_is_not_adopted() -> Result<()> {
    let mut value: Record = serde_json::from_str(V1)?;
    ensure!(valid_record(&value));
    ensure!(
        io::json(&value, RECORD_BYTES)? == V1,
        "the optional new field changed terminal v1 encoding"
    );
    value.status = Status::Accepted;
    ensure!(
        !valid_record(&value),
        "unreserved v1 acceptance became a v2 obligation"
    );
    value.version = 3;
    ensure!(!valid_record(&value), "future recovery format was guessed");
    Ok(())
}

#[test]
fn reservation_decode_rejects_wrong_checkpoint_size_state_and_unknown_fields() -> Result<()> {
    let mut value: Record = serde_json::from_str(V1)?;
    value.version = 2;
    value.status = Status::Accepted;
    value.history_reservation = Some(HistoryReservation {
        checkpoint: value.checkpoint.clone(),
        rows: 1,
        bytes: history::REVISION_BYTES,
        state: ReservationState::Held,
    });
    ensure!(valid_record(&value));
    let encoded = serde_json::to_value(&value)?;
    for (name, field) in [
        ("checkpoint", serde_json::json!("another-checkpoint")),
        ("rows", serde_json::json!(0)),
        ("bytes", serde_json::json!(history::REVISION_BYTES - 1)),
        (
            "state",
            serde_json::json!({"state":"released_without_revision"}),
        ),
    ] {
        let mut changed = encoded.clone();
        changed["history_reservation"][name] = field;
        ensure!(!valid_record(&serde_json::from_value(changed)?));
    }
    let mut extra = encoded;
    extra["history_reservation"]["caller_release"] = serde_json::json!(true);
    ensure!(serde_json::from_value::<Record>(extra).is_err());
    Ok(())
}
