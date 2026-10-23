//! Pure parameterization and row-decoding contracts; no database is opened.

use anyhow::Context;

use super::*;

fn digest(epoch: &str, value: usize) -> Result<KeyedDigest> {
    KeyedDigest::parse(format!("hmac-sha256:{epoch}:{value:064x}"))
}

fn request(id: &str) -> Result<request_entity::Model> {
    Ok(request_entity::Model {
        request_id: id.into(),
        owner_user_id: "owner".into(),
        episode_id: "episode".into(),
        start_event_id: format!("start-{id}"),
        settlement_event_id: None,
        settlement_outbox_id: None,
        full_input_digest: digest("epoch-1", 1)?.as_str().into(),
        native_parent_id: None,
        protocol: "responses".into(),
        status: "started".into(),
    })
}

fn prefix(digest: &KeyedDigest, ambiguous: bool) -> PrefixRow {
    PrefixRow {
        namespace: "namespace".into(),
        owner_user_id: "owner".into(),
        full_input_digest: digest.as_str().into(),
        episode_id: "episode".into(),
        ambiguous,
    }
}

#[test]
fn limits_apply_to_input_count_before_deduplication() -> Result<()> {
    let ids = vec!["same".to_owned(); MAX_REQUEST_IDS];
    assert_eq!(request_keys("owner", &ids)?.len(), 1);
    let oversized_ids = vec!["same".to_owned(); MAX_REQUEST_IDS + 1];
    assert!(request_keys("owner", &oversized_ids).is_err());
    let digests = vec![digest("epoch-1", 1)?; MAX_PREFIX_DIGESTS];
    assert_eq!(prefix_keys("owner", &digests)?.len(), 1);
    let oversized_digests = vec![digest("epoch-1", 1)?; MAX_PREFIX_DIGESTS + 1];
    assert!(prefix_keys("owner", &oversized_digests).is_err());
    assert!(request_keys("", &ids).is_err());
    assert!(prefix_keys("", &digests).is_err());
    assert!(request_keys("owner", &["".into()]).is_err());
    assert!(request_keys("owner", &["x".repeat(513)]).is_err());
    assert!(request_keys("owner", &[])?.is_empty());
    assert!(prefix_keys("owner", &[])?.is_empty());
    Ok(())
}

#[test]
fn statements_bind_all_values_and_bound_rows_at_both_maxima() -> Result<()> {
    let namespace = "namespace-test";
    let owner = "owner' OR TRUE --";
    let injected = "request') OR TRUE --";
    let ids = vec![injected.into(), "ordinary".into(), injected.into()];
    let keys = request_keys(owner, &ids)?;
    let statement = batch_statement(namespace, owner, &keys, Lookup::Requests)?;
    assert!(
        statement
            .sql
            .contains("WHERE namespace=$1 AND owner_user_id=$2 AND request_id IN ($3,$4)")
    );
    assert!(statement.sql.ends_with("ORDER BY request_id LIMIT $5"));
    assert!(!statement.sql.contains(injected));
    assert!(!statement.sql.contains(owner));
    assert!(!statement.sql.contains(namespace));
    let values = statement.values.context("bound values missing")?.0;
    assert_eq!(
        values,
        vec![
            namespace.into(),
            owner.into(),
            "ordinary".into(),
            injected.into(),
            3_i64.into()
        ]
    );

    let ids = (0..MAX_REQUEST_IDS)
        .map(|index| format!("request-{index}"))
        .collect::<Vec<_>>();
    let keys = request_keys("owner", &ids)?;
    let statement = batch_statement("namespace", "owner", &keys, Lookup::Requests)?;
    assert!(statement.sql.ends_with("LIMIT $35"));
    let values = statement.values.context("request values missing")?.0;
    assert_eq!(values.len(), 35);
    assert_eq!(values.last(), Some(&Value::from(33_i64)));

    let digests = (0..MAX_PREFIX_DIGESTS)
        .map(|index| digest("epoch-1", index))
        .collect::<Result<Vec<_>>>()?;
    let keys = prefix_keys("owner", &digests)?;
    let statement = batch_statement("namespace", "owner", &keys, Lookup::Prefixes)?;
    assert!(
        statement
            .sql
            .contains("WHERE namespace=$1 AND owner_user_id=$2 AND full_input_digest IN ($3,$4,")
    );
    assert!(
        statement
            .sql
            .ends_with("ORDER BY full_input_digest LIMIT $4099")
    );
    assert_eq!(statement.sql.matches('$').count(), 4099);
    assert!(!statement.sql.contains("JOIN"));
    assert!(!statement.sql.contains("gateway_shared_trajectory_episodes"));
    assert!(!statement.sql.contains("gateway_shared_trajectory_events"));
    let values = statement.values.context("prefix values missing")?.0;
    assert_eq!(values.len(), 4099);
    assert_eq!(values.last(), Some(&Value::from(4097_i64)));
    assert!(batch_statement("namespace", "owner", &BTreeSet::new(), Lookup::Prefixes).is_err());
    validate_row_count(4096, 4096)?;
    assert!(validate_row_count(4097, 4096).is_err());
    assert!(validate_row_count(33, 32).is_err());
    Ok(())
}

