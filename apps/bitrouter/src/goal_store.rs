//! App-owned transactional Goal heads and immutable admission receipts.

use bitrouter_orchestrator::goal::{
    GoalError, GoalOperation, GoalPage, GoalSnapshot, GoalStore, HistoryPage, MAX_RECORD_BYTES,
    identifier, page_bounds, replay, storage_error,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction, EntityTrait,
    QueryFilter, QueryOrder, QuerySelect, Set, TransactionTrait, sea_query::Expr,
};

pub struct DatabaseGoalStore {
    db: DatabaseConnection,
}

impl DatabaseGoalStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

mod heads {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "bro_goals")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub position: i64,
        pub id: String,
        pub owner: String,
        pub revision: i64,
        pub payload: Vec<u8>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

mod operations {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "bro_goal_operations")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub owner: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub goal_id: String,
        pub revision: i64,
        pub payload: Vec<u8>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

fn signed(value: u64) -> Result<i64, GoalError> {
    i64::try_from(value).map_err(storage_error)
}

fn unsigned(value: i64) -> Result<u64, GoalError> {
    u64::try_from(value).map_err(storage_error)
}

fn encode(value: &impl serde::Serialize) -> Result<Vec<u8>, GoalError> {
    let bytes = serde_json::to_vec(value).map_err(storage_error)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(GoalError::Invalid {
            message: "Goal record exceeds byte bound".into(),
        });
    }
    Ok(bytes)
}

fn snapshot(row: heads::Model) -> Result<GoalSnapshot, GoalError> {
    if row.payload.len() > MAX_RECORD_BYTES {
        return Err(storage_error("oversized Goal head"));
    }
    let goal: GoalSnapshot = serde_json::from_slice(&row.payload).map_err(storage_error)?;
    if goal.id != row.id || goal.owner != row.owner || signed(goal.revision)? != row.revision {
        return Err(storage_error("Goal head identity or revision mismatch"));
    }
    Ok(goal)
}

fn receipt(row: operations::Model) -> Result<GoalOperation, GoalError> {
    if row.payload.len() > MAX_RECORD_BYTES {
        return Err(storage_error("oversized Goal operation"));
    }
    let operation: GoalOperation = serde_json::from_slice(&row.payload).map_err(storage_error)?;
    if operation.id != row.id
        || operation.goal_id != row.goal_id
        || signed(operation.revision)? != row.revision
    {
        return Err(storage_error(
            "Goal operation identity or revision mismatch",
        ));
    }
    Ok(operation)
}

async fn find_operation(
    db: &impl ConnectionTrait,
    owner: &str,
    id: &str,
) -> Result<Option<GoalOperation>, GoalError> {
    operations::Entity::find_by_id((owner.to_owned(), id.to_owned()))
        .one(db)
        .await
        .map_err(storage_error)?
        .map(receipt)
        .transpose()
}

#[async_trait::async_trait]
impl GoalStore for DatabaseGoalStore {
    async fn get(&self, owner: &str, id: &str) -> Result<GoalSnapshot, GoalError> {
        identifier(owner)?;
        identifier(id)?;
        let row = heads::Entity::find()
            .filter(heads::Column::Owner.eq(owner))
            .filter(heads::Column::Id.eq(id))
            .one(&self.db)
            .await
            .map_err(storage_error)?
            .ok_or(GoalError::NotFound)?;
        snapshot(row)
    }

    async fn operation(&self, owner: &str, id: &str) -> Result<Option<GoalOperation>, GoalError> {
        identifier(owner)?;
        identifier(id)?;
        find_operation(&self.db, owner, id).await
    }

    async fn list(
        &self,
        owner: &str,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
    ) -> Result<GoalPage, GoalError> {
        identifier(owner)?;
        page_bounds(after, cutoff, limit)?;
        let latest = heads::Entity::find()
            .filter(heads::Column::Owner.eq(owner))
            .order_by_desc(heads::Column::Position)
            .one(&self.db)
            .await
            .map_err(storage_error)?;
        let latest = match latest {
            Some(row) => unsigned(row.position)?,
            None => 0,
        };
        let cutoff = cutoff.unwrap_or(latest);
        if cutoff > latest {
            return Err(GoalError::Invalid {
                message: "directory cutoff is in the future".into(),
            });
        }
        let rows = heads::Entity::find()
            .filter(heads::Column::Owner.eq(owner))
            .filter(heads::Column::Position.gt(signed(after)?))
            .filter(heads::Column::Position.lte(signed(cutoff)?))
            .order_by_asc(heads::Column::Position)
            .limit((limit + 1) as u64)
            .all(&self.db)
            .await
            .map_err(storage_error)?;
        let mut goals = Vec::new();
        let mut bytes = 0;
        let mut cursor = after;
        let mut more = false;
        for row in rows {
            if goals.len() == limit || bytes + row.payload.len() > MAX_RECORD_BYTES {
                more = true;
                break;
            }
            bytes += row.payload.len();
            cursor = unsigned(row.position)?;
            goals.push(snapshot(row)?);
        }
        Ok(GoalPage {
            goals,
            cutoff,
            next_after: more.then_some(cursor),
        })
    }

