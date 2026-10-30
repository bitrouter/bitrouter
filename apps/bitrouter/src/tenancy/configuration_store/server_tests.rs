//! Opt-in server checks use only UUID-qualified fixtures, rolled back after use.

use super::*;

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL for a dedicated PostgreSQL or MySQL test database"]
async fn server_configuration_store_behaviors() -> Result<()> {
    let database_url = std::env::var("BITROUTER_TEST_DATABASE_URL").context(
        "set BITROUTER_TEST_DATABASE_URL to a dedicated test database; no fallback is used",
    )?;
    let parsed = url::Url::parse(&database_url)
        .context("BITROUTER_TEST_DATABASE_URL must be a valid server database URL")?;
    match parsed.scheme() {
        "postgres" | "postgresql" => {
            ensure!(
                cfg!(feature = "db-postgres"),
                "enable the db-postgres feature"
            );
        }
        "mysql" => ensure!(cfg!(feature = "db-mysql"), "enable the db-mysql feature"),
        _ => anyhow::bail!("BITROUTER_TEST_DATABASE_URL must select PostgreSQL or MySQL"),
    }
    ensure!(
        !parsed.path().trim_matches('/').is_empty(),
        "BITROUTER_TEST_DATABASE_URL must explicitly name a dedicated test database"
    );
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        let db = crate::db::connect(&database_url).await?;
        crate::db::run_migrations(&db).await?;
        let tx = db.begin().await?;
        let result = exercise(&tx).await;
        tx.rollback().await?;
        db.close().await?;
        result
    })
    .await
    .context("configuration store regression exceeded its two-minute deadline")?
}

