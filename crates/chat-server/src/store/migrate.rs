//! 库结构迁移。
//!
//! 版本号存在 SQLite 的 `PRAGMA user_version` 里，只往前推不回头：
//! 每一步单独一个事务，建表与写版本号要么一起生效，要么一起回滚。
//! 之前那套 `if_not_exists` + 忽略 `ALTER TABLE` 报错的做法只适合空库，
//! 一旦库里有真实数据就没法判断该补哪一步。

use chat_core::{ChatError, Result};
use sea_query::{
    ForeignKeyStatement, IndexCreateStatement, IndexStatement, SchemaStatement, SqliteQueryBuilder,
    TableCreateStatement, TableStatement,
};
use sqlx::SqlitePool;

/// 建表语句。
pub fn table(statement: TableCreateStatement) -> SchemaStatement {
    SchemaStatement::TableStatement(TableStatement::Create(statement))
}

/// 建索引语句。
pub fn index(statement: IndexCreateStatement) -> SchemaStatement {
    SchemaStatement::IndexStatement(IndexStatement::Create(statement))
}

/// 一步迁移。`schema` 用 sea-query 构造，不手写 SQL。
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub schema: fn() -> Vec<SchemaStatement>,
}

/// 把库升到最新版本；已经是最新就什么都不做。
pub async fn apply(pool: &SqlitePool, migrations: &[Migration]) -> Result<()> {
    let current: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await
        .map_err(storage)?;

    for migration in migrations.iter().filter(|m| m.version > current) {
        let mut tx = pool.begin().await.map_err(storage)?;
        for statement in (migration.schema)() {
            sqlx::query(&build(&statement))
                .execute(&mut *tx)
                .await
                .map_err(|e| ChatError::Storage(format!("迁移 {} 失败: {e}", migration.name)))?;
        }
        // PRAGMA 不支持参数绑定，版本号是编译期常量
        sqlx::query(&format!("PRAGMA user_version = {}", migration.version))
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
    }
    Ok(())
}

/// 当前库结构版本，测试与诊断用。
#[cfg(test)]
pub async fn version(pool: &SqlitePool) -> Result<i64> {
    sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await
        .map_err(storage)
}

fn build(statement: &SchemaStatement) -> String {
    match statement {
        SchemaStatement::TableStatement(statement) => match statement {
            TableStatement::Create(statement) => statement.build(SqliteQueryBuilder),
            TableStatement::Alter(statement) => statement.build(SqliteQueryBuilder),
            TableStatement::Drop(statement) => statement.build(SqliteQueryBuilder),
            TableStatement::Rename(statement) => statement.build(SqliteQueryBuilder),
            TableStatement::Truncate(statement) => statement.build(SqliteQueryBuilder),
        },
        SchemaStatement::IndexStatement(statement) => match statement {
            IndexStatement::Create(statement) => statement.build(SqliteQueryBuilder),
            IndexStatement::Drop(statement) => statement.build(SqliteQueryBuilder),
        },
        SchemaStatement::ForeignKeyStatement(statement) => match statement {
            ForeignKeyStatement::Create(statement) => statement.build(SqliteQueryBuilder),
            ForeignKeyStatement::Drop(statement) => statement.build(SqliteQueryBuilder),
        },
    }
}

fn storage(error: sqlx::Error) -> ChatError {
    ChatError::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_query::{Alias, ColumnDef, Table};
    use sqlx::sqlite::SqlitePoolOptions;

    fn create(name: &str) -> Vec<SchemaStatement> {
        vec![table(
            Table::create()
                .table(Alias::new(name))
                .if_not_exists()
                .col(ColumnDef::new("id").integer().not_null().primary_key())
                .to_owned(),
        )]
    }

    fn v1() -> Vec<SchemaStatement> {
        create("first")
    }

    fn v2() -> Vec<SchemaStatement> {
        create("second")
    }

    #[tokio::test]
    async fn applies_each_step_once_and_records_the_version() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let migrations = [
            Migration {
                version: 1,
                name: "第一步",
                schema: v1,
            },
            Migration {
                version: 2,
                name: "第二步",
                schema: v2,
            },
        ];

        apply(&pool, &migrations).await.unwrap();
        assert_eq!(version(&pool).await.unwrap(), 2);

        // 重复启动不应重跑已经完成的步骤
        apply(&pool, &migrations).await.unwrap();
        assert_eq!(version(&pool).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn a_failing_step_rolls_back_without_bumping_the_version() {
        // 同一张表连建两次，且都不带 if_not_exists：第二句必然报错
        fn broken() -> Vec<SchemaStatement> {
            let statement = || {
                table(
                    Table::create()
                        .table(Alias::new("third"))
                        .col(ColumnDef::new("id").integer().not_null().primary_key())
                        .to_owned(),
                )
            };
            vec![statement(), statement()]
        }

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let migrations = [Migration {
            version: 1,
            name: "会失败的一步",
            schema: broken,
        }];
        // 同一张表建两次：第二句必然报错，整步回滚
        assert!(apply(&pool, &migrations).await.is_err());
        assert_eq!(version(&pool).await.unwrap(), 0);
    }
}