#[test]
fn requests_return_only_exact_requested_scoped_typed_rows() -> Result<()> {
    let ids = vec!["current".into(), "native".into(), "missing".into()];
    let keys = request_keys("owner", &ids)?;
    let mut found = BTreeMap::new();
    insert_request(
        "namespace",
        "owner",
        &keys,
        &mut found,
        "namespace",
        request("current")?,
    )?;
    let mut native = request("native")?;
    native.full_input_digest = digest("epoch-2", 2)?.as_str().into();
    insert_request("namespace", "owner", &keys, &mut found, "namespace", native)?;
    assert_eq!(found.len(), 2);
    assert!(!found.contains_key("missing"));
    assert_eq!(
        found
            .get("current")
            .context("current missing")?
            .full_input_digest
            .key_id(),
        "epoch-1"
    );
    assert_eq!(
        found
            .get("native")
            .context("native missing")?
            .full_input_digest
            .key_id(),
        "epoch-2"
    );
    let before = found.clone();
    assert!(
        insert_request(
            "namespace",
            "owner",
            &keys,
            &mut found,
            "namespace",
            request("current")?
        )
        .is_err()
    );
    assert_eq!(found, before);
    for (namespace, mut row) in [
        ("foreign", request("missing")?),
        ("namespace", request("unrequested")?),
    ] {
        row.full_input_digest = digest("epoch-1", 3)?.as_str().into();
        assert!(insert_request("namespace", "owner", &keys, &mut found, namespace, row).is_err());
    }
    let mut row = request("missing")?;
    row.owner_user_id = "foreign".into();
    assert!(insert_request("namespace", "owner", &keys, &mut found, "namespace", row).is_err());
    let mut row = request("missing")?;
    row.full_input_digest = "not-a-keyed-digest".into();
    assert!(insert_request("namespace", "owner", &keys, &mut found, "namespace", row).is_err());
    let mut row = request("missing")?;
    row.status = "unknown".into();
    assert!(insert_request("namespace", "owner", &keys, &mut found, "namespace", row).is_err());
    assert_eq!(found, before);
    Ok(())
}

#[test]
fn prefix_maps_keep_epoch_keys_missing_and_ambiguity_distinct() -> Result<()> {
    let digests = vec![
        digest("epoch-1", 1)?,
        digest("epoch-2", 1)?,
        digest("epoch-2", 2)?,
        digest("epoch-2", 3)?,
    ];
    let keys = prefix_keys("owner", &digests)?;
    let mut found = BTreeMap::new();
    insert_prefix(
        "namespace",
        "owner",
        &keys,
        &mut found,
        prefix(&digests[0], false),
    )?;
    insert_prefix(
        "namespace",
        "owner",
        &keys,
        &mut found,
        prefix(&digests[1], false),
    )?;
    insert_prefix(
        "namespace",
        "owner",
        &keys,
        &mut found,
        prefix(&digests[2], true),
    )?;
    assert_eq!(found.len(), 3);
    assert_eq!(
        found.get(digests[0].as_str()),
        Some(&PrefixMatch::Unique("episode".into()))
    );
    assert_eq!(
        found.get(digests[1].as_str()),
        Some(&PrefixMatch::Unique("episode".into()))
    );
    assert_eq!(
        found.get(digests[2].as_str()),
        Some(&PrefixMatch::Ambiguous)
    );
    assert!(!found.contains_key(digests[3].as_str()));
    assert!(
        insert_prefix(
            "namespace",
            "owner",
            &keys,
            &mut found,
            prefix(&digests[0], false)
        )
        .is_err()
    );
    assert_eq!(found.len(), 3);
    Ok(())
}

#[test]
fn prefix_rows_reject_foreign_unrequested_and_invalid_index_values() -> Result<()> {
    let keyed = digest("epoch-1", 1)?;
    let digests = vec![keyed.clone()];
    let keys = prefix_keys("owner", &digests)?;
    let mut found = BTreeMap::new();
    let mut row = prefix(&keyed, false);
    row.namespace = "foreign".into();
    assert!(insert_prefix("namespace", "owner", &keys, &mut found, row).is_err());
    let mut row = prefix(&keyed, false);
    row.owner_user_id = "foreign".into();
    assert!(insert_prefix("namespace", "owner", &keys, &mut found, row).is_err());
    let mut row = prefix(&keyed, false);
    row.full_input_digest = digest("epoch-2", 1)?.as_str().into();
    assert!(insert_prefix("namespace", "owner", &keys, &mut found, row).is_err());
    let mut row = prefix(&keyed, false);
    row.episode_id.clear();
    assert!(insert_prefix("namespace", "owner", &keys, &mut found, row).is_err());
    let mut row = prefix(&keyed, false);
    row.full_input_digest = "invalid".into();
    assert!(
        insert_prefix(
            "namespace",
            "owner",
            &BTreeSet::from(["invalid"]),
            &mut found,
            row
        )
        .is_err()
    );
    assert!(found.is_empty());
    Ok(())
}
