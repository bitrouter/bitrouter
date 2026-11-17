use super::*;

#[test]
fn reviewed_model_has_complete_receipt_and_dispatch_shapes() -> Result<()> {
    let tables = expected()?;
    ensure!(tables.len() == 52);
    ensure!(
        tables
            .iter()
            .map(|table| table.columns.len())
            .sum::<usize>()
            == 449
    );
    for name in ["tenancy_execution_receipts", "tenancy_dispatch_intents"] {
        let table = tables
            .iter()
            .find(|table| table.name == name)
            .context("missing table")?;
        ensure!(table.primary == ["installation_id", "tenant_id", "execution_id"]);
        ensure!(
            table
                .columns
                .iter()
                .filter(|column| table.primary.contains(&column.name))
                .all(|column| column.binary && !column.nullable)
        );
    }
    Ok(())
}

#[test]
fn server_model_excludes_sqlite_only_pending_obligation_projection() -> Result<()> {
    let tables = expected()?;
    let slots = tables
        .iter()
        .find(|table| table.name == "tenancy_oauth_slots")
        .context("OAuth slot table")?;
    // m043 adds these lookup projections only on SQLite. Servers retain the
    // m037 slot columns and state index; the baseline never runs m043 there.
    ensure!(slots.columns.len() == 11);
    ensure!(slots.columns.iter().all(|column| !matches!(
        column.name.as_str(),
        "pending_tenant_id" | "pending_execution_id"
    )));
    ensure!(slots.indexes.len() == 1);
    ensure!(slots.indexes[0].name == "idx_tenancy_oauth_pending");
    ensure!(slots.indexes[0].columns == ["installation_id", "state", "slot_key"]);
    Ok(())
}
#[test]
fn altered_keys_and_checks_are_not_hidden_by_catalog_names() -> Result<()> {
    let table = expected()?
        .into_iter()
        .find(|table| table.name == "tenancy_dispatch_intents")
        .context("dispatch table")?;
    ensure!(validate_constraints(DatabaseBackend::Postgres, &table, BTreeMap::new()).is_err());
    let mut keys = BTreeMap::new();
    keys.insert(
        "same-name".into(),
        Constraint {
            kind: "p".into(),
            columns: vec!["execution_id".into()],
            ..Default::default()
        },
    );
    ensure!(validate_constraints(DatabaseBackend::Postgres, &table, keys).is_err());
    Ok(())
}
#[test]
fn casts_and_backend_check_rendering_preserve_literal_case_and_membership() {
    assert_eq!(
        normalized(r#"(("publication_generation")::text <> ''::text)"#),
        normalized(r#""publication_generation" <> ''"#)
    );
    assert_eq!(
        normalized(
            r#"(("publication_state")::text = ANY ((ARRAY['provisional'::character varying, 'delivering'::character varying, 'active'::character varying])::text[]))"#
        ),
        normalized(r#""publication_state" IN ('provisional', 'delivering', 'active')"#)
    );
    assert_ne!(
        normalized("state IN ('active')"),
        normalized("state IN ('ACTIVE')")
    );
    assert_ne!(
        normalized("state IN ('active')"),
        normalized("state IN ('active','other')")
    );
    assert_ne!(normalized("'local::text'"), normalized("'local'"));
    assert_ne!(normalized("'_utf8mb4local'"), normalized("'local'"));
}

#[test]
fn mysql_check_catalog_matches_escaped_quotes_without_widening_literals() {
    for (expected, observed) in [
        (
            r#""publication_state" IN ('provisional', 'delivering', 'active')"#,
            r"(`publication_state` in (_utf8mb4\'provisional\',_utf8mb4\'delivering\',_utf8mb4\'active\'))",
        ),
        (
            r#""publication_generation" <> ''"#,
            r"(`publication_generation` <> _utf8mb4\'\')",
        ),
    ] {
        let expected = normalized(expected);
        let observed = normalized(observed);
        assert!(check_matches(DatabaseBackend::MySql, &expected, &observed));
        assert!(!check_matches(
            DatabaseBackend::Postgres,
            &expected,
            &observed
        ));
        assert!(!check_matches(
            DatabaseBackend::MySql,
            &expected,
            &observed.replacen(r"\'", r"\\'", 1),
        ));
        assert!(!check_matches(
            DatabaseBackend::MySql,
            &expected,
            &observed.replacen(r"\'", r"\'X", 1),
        ));
    }
}

#[test]
fn default_values_cannot_be_replaced_with_expressions() -> Result<()> {
    let column = Column {
        name: "active".into(),
        kind: "integer".into(),
        mysql_kind: None,
        nullable: false,
        binary: false,
        automatic: false,
        default: Some("1".into()),
    };
    ensure!(default_matches(
        DatabaseBackend::Postgres,
        &column,
        Some("1")
    ));
    ensure!(!default_matches(
        DatabaseBackend::Postgres,
        &column,
        Some("1 + 0")
    ));
    ensure!(!default_matches(DatabaseBackend::Postgres, &column, None));
    let text = Column {
        kind: "varchar".into(),
        default: Some("'local'".into()),
        ..column
    };
    ensure!(default_matches(
        DatabaseBackend::MySql,
        &text,
        Some("local")
    ));
    for changed in ["LOCAL", "local::text", "_utf8mb4local", "'local'"] {
        ensure!(!default_matches(
            DatabaseBackend::MySql,
            &text,
            Some(changed)
        ));
    }
    Ok(())
}

#[test]
fn mysql_content_types_follow_explicit_backend_ddl() -> Result<()> {
    let tables = expected()?;
    // m019, m020 and m027 widen only these non-tenancy content fields.
    for (table, column) in [
        ("acp_capture_events", "event_json"),
        ("acp_checkpoints", "manifest_json"),
        ("acp_resource_observations", "observation_json"),
        ("acp_assessment_revisions", "revision_json"),
        ("checkpoint_evolution", "body"),
        ("bro_managed_sessions", "payload"),
    ] {
        let field = tables
            .iter()
            .find(|candidate| candidate.name == table)
            .and_then(|table| table.columns.iter().find(|field| field.name == column))
            .context("backend content field")?;
        ensure!(kind(DatabaseBackend::MySql, field) == "longtext");
        ensure!(kind(DatabaseBackend::Postgres, field) == "text");
    }
    // Adjacent JSON and timestamp fields retain their original TEXT declaration.
    for (table, column) in [
        ("acp_capture_connections", "metadata_json"),
        ("acp_resource_observations", "observed_at"),
    ] {
        let field = tables
            .iter()
            .find(|candidate| candidate.name == table)
            .and_then(|table| table.columns.iter().find(|field| field.name == column))
            .context("unchanged text field")?;
        ensure!(kind(DatabaseBackend::MySql, field) == "text");
    }
    Ok(())
}

fn outbox_index_fixture() -> BTreeMap<String, ObservedIndex> {
    [
        (
            "idx_traj32_outbox_pending",
            vec![
                "installation_id",
                "tenant_id",
                "delivered_at",
                "attempts",
                "created_at",
            ],
            vec![None, None, Some(64), None, Some(64)],
            false,
        ),
        (
            "idx_trajectory_outbox_delivery_order",
            vec!["delivered_at", "attempts", "created_at", "outbox_id"],
            vec![None, None, None, None],
            false,
        ),
        (
            "idx_trajectory_outbox_pending",
            vec![
                "owner_user_id",
                "delivered_at",
                "attempts",
                "created_at",
                "outbox_id",
            ],
            vec![None, Some(64), None, Some(64), None],
            false,
        ),
        ("PRIMARY", vec!["outbox_id"], vec![None], true),
    ]
    .into_iter()
    .map(|(name, columns, prefixes, primary)| {
        (
            name.into(),
            ObservedIndex {
                columns: columns.into_iter().map(str::to_owned).collect(),
                prefixes,
                unique: primary,
                primary,
            },
        )
    })
    .collect()
}

#[test]
fn mysql_index_prefixes_are_exact_and_do_not_apply_to_postgres() -> Result<()> {
    let table = expected()?
        .into_iter()
        .find(|table| table.name == "trajectory_outbox")
        .context("outbox table")?;
    let current = outbox_index_fixture();
    validate_indexes(DatabaseBackend::MySql, &table, current.clone())?;
    for (name, ordinal, prefix) in [
        ("idx_traj32_outbox_pending", 2, None),
        ("idx_traj32_outbox_pending", 2, Some(63)),
        ("idx_traj32_outbox_pending", 2, Some(65)),
        ("idx_trajectory_outbox_pending", 1, None),
        ("idx_trajectory_outbox_delivery_order", 0, Some(64)),
        ("PRIMARY", 0, Some(64)),
    ] {
        let mut changed = current.clone();
        changed.get_mut(name).context("fixture index")?.prefixes[ordinal] = prefix;
        ensure!(
            validate_indexes(DatabaseBackend::MySql, &table, changed).is_err(),
            "accepted altered prefix on {name}"
        );
    }
    let mut postgres = current;
    for index in postgres.values_mut() {
        index.prefixes.fill(None);
    }
    validate_indexes(DatabaseBackend::Postgres, &table, postgres)?;
    Ok(())
}
