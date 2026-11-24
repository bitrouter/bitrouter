use super::*;
fn selected(backend: DatabaseBackend) -> String {
    let mut columns = Vec::new();
    for name in ["tenant_id", "execution_id"] {
        columns.push(if backend == DatabaseBackend::Sqlite {
            format!("CASE WHEN typeof({name})='text' AND length(CAST({name} AS BLOB)) BETWEEN 1 AND 128 THEN {name} ELSE NULL END AS {name}")
        } else {
            format!("CASE WHEN octet_length({name}) BETWEEN 1 AND 128 THEN {name} ELSE NULL END AS {name}")
        });
    }
    for name in [
        "source_mask",
        "first_seconds",
        "first_nanos",
        "due_seconds",
        "due_nanos",
        "escalated",
    ] {
        columns.push(if backend == DatabaseBackend::Sqlite {
            format!("CASE WHEN typeof({name})='integer' THEN {name} ELSE NULL END AS {name}")
        } else {
            format!("CAST({name} AS BIGINT) AS {name}")
        });
    }
    columns.join(",")
}
fn decode(row: QueryResult) -> Result<Record> {
    let time = |prefix: &str| -> Result<DateTime<Utc>> {
        let seconds = row.try_get::<i64>("", &format!("{prefix}_seconds"))?;
        let nanos = u32::try_from(row.try_get::<i64>("", &format!("{prefix}_nanos"))?)?;
        ensure!(nanos < 1_000_000_000, "unknown timestamp nanos");
        DateTime::from_timestamp(seconds, nanos).context("unknown timestamp range")
    };
    let escalated = row.try_get::<i64>("", "escalated")?;
    ensure!(
        (0..=1).contains(&escalated),
        "unknown escalation projection"
    );
    let value = Record {
        key: Key {
            tenant: row.try_get::<String>("", "tenant_id")?.try_into()?,
            execution: row.try_get::<String>("", "execution_id")?.try_into()?,
        },
        source_mask: u8::try_from(row.try_get::<i64>("", "source_mask")?)?,
        first: time("first")?,
        due: time("due")?,
        escalated: escalated == 1,
    };
    value.validate()?;
    Ok(value)
}
pub(super) async fn lookup(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    key: &Key,
    charge: &Charge<'_>,
) -> Result<Option<Record>> {
    let backend = supported(db)?;
    charge(1, 1024)?;
    let mut query = Query::select();
    query
        .from(Alias::new(TABLE))
        .expr(Expr::cust(selected(backend)))
        .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
        .and_where(Expr::col(Alias::new("tenant_id")).eq(key.tenant.as_str()))
        .and_where(Expr::col(Alias::new("execution_id")).eq(key.execution.as_str()))
        .limit(1);
    let row = db.query_one(backend.build(&query)).await?;
    let value = row.map(decode).transpose()?;
    charge(0, 0)?;
    Ok(value)
}
pub(super) async fn page(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    after: Option<&Key>,
    charge: &Charge<'_>,
) -> Result<Vec<Record>> {
    let backend = supported(db)?;
    charge(PAGE, PAGE * 1024)?;
    let mut query = Query::select();
    query
        .from(Alias::new(TABLE))
        .expr(Expr::cust(selected(backend)))
        .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
        .order_by((Alias::new(TABLE), Alias::new("tenant_id")), Order::Asc)
        .order_by((Alias::new(TABLE), Alias::new("execution_id")), Order::Asc)
        .limit(PAGE as u64);
    if let Some(after) = after {
        let tenant = Expr::col(Alias::new("tenant_id"));
        query.and_where(
            tenant.clone().gt(after.tenant.as_str()).or(tenant
                .eq(after.tenant.as_str())
                .and(Expr::col(Alias::new("execution_id")).gt(after.execution.as_str()))),
        );
    }
    let rows = db.query_all(backend.build(&query)).await?;
    let result = rows.into_iter().map(decode).collect::<Result<Vec<_>>>()?;
    let mut previous = after;
    for row in &result {
        ensure!(
            previous.is_none_or(|key| key < &row.key),
            "unknown page key order"
        );
        previous = Some(&row.key);
    }
    charge(0, 0)?;
    Ok(result)
}
pub(super) async fn next_due(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    charge: &Charge<'_>,
) -> Result<Option<DateTime<Utc>>> {
    let backend = supported(db)?;
    charge(1, 1024)?;
    let mut query = Query::select();
    query
        .from(Alias::new(TABLE))
        .expr(Expr::cust(selected(backend)))
        .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
        .and_where(Expr::col(Alias::new("escalated")).eq(0_i32))
        .order_by((Alias::new(TABLE), Alias::new("due_seconds")), Order::Asc)
        .order_by((Alias::new(TABLE), Alias::new("due_nanos")), Order::Asc)
        .order_by((Alias::new(TABLE), Alias::new("tenant_id")), Order::Asc)
        .order_by((Alias::new(TABLE), Alias::new("execution_id")), Order::Asc)
        .limit(1);
    let row = db.query_one(backend.build(&query)).await?;
    let due = row.map(decode).transpose()?.map(|row| row.due);
    charge(0, 0)?;
    Ok(due)
}

