//! Real guarded startup reads preserve INT4 versions and INT8 counters.
use super::*;
use crate::tenancy::store::{self, directory::CapacityExceeded, startup};
use sea_orm::{
    ConnectionTrait,
    sea_query::{Alias, Expr, Query},
};

pub(super) async fn check(database: &mut database::Fixture) -> Result<()> {
    let db = database.connect().await?;
    let fixture =
        Fixture::with_database(db.clone(), &mut |service| database.retain(service)).await?;
    let response = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(
                fixture.tenant.clone(),
                fixture.configuration_input("projection-config")?,
            ),
        )
        .await
        .map_err(failure)?;
    let StoredResponse::Configuration(configuration) = response else {
        anyhow::bail!("actual configuration was not retained")
    };
    ensure!(configuration.schema_version == 1);
    let expected = fixture.service.current().map_err(failure)?;
    ensure!(expected.revision.get() == 4 && expected.operation_ids == 4);
    fixture.service.shutdown().await?;
    database.quiesce().await?;

    // The complete reader consumes real nonempty operation hash_version fields
    // as i32, as well as the guarded header and retained configuration bodies.
    let slot = database.start(&db).await?;
    let checked = async {
        let tx = database.tx(slot)?;
        let loaded = store::load_existing(tx, Utc::now()).await?;
        ensure!(
            loaded.installation == expected.installation
                && loaded.revision == expected.revision
                && loaded.operation_ids == expected.operation_ids
                && loaded.safety_operation_ids == 0
                && !loaded.saturated
        );
        let config = crate::tenancy::configuration_store::configuration(
            tx,
            &fixture.tenant,
            &configuration.revision_id,
        )
        .await?
        .context("retained configuration missing")?;
        ensure!(
            config.revision.schema_version == 1 && config.authority_revision == expected.revision
        );
        let error = startup::read_header_in(tx, 1)
            .await
            .err()
            .context("oversized header accepted")?;
        ensure!(
            error.is::<CapacityExceeded>(),
            "wrong bounded refusal: {error:#}"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    database.rollback(slot).await?;
    checked?;

    // A wide revision and nonzero ordinary/safety counters must still decode
    // as i64; replacing every typed NULL with INT4 would regress these fields.
    let slot = database.start(&db).await?;
    let checked = async {
        let tx = database.tx(slot)?;
        let wide = i64::from(i32::MAX) + 1;
        let query = Query::update()
            .table(Alias::new("tenancy_installation"))
            .values([
                (Alias::new("authority_revision"), wide.into()),
                (Alias::new("operation_ids"), 7_i32.into()),
                (Alias::new("safety_operation_ids"), 3_i32.into()),
                (Alias::new("saturated"), true.into()),
            ])
            .and_where(Expr::col(Alias::new("singleton")).eq(1_i32))
            .to_owned();
        ensure!(
            tx.execute(tx.get_database_backend().build(&query))
                .await?
                .rows_affected()
                == 1
        );
        let loaded = startup::read_header_in(tx, 4096)
            .await?
            .context("header missing")?;
        ensure!(
            loaded.format == startup::Format::V4
                && loaded.header.revision.get() == u64::try_from(wide)?
                && loaded.header.operation_ids == 7
                && loaded.header.safety_operation_ids == 3
                && loaded.header.saturated
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    database.rollback(slot).await?;
    checked?;

    // Deliberate persisted version corruption remains a canonical refusal,
    // not a type error, capacity fallback, or silently accepted row.
    for (table, field, value, key, id, reason) in [
        (
            "tenancy_installation",
            "schema_version",
            0_i32,
            "installation_id",
            fixture.installation.as_str(),
            "unsupported authority store format",
        ),
        (
            "tenancy_operations",
            "hash_version",
            2_i32,
            "operation_id",
            "projection-config",
            "unsupported command hash version",
        ),
    ] {
        let slot = database.start(&db).await?;
        let checked = async {
            let tx = database.tx(slot)?;
            let query = Query::update()
                .table(Alias::new(table))
                .value(Alias::new(field), value)
                .and_where(Expr::col(Alias::new(key)).eq(id))
                .to_owned();
            ensure!(
                tx.execute(tx.get_database_backend().build(&query))
                    .await?
                    .rows_affected()
                    == 1
            );
            let error = store::load_existing(tx, Utc::now())
                .await
                .err()
                .context("invalid version accepted")?;
            ensure!(
                !error.is::<CapacityExceeded>() && format!("{error:#}").contains(reason),
                "wrong invalid-version refusal: {error:#}"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        database.rollback(slot).await?;
        checked?;
    }
    let restored = store::load_existing(&db, Utc::now()).await?;
    ensure!(
        restored.revision == expected.revision
            && restored.operation_ids == expected.operation_ids
            && restored.safety_operation_ids == 0
            && !restored.saturated
    );
    Ok(())
}