    async fn history(
        &self,
        owner: &str,
        id: &str,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
    ) -> Result<HistoryPage, GoalError> {
        page_bounds(after, cutoff, limit)?;
        let head = self.get(owner, id).await?;
        let cutoff = cutoff.unwrap_or(head.revision);
        if cutoff > head.revision {
            return Err(GoalError::Invalid {
                message: "history cutoff is in the future".into(),
            });
        }
        let rows = operations::Entity::find()
            .filter(operations::Column::Owner.eq(owner))
            .filter(operations::Column::GoalId.eq(id))
            .filter(operations::Column::Revision.gt(signed(after)?))
            .filter(operations::Column::Revision.lte(signed(cutoff)?))
            .order_by_asc(operations::Column::Revision)
            .limit((limit + 1) as u64)
            .all(&self.db)
            .await
            .map_err(storage_error)?;
        let mut operations = Vec::new();
        let mut bytes = 0;
        let mut cursor = after;
        let mut more = false;
        for row in rows {
            if operations.len() == limit || bytes + row.payload.len() > MAX_RECORD_BYTES {
                more = true;
                break;
            }
            bytes += row.payload.len();
            cursor = unsigned(row.revision)?;
            operations.push(receipt(row)?);
        }
        Ok(HistoryPage {
            operations,
            cutoff,
            next_after: more.then_some(cursor),
        })
    }

    async fn commit(
        &self,
        expected_revision: Option<u64>,
        goal: GoalSnapshot,
        operation: GoalOperation,
    ) -> Result<GoalOperation, GoalError> {
        identifier(&goal.owner)?;
        identifier(&goal.id)?;
        identifier(&operation.id)?;
        if operation.goal_id != goal.id
            || operation.revision != goal.revision
            || operation.requirements_revision != goal.requirements_revision
            || expected_revision.map_or(goal.revision != 1, |revision| {
                revision.checked_add(1) != Some(goal.revision)
            })
        {
            return Err(GoalError::Invalid {
                message: "inconsistent Goal commit".into(),
            });
        }
        let tx = self.db.begin().await.map_err(storage_error)?;
        let result = write_commit(&tx, expected_revision, &goal, &operation).await;
        let failure = match result {
            Ok(original) => match tx.commit().await {
                Ok(()) => return Ok(original),
                Err(error) => storage_error(error),
            },
            Err(error) => {
                tx.rollback().await.map_err(storage_error)?;
                error
            }
        };
        // A competing admission or uncertain commit may have persisted the
        // original operation. Adopt it, never create another identity.
        match self.operation(&goal.owner, &operation.id).await? {
            Some(original) => replay(original, &operation.fingerprint),
            None => Err(failure),
        }
    }
}

async fn write_commit(
    tx: &DatabaseTransaction,
    expected_revision: Option<u64>,
    goal: &GoalSnapshot,
    operation: &GoalOperation,
) -> Result<GoalOperation, GoalError> {
    if let Some(original) = find_operation(tx, &goal.owner, &operation.id).await? {
        return replay(original, &operation.fingerprint);
    }
    let payload = encode(goal)?;
    if let Some(expected) = expected_revision {
        let update = heads::Entity::update_many()
            .col_expr(heads::Column::Payload, Expr::value(payload))
            .col_expr(heads::Column::Revision, Expr::value(signed(goal.revision)?))
            .filter(heads::Column::Id.eq(&goal.id))
            .filter(heads::Column::Owner.eq(&goal.owner))
            .filter(heads::Column::Revision.eq(signed(expected)?))
            .exec(tx)
            .await
            .map_err(storage_error)?;
        if update.rows_affected != 1 {
            return Err(GoalError::Conflict);
        }
    } else {
        heads::Entity::insert(heads::ActiveModel {
            id: Set(goal.id.clone()),
            owner: Set(goal.owner.clone()),
            revision: Set(signed(goal.revision)?),
            payload: Set(payload),
            ..Default::default()
        })
        .exec(tx)
        .await
        .map_err(storage_error)?;
    }
    operations::Entity::insert(operations::ActiveModel {
        owner: Set(goal.owner.clone()),
        id: Set(operation.id.clone()),
        goal_id: Set(operation.goal_id.clone()),
        revision: Set(signed(operation.revision)?),
        payload: Set(encode(operation)?),
    })
    .exec(tx)
    .await
    .map_err(storage_error)?;
    Ok(operation.clone())
}
