//! Bounded PostgreSQL transactions for concrete gateway authority records.
//!
//! Callers own the lock order and may not perform provider, refresh or delivery
//! I/O inside a transaction. The database's clock owns all expiry decisions.

use std::future::Future;
use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseBackend, DatabaseConnection, DatabaseTransaction,
    EntityTrait, IsolationLevel, QueryFilter, QueryOrder, QuerySelect, Set, Statement,
    TransactionTrait, TryInsertResult,
};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tracing::instrument::WithSubscriber;

pub(super) const MAX_RECORD_BYTES: usize = 2 * 1024 * 1024;
const MAX_SCAN_ROWS: u64 = 256;

mod entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "gateway_shared_records")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub namespace: String,
        pub kind: String,
        pub key: String,
        pub revision: i64,
        pub status: String,
        pub due_at_ms: Option<i64>,
        pub updated_at_ms: i64,
        pub body: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// Category names and record identities are internal, not client supplied SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Authority,
    Tenant,
    Principal,
    Resource,
    Execution,
    Attempt,
    Idempotency,
    Instance,
    Outbox,
    Credential,
    EpisodeFence,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Authority => "authority",
            Self::Tenant => "tenant",
            Self::Principal => "principal",
            Self::Resource => "resource",
            Self::Execution => "execution",
            Self::Attempt => "attempt",
            Self::Idempotency => "idempotency",
            Self::Instance => "instance",
            Self::Outbox => "outbox",
            Self::Credential => "credential",
            Self::EpisodeFence => "episode_fence",
        }
    }
}

/// Loaded row metadata cannot be forged outside this persistence module.
pub(super) struct Stored<T> {
    row: entity::Model,
    pub value: T,
}

impl<T> Stored<T> {
    pub fn revision(&self) -> Result<u64> {
        u64::try_from(self.row.revision).context("negative shared authority revision")
    }

    pub fn key(&self) -> &str {
        &self.row.key
    }

    pub fn due_at_ms(&self) -> Option<i64> {
        self.row.due_at_ms
    }
}

#[derive(Clone)]
pub(super) struct Store {
    db: DatabaseConnection,
    namespace: String,
    transaction_timeout: Duration,
}

