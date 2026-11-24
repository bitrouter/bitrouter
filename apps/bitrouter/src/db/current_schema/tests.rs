use super::*;

#[test]
fn reviewed_model_has_complete_receipt_and_dispatch_shapes() -> Result<()> {
    let tables = expected(DatabaseBackend::Postgres)?;
    ensure!(!tables.is_empty());
    let records = tables
        .iter()
        .find(|table| table.name == "gateway_request_records")
        .context("gateway request records table")?;
    ensure!(records.primary == ["record_id"]);
    ensure!(records.indexes.iter().any(|index| {
        index.name == "idx_gateway_request_records_scope_kind"
            && index.columns == ["scope_id", "kind"]
    }));
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
fn owned_backends_retain_pending_projection_and_mysql_keeps_its_profile() -> Result<()> {
    let postgres = expected(DatabaseBackend::Postgres)?;
    let slots = postgres
        .iter()
        .find(|table| table.name == "tenancy_oauth_slots")
        .context("OAuth slot table")?;
    ensure!(
        slots
            .columns
            .iter()
            .any(|column| column.name == "pending_tenant_id")
    );
    ensure!(
        slots
            .columns
            .iter()
            .any(|column| column.name == "pending_execution_id")
    );
    for name in ["tenancy_receipt_archive", "tenancy_unknown_obligations"] {
        ensure!(postgres.iter().any(|table| table.name == name));
    }
    let tables = expected(DatabaseBackend::MySql)?;
    let slots = tables
        .iter()
        .find(|table| table.name == "tenancy_oauth_slots")
        .context("OAuth slot table")?;
    // MySQL retains its existing unowned storage profile.
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
fn native_capacity_keeps_integer_domains_without_sqlite_dynamic_type_checks() -> Result<()> {
    for backend in [DatabaseBackend::Postgres, DatabaseBackend::Sqlite] {
        let tables = expected(backend)?;
        let capacity = tables
            .iter()
            .find(|table| table.name == "tenancy_receipt_capacity")
            .context("capacity table")?;
        ensure!(
            capacity.checks.len()
                == if backend == DatabaseBackend::Sqlite {
                    18
                } else {
                    9
                }
        );
        ensure!(
            capacity
                .checks
                .iter()
                .any(|check| check.contains("unknown_count"))
        );
        ensure!(
            capacity
                .checks
                .iter()
                .any(|check| check.contains("continuation_epoch_required"))
        );
        for name in ["tenancy_execution_receipts", "tenancy_usage_evidence"] {
            let receipt = tables
                .iter()
                .find(|table| table.name == name)
                .context("receipt table")?;
            ensure!(receipt.checks == ["(\"storage_kind\" IN (0, 1))"]);
        }
        let ddl = statements(backend)?.join("\n");
        ensure!(ddl.contains("typeof(") == (backend == DatabaseBackend::Sqlite));
        let legacy = legacy_statements(backend)?.join("\n");
        ensure!(!legacy.contains("tenancy_receipt_capacity"));
        ensure!(
            legacy.contains("tenancy_receipt_archive")
                && legacy.contains("tenancy_unknown_obligations")
        );
        ensure!(legacy.contains("pending_tenant_id") && legacy.contains("pending_execution_id"));
        ensure!(!legacy.contains("typeof("));
    }
    Ok(())
}
#[test]
fn altered_keys_and_checks_are_not_hidden_by_catalog_names() -> Result<()> {
    let table = expected(DatabaseBackend::Postgres)?
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
    assert_eq!(
        normalized("storage_kind = ANY (ARRAY[0, 1])"),
        normalized("storage_kind IN (0, 1)")
    );
    assert_ne!(
        normalized("storage_kind = ANY (ARRAY[0, 2])"),
        normalized("storage_kind IN (0, 1)")
    );
    assert_ne!(
        normalized("other_column = ANY (ARRAY[0, 1])"),
        normalized("storage_kind IN (0, 1)")
    );
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
fn postgres_capacity_epoch_domain_accepts_exact_deparse_without_widening_membership() -> Result<()>
{
    let table = expected(DatabaseBackend::Postgres)?
        .into_iter()
        .find(|table| table.name == "tenancy_receipt_capacity")
        .context("capacity table")?;
    let constraints = |epoch: &str| {
        let mut actual = BTreeMap::from([(
            "capacity_pkey".into(),
            Constraint {
                kind: "p".into(),
                columns: table.primary.clone(),
                ..Default::default()
            },
        )]);
        for (index, check) in table.checks.iter().enumerate() {
            actual.insert(
                format!("capacity_check_{index}"),
                Constraint {
                    kind: "c".into(),
                    expression: if check.contains("continuation_epoch_required") {
                        epoch.to_owned()
                    } else {
                        check.clone()
                    },
                    ..Default::default()
                },
            );
        }
        actual
    };
    for expression in [
        "(continuation_epoch_required = ANY (ARRAY[0, 1]))",
        r#"(("continuation_epoch_required") = ANY ((ARRAY[0::integer, 1::integer])))"#,
    ] {
        validate_constraints(DatabaseBackend::Postgres, &table, constraints(expression))?;
        ensure!(!check_matches(
            DatabaseBackend::MySql,
            &normalized("continuation_epoch_required IN (0, 1)"),
            &normalized(expression)
        ));
    }
    for expression in [
        "(continuation_epoch_required = ANY (ARRAY[0, 1, 2]))",
        "(continuation_epoch_required = ANY (ARRAY[0]))",
        "(continuation_epoch_required = ANY (ARRAY[1]))",
        "(continuation_epoch_required = ANY (ARRAY[0, 1, NULL]))",
        "(continuation_epoch_required = ANY (ARRAY['0', '1']))",
        "(continuation_epoch_required <> ANY (ARRAY[0, 1]))",
        "(continuation_epoch_required = ALL (ARRAY[0, 1]))",
        "(unknown_count = ANY (ARRAY[0, 1]))",
        "(continuation_epoch_required = ANY (ARRAY[0, 1]) OR true)",
    ] {
        ensure!(
            validate_constraints(DatabaseBackend::Postgres, &table, constraints(expression))
                .is_err(),
            "accepted altered epoch constraint: {expression}"
        );
    }
    Ok(())
}

#[test]
fn default_values_cannot_be_replaced_with_expressions() -> Result<()> {
    let column = Column {
        name: "active".into(),
        kind: "integer".into(),
        mysql_kind: None,
        sqlite_kind: None,
        owned_only: false,
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
    let tables = expected(DatabaseBackend::Postgres)?;
    let find = |name: &str, column: &str| {
        tables
            .iter()
            .find(|table| table.name == name)
            .and_then(|table| table.columns.iter().find(|field| field.name == column))
            .context("backend content field")
    };
    let content = find("gateway_request_records", "body")?;
    ensure!(kind(DatabaseBackend::MySql, content) == "longtext");
    ensure!(kind(DatabaseBackend::Postgres, content) == "text");
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
    let table = expected(DatabaseBackend::Postgres)?
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

#[test]
fn postgres_obligation_checks_preserve_exact_bounds_and_membership() {
    for (expected, observed) in [
        (
            "source_mask BETWEEN 1 AND 7",
            "source_mask >= 1 AND source_mask <= 7",
        ),
        (
            "first_nanos BETWEEN 0 AND 999999999",
            "first_nanos >= 0 AND first_nanos <= 999999999",
        ),
        ("escalated IN (0,1)", "escalated = ANY (ARRAY[0,1])"),
    ] {
        assert!(check_matches(
            DatabaseBackend::Postgres,
            &normalized(expected),
            &normalized(observed)
        ));
        assert!(!check_matches(
            DatabaseBackend::Postgres,
            &normalized(expected),
            &normalized(&observed.replace('1', "2").replace('9', "8"))
        ));
        assert!(!check_matches(
            DatabaseBackend::Postgres,
            &normalized(expected),
            &normalized(
                &observed
                    .replace("source_mask", "other_column")
                    .replace("first_nanos", "other_column")
                    .replace("escalated", "other_column")
            )
        ));
    }
}