pub(super) async fn due_page(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    now: DateTime<Utc>,
    charge: &Charge<'_>,
) -> Result<Vec<Record>> {
    let backend = supported(db)?;
    charge(PAGE, PAGE * 1024)?;
    let seconds = Expr::col(Alias::new("due_seconds"));
    let mut query = Query::select();
    query
        .from(Alias::new(TABLE))
        .expr(Expr::cust(selected(backend)))
        .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
        .and_where(Expr::col(Alias::new("escalated")).eq(0_i32))
        .and_where(
            seconds
                .clone()
                .lt(now.timestamp())
                .or(seconds.eq(now.timestamp()).and(
                    Expr::col(Alias::new("due_nanos"))
                        .lte(i32::try_from(now.timestamp_subsec_nanos())?),
                )),
        )
        .order_by((Alias::new(TABLE), Alias::new("due_seconds")), Order::Asc)
        .order_by((Alias::new(TABLE), Alias::new("due_nanos")), Order::Asc)
        .order_by((Alias::new(TABLE), Alias::new("tenant_id")), Order::Asc)
        .order_by((Alias::new(TABLE), Alias::new("execution_id")), Order::Asc)
        .limit(PAGE as u64);
    let rows = db.query_all(backend.build(&query)).await?;
    let result = rows.into_iter().map(decode).collect::<Result<Vec<_>>>()?;
    charge(0, 0)?;
    Ok(result)
}
pub(super) async fn write(
    tx: &DatabaseTransaction,
    installation: &InstallationId,
    key: &Key,
    existed: bool,
    value: Option<&Record>,
    charge: &Charge<'_>,
) -> Result<()> {
    let backend = supported(tx)?;
    charge(1, 4096)?;
    let statement = if let Some(value) = value {
        value.validate()?;
        let mut insert = Query::insert();
        insert
            .into_table(Alias::new(TABLE))
            .columns(
                [
                    "installation_id",
                    "tenant_id",
                    "execution_id",
                    "source_mask",
                    "first_seconds",
                    "first_nanos",
                    "due_seconds",
                    "due_nanos",
                    "escalated",
                ]
                .map(Alias::new),
            )
            .values([
                installation.as_str().into(),
                key.tenant.as_str().into(),
                key.execution.as_str().into(),
                i32::from(value.source_mask).into(),
                value.first.timestamp().into(),
                i32::try_from(value.first.timestamp_subsec_nanos())?.into(),
                value.due.timestamp().into(),
                i32::try_from(value.due.timestamp_subsec_nanos())?.into(),
                i32::from(value.escalated).into(),
            ])?
            .on_conflict(
                OnConflict::columns(
                    ["installation_id", "tenant_id", "execution_id"].map(Alias::new),
                )
                .update_columns(
                    [
                        "source_mask",
                        "first_seconds",
                        "first_nanos",
                        "due_seconds",
                        "due_nanos",
                        "escalated",
                    ]
                    .map(Alias::new),
                )
                .to_owned(),
            );
        backend.build(&insert)
    } else {
        ensure!(existed, "unknown removal without prior row");
        let mut delete = Query::delete();
        delete
            .from_table(Alias::new(TABLE))
            .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
            .and_where(Expr::col(Alias::new("tenant_id")).eq(key.tenant.as_str()))
            .and_where(Expr::col(Alias::new("execution_id")).eq(key.execution.as_str()));
        backend.build(&delete)
    };
    ensure!(
        tx.execute(statement).await?.rows_affected() == 1,
        "unknown projection write conflict"
    );
    if existed != value.is_some() {
        crate::db::migration::capacity::adjust(
            tx,
            crate::db::migration::capacity::Delta {
                unknown_count: if value.is_some() { 1 } else { -1 },
                ..Default::default()
            },
        )
        .await?;
    }
    charge(0, 0)?;
    Ok(())
}
