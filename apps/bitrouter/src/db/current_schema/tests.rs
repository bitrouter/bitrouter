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
            == 451
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
fn default_values_cannot_be_replaced_with_expressions() -> Result<()> {
    let column = Column {
        name: "active".into(),
        kind: "integer".into(),
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
