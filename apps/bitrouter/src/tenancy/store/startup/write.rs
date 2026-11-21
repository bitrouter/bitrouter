use super::*;
use sea_orm::sea_query::Query;

pub(super) async fn write_header_in(db: &impl ConnectionTrait, next: &LoadedHeader) -> Result<()> {
    let header = &next.header;
    chrono::DateTime::parse_from_rfc3339(&header.created_at)?;
    let ordinary = i64::try_from(header.operation_ids)?;
    let safety = i64::try_from(header.safety_operation_ids)?;
    let revision = i64::try_from(header.revision.get())?;
    let mut query = Query::update();
    query
        .table(Alias::new(Table::Header.name()))
        .values([
            (Alias::new("authority_revision"), Expr::val(revision).into()),
            (Alias::new("operation_ids"), Expr::val(ordinary).into()),
            (Alias::new("safety_operation_ids"), Expr::val(safety).into()),
            (Alias::new("saturated"), Expr::val(header.saturated).into()),
        ])
        .and_where(Expr::col(Alias::new("singleton")).eq(1_i32))
        .and_where(Expr::col(Alias::new("schema_version")).eq(next.format.number()))
        .and_where(Expr::col(Alias::new("installation_id")).eq(header.installation.as_str()))
        .and_where(Expr::col(Alias::new("created_at")).eq(header.created_at.as_str()))
        .and_where(Expr::col(Alias::new("authority_revision")).lte(revision))
        .and_where(Expr::col(Alias::new("operation_ids")).lte(ordinary))
        .and_where(Expr::col(Alias::new("safety_operation_ids")).lte(safety));
    if !header.saturated {
        query.and_where(Expr::col(Alias::new("saturated")).eq(false));
    }
    ensure!(
        db.execute(db.get_database_backend().build(&query))
            .await?
            .rows_affected()
            == 1,
        "authority header changed, format differs or stopped state would be cleared"
    );
    Ok(())
}