impl Store {
    pub fn new(
        db: DatabaseConnection,
        namespace: &str,
        transaction_timeout: Duration,
    ) -> Result<Self> {
        ensure!(
            db.get_database_backend() == DatabaseBackend::Postgres,
            "shared gateway authority requires PostgreSQL"
        );
        ensure!(
            !namespace.is_empty()
                && namespace.len() <= 64
                && namespace
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte)),
            "shared gateway namespace must contain 1..64 ASCII letters, digits, hyphens or underscores"
        );
        ensure!(
            (Duration::from_millis(100)..=Duration::from_secs(30)).contains(&transaction_timeout),
            "shared transaction timeout must be between 100ms and 30s"
        );
        Ok(Self {
            db,
            namespace: namespace.into(),
            transaction_timeout,
        })
    }

    pub fn database(&self) -> &DatabaseConnection {
        &self.db
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub async fn bounded<T>(&self, operation: impl Future<Output = Result<T>>) -> Result<T> {
        tokio::time::timeout(self.transaction_timeout, operation)
            .await
            .context("shared gateway transaction deadline exceeded")?
    }

    /// Retry only database-proven transaction aborts, never ambiguous commits.
    /// The callback must contain one complete local transaction and no remote I/O.
    pub async fn retry<T, F, Fut>(&self, mut operation: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        self.bounded(async {
            for attempt in 0..4_u64 {
                match operation().await {
                    Err(error) if attempt < 3 && retryable_abort(&error) => {
                        tokio::time::sleep(Duration::from_millis(5 * (attempt + 1))).await;
                    }
                    result => return result,
                }
            }
            anyhow::bail!("shared transaction retry budget exhausted")
        })
        .await
    }

    pub async fn begin(&self) -> Result<DatabaseTransaction> {
        self.begin_with_isolation(IsolationLevel::Serializable)
            .await
    }

    pub async fn begin_with_isolation(
        &self,
        isolation: IsolationLevel,
    ) -> Result<DatabaseTransaction> {
        let tx = self
            .db
            .begin_with_config(Some(isolation), None)
            .await
            .context("beginning shared gateway transaction")?;
        let timeout = format!("{}ms", self.transaction_timeout.as_millis());
        // SET LOCAL is confined to this transaction even on pooled connections.
        // Server-side bounds also release locks if the owning task disappears.
        // https://www.postgresql.org/docs/current/runtime-config-client.html
        tx.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT set_config('statement_timeout', $1, true), \
                    set_config('lock_timeout', $1, true), \
                    set_config('idle_in_transaction_session_timeout', $1, true)",
            [timeout.into()],
        ))
        .await
        .context("bounding shared gateway transaction")?;
        Ok(tx)
    }

    pub async fn now(connection: &impl ConnectionTrait) -> Result<i64> {
        // clock_timestamp observes actual database time, unlike transaction-start
        // CURRENT_TIMESTAMP. Replica wall clocks never determine lease expiry.
        // https://www.postgresql.org/docs/current/functions-datetime.html#FUNCTIONS-DATETIME-CURRENT
        connection
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::bigint AS now_ms",
            ))
            .await?
            .context("shared database clock returned no row")?
            .try_get("", "now_ms")
            .context("reading shared database clock")
    }

    pub async fn read<T: DeserializeOwned>(
        &self,
        connection: &DatabaseTransaction,
        kind: Kind,
        key: &str,
    ) -> Result<Option<Stored<T>>> {
        let row = entity::Entity::find_by_id(self.id(kind, key)?)
            .one(connection)
            .await?;
        row.map(|row| self.decode(kind, key, row)).transpose()
    }

    /// A pooled read still needs a server deadline: dropping a client future
    /// alone does not cancel a PostgreSQL query or return its connection.
    pub async fn get<T: DeserializeOwned>(
        &self,
        kind: Kind,
        key: &str,
    ) -> Result<Option<Stored<T>>> {
        self.bounded(async {
            let tx = self.begin().await?;
            let row = self.read(&tx, kind, key).await?;
            tx.commit().await?;
            Ok(row)
        })
        .await
    }

    pub async fn lock<T: DeserializeOwned>(
        &self,
        tx: &DatabaseTransaction,
        kind: Kind,
        key: &str,
    ) -> Result<Stored<T>> {
        // Independent processes serialize only on the affected authority row.
        // https://www.postgresql.org/docs/current/explicit-locking.html#LOCKING-ROWS
        let row = entity::Entity::find_by_id(self.id(kind, key)?)
            .lock_exclusive()
            .one(tx)
            .await?
            .context("shared gateway authority record is missing")?;
        self.decode(kind, key, row)
    }

    /// Concurrent readers fence publication without serializing one another.
    pub async fn lock_shared<T: DeserializeOwned>(
        &self,
        tx: &DatabaseTransaction,
        kind: Kind,
        key: &str,
    ) -> Result<Stored<T>> {
        let row = entity::Entity::find_by_id(self.id(kind, key)?)
            .lock_shared()
            .one(tx)
            .await?
            .context("shared gateway authority record is missing")?;
        self.decode(kind, key, row)
    }

    pub async fn create<T: Serialize>(
        &self,
        tx: &DatabaseTransaction,
        write: NewRecord<'_, T>,
    ) -> Result<bool> {
        let inserted = entity::Entity::insert(entity::ActiveModel {
            id: Set(self.id(write.kind, write.key)?),
            namespace: Set(self.namespace.clone()),
            kind: Set(write.kind.as_str().into()),
            key: Set(write.key.into()),
            revision: Set(0),
            status: Set(validate_status(write.status)?.into()),
            due_at_ms: Set(write.due_at_ms),
            updated_at_ms: Set(write.now_ms),
            body: Set(encode(write.value)?),
        })
        .on_conflict(
            OnConflict::column(entity::Column::Id)
                .do_nothing()
                .to_owned(),
        )
        .do_nothing()
        .exec(tx)
        .await?;
        match inserted {
            TryInsertResult::Inserted(_) => Ok(true),
            TryInsertResult::Conflicted => Ok(false),
            TryInsertResult::Empty => anyhow::bail!("shared gateway insert had no record"),
        }
    }

    pub async fn save<T: Serialize>(
        &self,
        tx: &DatabaseTransaction,
        stored: &mut Stored<T>,
        status: &str,
        due_at_ms: Option<i64>,
        now_ms: i64,
    ) -> Result<()> {
        ensure!(
            stored.row.namespace == self.namespace,
            "shared record namespace mismatch"
        );
        let revision = stored
            .row
            .revision
            .checked_add(1)
            .context("shared gateway revision exhausted")?;
        let body = encode(&stored.value)?;
        let result = entity::Entity::update_many()
            .col_expr(entity::Column::Revision, Expr::value(revision))
            .col_expr(entity::Column::Body, Expr::value(body.clone()))
            .col_expr(
                entity::Column::Status,
                Expr::value(validate_status(status)?),
            )
            .col_expr(entity::Column::DueAtMs, Expr::value(due_at_ms))
            .col_expr(entity::Column::UpdatedAtMs, Expr::value(now_ms))
            .filter(entity::Column::Id.eq(&stored.row.id))
            .filter(entity::Column::Namespace.eq(&self.namespace))
            .filter(entity::Column::Revision.eq(stored.row.revision))
            .exec(tx)
            .await?;
        ensure!(
            result.rows_affected == 1,
            "shared authority revision changed"
        );
        stored.row.revision = revision;
        stored.row.status = status.into();
        stored.row.due_at_ms = due_at_ms;
        stored.row.updated_at_ms = now_ms;
        stored.row.body = body;
        Ok(())
    }

    pub async fn due<T: DeserializeOwned>(
        &self,
        kind: Kind,
        status: &str,
        now_ms: i64,
        limit: u64,
    ) -> Result<Vec<Stored<T>>> {
        self.due_states(kind, &[status], now_ms, limit).await
    }

    pub async fn due_states<T: DeserializeOwned>(
        &self,
        kind: Kind,
        statuses: &[&str],
        now_ms: i64,
        limit: u64,
    ) -> Result<Vec<Stored<T>>> {
        ensure!(
            !statuses.is_empty() && statuses.len() <= 8,
            "invalid shared scan status count"
        );
        for status in statuses {
            validate_status(status)?;
        }
        ensure!(
            (1..=MAX_SCAN_ROWS).contains(&limit),
            "shared scan limit must be 1..256"
        );
        self.bounded(async {
            let tx = self.begin().await?;
            let rows = entity::Entity::find()
                .filter(entity::Column::Namespace.eq(&self.namespace))
                .filter(entity::Column::Kind.eq(kind.as_str()))
                .filter(entity::Column::Status.is_in(statuses.iter().copied()))
                .filter(entity::Column::DueAtMs.lte(now_ms))
                .order_by_asc(entity::Column::DueAtMs)
                .order_by_asc(entity::Column::Id)
                .limit(limit)
                .all(&tx)
                .await?;
            let records = rows
                .into_iter()
                .map(|row| self.decode(kind, &row.key.clone(), row))
                .collect::<Result<Vec<_>>>()?;
            tx.commit().await?;
            Ok(records)
        })
        .await
    }

    fn decode<T: DeserializeOwned>(
        &self,
        kind: Kind,
        key: &str,
        row: entity::Model,
    ) -> Result<Stored<T>> {
        ensure!(
            row.namespace == self.namespace
                && row.kind == kind.as_str()
                && row.key == key
                && row.id == self.id(kind, key)?,
            "shared record identity mismatch"
        );
        ensure!(row.revision >= 0, "negative shared authority revision");
        ensure!(
            row.body.len() <= MAX_RECORD_BYTES,
            "shared record exceeds byte limit"
        );
        let value = serde_json::from_str(&row.body).context("decoding shared gateway record")?;
        Ok(Stored { row, value })
    }

    fn id(&self, kind: Kind, key: &str) -> Result<String> {
        ensure!(
            !key.is_empty() && key.len() <= 256,
            "shared record key must contain 1..256 bytes"
        );
        Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
            "gateway-shared-record-v1",
            &self.namespace,
            kind.as_str(),
            key,
        ))?)))
    }
}

