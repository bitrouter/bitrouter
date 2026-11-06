use super::*;
fn selected() -> String {
    let mut columns = Vec::new();
    for name in ["tenant_id", "execution_id"] {
        columns.push(format!("CASE WHEN typeof({name})='text' AND length(CAST({name} AS BLOB)) BETWEEN 1 AND 128 THEN {name} ELSE NULL END AS {name}"));
    }
    for name in [
        "source_mask",
        "first_seconds",
        "first_nanos",
        "due_seconds",
        "due_nanos",
        "escalated",
    ] {
        columns.push(format!(
            "CASE WHEN typeof({name})='integer' THEN {name} ELSE NULL END AS {name}"
        ));
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
    charge(1, 1024)?;
    let row = db
        .query_one(sql(
            format!(
                "SELECT {} FROM {TABLE} WHERE installation_id=? AND tenant_id=? AND execution_id=?",
                selected()
            ),
            [
                installation.as_str().into(),
                key.tenant.as_str().into(),
                key.execution.as_str().into(),
            ],
        ))
        .await?;
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
    charge(PAGE, PAGE * 1024)?;
    let mut query = format!("SELECT {} FROM {TABLE} WHERE installation_id=?", selected());
    let mut values: Vec<Value> = vec![installation.as_str().into()];
    if let Some(after) = after {
        // Row-value comparison preserves the composite index's keyset order.
        // https://www.sqlite.org/rowvalue.html#scrolling_window_queries
        query.push_str(" AND (tenant_id,execution_id)>(?,?)");
        values.extend([
            after.tenant.as_str().into(),
            after.execution.as_str().into(),
        ]);
    }
    query.push_str(" ORDER BY tenancy_unknown_obligations.tenant_id,tenancy_unknown_obligations.execution_id LIMIT ?");
    values.push(i64::try_from(PAGE)?.into());
    let rows = db.query_all(sql(query, values)).await?;
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
    charge(1, 1024)?;
    let query = format!(
        "SELECT {} FROM {TABLE} WHERE installation_id=? AND escalated=0 ORDER BY tenancy_unknown_obligations.due_seconds,tenancy_unknown_obligations.due_nanos,tenancy_unknown_obligations.tenant_id,tenancy_unknown_obligations.execution_id LIMIT 1",
        selected()
    );
    let row = db
        .query_one(sql(query, [installation.as_str().into()]))
        .await?;
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
    charge(PAGE, PAGE * 1024)?;
    let query = format!(
        "SELECT {} FROM {TABLE} WHERE installation_id=? AND escalated=0 AND (due_seconds,due_nanos)<=(?,?) ORDER BY tenancy_unknown_obligations.due_seconds,tenancy_unknown_obligations.due_nanos,tenancy_unknown_obligations.tenant_id,tenancy_unknown_obligations.execution_id LIMIT ?",
        selected()
    );
    let rows = db
        .query_all(sql(
            query,
            [
                installation.as_str().into(),
                now.timestamp().into(),
                i64::from(now.timestamp_subsec_nanos()).into(),
                i64::try_from(PAGE)?.into(),
            ],
        ))
        .await?;
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
    charge(1, 4096)?;
    let statement = if let Some(value) = value {
        value.validate()?;
        sql(
            "INSERT INTO tenancy_unknown_obligations (installation_id,tenant_id,execution_id,source_mask,first_seconds,first_nanos,due_seconds,due_nanos,escalated) VALUES (?,?,?,?,?,?,?,?,?) ON CONFLICT (installation_id,tenant_id,execution_id) DO UPDATE SET source_mask=excluded.source_mask,first_seconds=excluded.first_seconds,first_nanos=excluded.first_nanos,due_seconds=excluded.due_seconds,due_nanos=excluded.due_nanos,escalated=excluded.escalated",
            [
                installation.as_str().into(),
                key.tenant.as_str().into(),
                key.execution.as_str().into(),
                i64::from(value.source_mask).into(),
                value.first.timestamp().into(),
                i64::from(value.first.timestamp_subsec_nanos()).into(),
                value.due.timestamp().into(),
                i64::from(value.due.timestamp_subsec_nanos()).into(),
                i64::from(value.escalated).into(),
            ],
        )
    } else {
        ensure!(existed, "unknown removal without prior row");
        sql(
            "DELETE FROM tenancy_unknown_obligations WHERE installation_id=? AND tenant_id=? AND execution_id=?",
            [
                installation.as_str().into(),
                key.tenant.as_str().into(),
                key.execution.as_str().into(),
            ],
        )
    };
    ensure!(
        tx.execute(statement).await?.rows_affected() == 1,
        "unknown projection write conflict"
    );
    charge(0, 0)?;
    Ok(())
}