async fn exercise(db: &impl ConnectionTrait) -> Result<()> {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let tenant = format!("store-{suffix}");
    let case_tenant = format!("Store-{suffix}");
    for owner in [&tenant, &case_tenant] {
        let insert = Query::insert()
            .into_table(Alias::new("tenancy_tenants"))
            .columns(["id", "revision", "state", "created_at"].map(Alias::new))
            .values([
                owner.as_str().into(),
                "initial".into(),
                "active".into(),
                "2026-01-01T00:00:00Z".into(),
            ])?
            .to_owned();
        db.execute(db.get_database_backend().build(&insert)).await?;
    }

    let mut original = artifact(&tenant, "v1")?;
    original.payload = json!({"ciphertext": "é".repeat(40_000)});
    insert_resource(db, &original).await?;
    insert_resource(db, &original).await?;
    let mut changed = original.clone();
    changed.payload = json!({"ciphertext": "changed"});
    conflict(insert_resource(db, &changed).await)?;
    ensure!(
        resource(db, &original.reference)
            .await?
            .context("artifact missing")?
            .payload
            == original.payload
    );

    // Every component of the qualified key is material; case is never folded.
    let mut variants = vec![artifact(&case_tenant, "v1")?, artifact(&tenant, "V1")?];
    let mut route_case = original.clone();
    route_case.reference.resource_id = "Route".try_into()?;
    variants.push(route_case);
    let mut pool = original.clone();
    pool.reference.owner_kind = OwnerKind::OperatorPool;
    variants.push(pool);
    let mut policy = original.clone();
    policy.reference.resource_kind = ResourceKind::Policy;
    variants.push(policy);
    for (index, mut variant) in variants.into_iter().enumerate() {
        variant.payload = json!({"ciphertext": format!("distinct-{index}")});
        insert_resource(db, &variant).await?;
        ensure!(
            resource(db, &variant.reference)
                .await?
                .context("qualified artifact missing")?
                .payload
                == variant.payload
        );
    }
    ensure!(
        resource(db, &original.reference)
            .await?
            .context("original artifact missing")?
            .payload
            == original.payload
    );

    let mut first = draft(&tenant, "v1", 7)?;
    first.compiled = json!({"ciphertext": "é".repeat(40_000)});
    let next = draft(&tenant, "v2", 9)?;
    let foreign = draft(&case_tenant, "foreign-only", 8)?;
    let case_revision = draft(&tenant, "V1", 7)?;
    let case_owner = draft(&case_tenant, "v1", 7)?;
    for record in [&first, &next, &foreign, &case_revision, &case_owner] {
        insert_configuration(db, record).await?;
        insert_configuration(db, record).await?;
        let loaded = configuration(db, &record.revision.tenant_id, &record.revision.revision_id)
            .await?
            .context("configuration missing")?;
        ensure!(loaded.compiled == record.compiled && loaded.dependencies == record.dependencies);
        ensure!(loaded.authority_revision == record.authority_revision);
    }
    let mut changed = first.clone();
    changed.compiled = json!({"ciphertext": "changed"});
    conflict(insert_configuration(db, &changed).await)?;
    let owner = &first.revision.tenant_id;
    let v1 = &first.revision.revision_id;
    let v2 = &next.revision.revision_id;
    ensure!(
        set_active(db, owner, None, v1, 6.try_into()?)
            .await
            .is_err()
    );
    ensure!(
        set_active(
            db,
            owner,
            None,
            &foreign.revision.revision_id,
            8.try_into()?
        )
        .await
        .is_err()
    );
    set_active(db, owner, None, v1, 8.try_into()?).await?;
    // An expected absence conflict must not abort PostgreSQL's transaction,
    // nor pass as success through MySQL's FOUND_ROWS no-op upsert behavior.
    for (target, proposed) in [(v1, 8_u64), (v1, 9), (v2, 10)] {
        conflict(set_active(db, owner, None, target, proposed.try_into()?).await)?;
        let select = Query::select()
            .columns(["revision_id", "authority_revision"].map(Alias::new))
            .from(Alias::new(ACTIVE))
            .and_where(Expr::col(Alias::new("tenant_id")).eq(owner.as_str()))
            .to_owned();
        let row = db
            .query_one(db.get_database_backend().build(&select))
            .await?
            .context("active pointer missing after rejected CAS")?;
        ensure!(text(&row, "revision_id")? == v1.as_str() && authority(&row)?.get() == 8);
    }
    conflict(set_active(db, owner, Some(v2), v2, 10.try_into()?).await)?;
    conflict(set_active(db, owner, Some(v1), v1, 8.try_into()?).await)?;
    set_active(db, owner, Some(v1), v2, 10.try_into()?).await?;
    conflict(set_active(db, owner, Some(v1), v1, 11.try_into()?).await)?;
    set_active(db, owner, Some(v2), v1, 11.try_into()?).await?;
    let select = Query::select()
        .columns(["revision_id", "authority_revision"].map(Alias::new))
        .from(Alias::new(ACTIVE))
        .and_where(Expr::col(Alias::new("tenant_id")).eq(owner.as_str()))
        .to_owned();
    let row = db
        .query_one(db.get_database_backend().build(&select))
        .await?
        .context("active pointer missing")?;
    ensure!(text(&row, "revision_id")? == v1.as_str() && authority(&row)?.get() == 11);

    // Valid JSON with fewer than 8 Mi characters, but more than 8 MiB: the
    // server must project NULL before either getter receives the oversized cell.
    let oversized = json!("é".repeat(MAX_ENVELOPE_BYTES / 2)).to_string();
    let update = Query::update()
        .table(Alias::new(RESOURCES))
        .value(Alias::new("payload_json"), oversized.clone())
        .and_where(Expr::col(Alias::new("owner_kind")).eq("tenant"))
        .and_where(Expr::col(Alias::new("owner_id")).eq(tenant.as_str()))
        .and_where(Expr::col(Alias::new("resource_kind")).eq("route"))
        .and_where(Expr::col(Alias::new("resource_id")).eq("route"))
        .and_where(Expr::col(Alias::new("revision_id")).eq("v1"))
        .to_owned();
    ensure!(
        db.execute(db.get_database_backend().build(&update))
            .await?
            .rows_affected()
            == 1
    );
    let update = Query::update()
        .table(Alias::new(CONFIGURATIONS))
        .value(Alias::new("compiled_json"), oversized)
        .and_where(Expr::col(Alias::new("tenant_id")).eq(owner.as_str()))
        .and_where(Expr::col(Alias::new("revision_id")).eq(v1.as_str()))
        .to_owned();
    ensure!(
        db.execute(db.get_database_backend().build(&update))
            .await?
            .rows_affected()
            == 1
    );
    let row = resource_row(db, &original.reference, false)
        .await?
        .context("artifact row missing")?;
    ensure!(row.try_get::<Option<String>>("", "payload_json")?.is_none());
    ensure!(resource(db, &original.reference).await.is_err());
    let row = configuration_row(db, owner, v1, false)
        .await?
        .context("configuration row missing")?;
    ensure!(
        row.try_get::<Option<String>>("", "compiled_json")?
            .is_none()
    );
    ensure!(configuration(db, owner, v1).await.is_err());
    Ok(())
}