/// Bounded shared pools do not inherit permissive local connection defaults.
pub(super) async fn connect(
    url: &str,
    pool_limit: u32,
    timeout: Duration,
) -> Result<DatabaseConnection> {
    let parsed =
        url::Url::parse(url).map_err(|_| anyhow::anyhow!("invalid shared database URL"))?;
    ensure!(
        matches!(parsed.scheme(), "postgres" | "postgresql"),
        "shared gateway requires a PostgreSQL URL"
    );
    ensure!(
        !parsed.path().trim_matches('/').is_empty(),
        "shared database URL must name a database"
    );
    ensure!(
        (1..=128).contains(&pool_limit),
        "shared database pool limit must be 1..128"
    );
    ensure!(
        (Duration::from_millis(100)..=Duration::from_secs(30)).contains(&timeout),
        "invalid shared database timeout"
    );
    let mut options = sea_orm::ConnectOptions::new(url);
    options
        .min_connections(0)
        .max_connections(pool_limit)
        .connect_timeout(timeout)
        .acquire_timeout(timeout)
        .idle_timeout(Duration::from_secs(60))
        .max_lifetime(Duration::from_secs(300))
        .sqlx_logging(false);
    // Connector diagnostics can include the URL or credentials. The service
    // boundary exposes a stable classification instead of forwarding them.
    // SeaORM 1.1 instruments its complete ConnectOptions at trace level. Its
    // Debug includes the credential-bearing URL, even with SQL logging disabled.
    // Limit suppression to polling this construction future; callers still get
    // the stable error classification. It never changes the global subscriber.
    // https://docs.rs/sea-orm/1.1.20/src/sea_orm/driver/sqlx_postgres.rs.html
    let connection = sea_orm::Database::connect(options)
        .with_subscriber(tracing::subscriber::NoSubscriber::default());
    tokio::time::timeout(timeout, connection)
        .await
        .map_err(|_| anyhow::anyhow!("shared database connection deadline exceeded"))?
        .map_err(|_| anyhow::anyhow!("shared database connection failed"))
}

