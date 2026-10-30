use super::*;
use anyhow::{Result, ensure};

#[test]
fn streamed_history_commitment_matches_existing_native_semantics_and_bounds_work() -> Result<()> {
    let mut messages = vec![
        Message::text(Role::User, "prefix"),
        Message::text(Role::Assistant, "answer"),
    ];
    put(
        &mut messages[1].content[0],
        ORIGIN_FIELD,
        "ignored-origin".into(),
    );
    put(
        &mut messages[1].content[0],
        CONTINUATION_FIELD,
        "ignored-continuation".into(),
    );
    let expected = crate::gateway::pipeline::native_continuation::history_commitment(&messages)
        .map_err(|_| anyhow::anyhow!("existing history commitment failed"))?;
    ensure!(
        history(messages.iter()).map_err(|_| anyhow::anyhow!("streamed commitment failed"))?
            == expected
    );
    let large = Message::text(Role::User, "x".repeat(1024 * 1024));
    ensure!(history(std::iter::repeat_n(&large, 17)).is_err());
    ensure!(
        history(messages.iter()).is_ok(),
        "failed budget must not poison later work"
    );
    Ok(())
}