pub(super) struct NewRecord<'a, T> {
    pub kind: Kind,
    pub key: &'a str,
    pub status: &'a str,
    pub due_at_ms: Option<i64>,
    pub now_ms: i64,
    pub value: &'a T,
}

fn encode(value: &impl Serialize) -> Result<String> {
    let mut writer = BoundedWriter(Vec::new());
    serde_json::to_writer(&mut writer, value).context("encoding bounded shared record")?;
    String::from_utf8(writer.0).context("shared record is not UTF-8")
}

struct BoundedWriter(Vec<u8>);

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_RECORD_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("shared record exceeds byte limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn retryable_abort(error: &anyhow::Error) -> bool {
    use sea_orm::{DbErr, RuntimeErr, SqlxError};
    error.chain().any(|cause| {
        let Some(
            DbErr::Exec(RuntimeErr::SqlxError(SqlxError::Database(database)))
            | DbErr::Query(RuntimeErr::SqlxError(SqlxError::Database(database))),
        ) = cause.downcast_ref::<DbErr>()
        else {
            return false;
        };
        // PostgreSQL guarantees these transactions were aborted. Connection loss,
        // timeouts and other errors cannot prove whether COMMIT took effect.
        // https://www.postgresql.org/docs/current/mvcc-serialization-failure-handling.html
        matches!(database.code().as_deref(), Some("40001" | "40P01"))
    })
}

fn validate_status(status: &str) -> Result<&str> {
    ensure!(
        !status.is_empty()
            && status.len() <= 32
            && status
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_'),
        "invalid shared record status"
    );
    Ok(status)
}
