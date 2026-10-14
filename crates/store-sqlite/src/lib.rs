//! 服务端消息持久化：SQLite + sea-query。
//!
//! 消息全部落盘，进程内不留副本——房间历史再长也不会把内存撑爆。
//! 房间标识与消息同库保存：库在，房间就在，重启后历史仍读得回来。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chat_core::{ChatError, MessageStore, NewMessage, Result};
use chat_proto::{Message, MessageKind, RoomId, Seq};
use sea_query::{
    ColumnDef, Expr, Index, Order, Query, SqliteQueryBuilder, Table, Value, Values,
};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use tokio::sync::Mutex;

/// `messages` 表。
mod messages {
    pub const TABLE: &str = "messages";
    pub const ROOM_ID: &str = "room_id";
    pub const SEQ: &str = "seq";
    pub const MSG_ID: &str = "msg_id";
    pub const SENDER: &str = "sender";
    pub const SENDER_NAME: &str = "sender_name";
    pub const KIND: &str = "kind";
    pub const BODY: &str = "body";
    pub const SERVER_TS: &str = "server_ts";
}

/// `room_meta` 表：单行，记着本库属于哪个房间。
mod room_meta {
    pub const TABLE: &str = "room_meta";
    pub const ID: &str = "id";
    pub const ROOM_ID: &str = "room_id";
}

/// 哨兵行的主键值，`room_meta` 恒只有这一行。
const ROOM_ROW: i32 = 1;

/// 读消息时统一选出的列，顺序与 [`row_to_message`] 的下标一一对应。
const MESSAGE_COLUMNS: [&str; 7] = [
    messages::SEQ,
    messages::MSG_ID,
    messages::SENDER,
    messages::SENDER_NAME,
    messages::KIND,
    messages::BODY,
    messages::SERVER_TS,
];

pub struct SqliteStore {
    pool: SqlitePool,
    room: RoomId,
    /// 串行化「取最大序号 + 插入」，否则两条连接会抢到同一个 seq。
    write: Mutex<()>,
}

impl SqliteStore {
    /// 打开（或新建）库文件，并把房间标识固定下来：库里已有就复用，没有才生成。
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ChatError::Storage(format!("创建数据目录失败: {e}")))?;
        }

        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .map_err(db_err)?;

        let store = Self {
            pool,
            room: RoomId::new(),
            write: Mutex::new(()),
        };
        store.migrate().await?;
        let room = store.load_or_init_room().await?;
        Ok(Self { room, ..store })
    }

    /// 本库对应的房间标识。
    pub fn room(&self) -> RoomId {
        self.room
    }

    async fn migrate(&self) -> Result<()> {
        let messages_table = Table::create()
            .table(messages::TABLE)
            .if_not_exists()
            .col(ColumnDef::new(messages::ROOM_ID).text().not_null())
            .col(ColumnDef::new(messages::SEQ).big_integer().not_null())
            .col(ColumnDef::new(messages::MSG_ID).text().not_null())
            .col(ColumnDef::new(messages::SENDER).text().not_null())
            .col(ColumnDef::new(messages::SENDER_NAME).text().not_null())
            .col(ColumnDef::new(messages::KIND).text().not_null())
            .col(ColumnDef::new(messages::BODY).text().not_null())
            .col(ColumnDef::new(messages::SERVER_TS).big_integer().not_null())
            .primary_key(Index::create().col(messages::ROOM_ID).col(messages::SEQ))
            .to_owned();

        let meta_table = Table::create()
            .table(room_meta::TABLE)
            .if_not_exists()
            .col(ColumnDef::new(room_meta::ID).integer().not_null().primary_key())
            .col(ColumnDef::new(room_meta::ROOM_ID).text().not_null())
            .to_owned();

        for statement in [messages_table, meta_table] {
            let sql = statement.build(SqliteQueryBuilder);
            sqlx::query(&sql).execute(&self.pool).await.map_err(db_err)?;
        }
        Ok(())
    }

    async fn load_or_init_room(&self) -> Result<RoomId> {
        let (sql, values) = Query::select()
            .column(room_meta::ROOM_ID)
            .from(room_meta::TABLE)
            .and_where(Expr::col(room_meta::ID).eq(ROOM_ROW))
            .build(SqliteQueryBuilder);

        let row = bind(sqlx::query(&sql), values)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;

        if let Some(row) = row {
            let text: String = row.get(0);
            let room = text
                .parse::<RoomId>()
                .map_err(|e| ChatError::Storage(format!("库中的房间标识无效: {e}")))?;
            return Ok(room);
        }

        let room = RoomId::new();
        let (sql, values) = Query::insert()
            .into_table(room_meta::TABLE)
            .columns([room_meta::ID, room_meta::ROOM_ID])
            .values_panic([ROOM_ROW.into(), room.to_string().into()])
            .build(SqliteQueryBuilder);

        bind(sqlx::query(&sql), values)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(room)
    }
}

#[async_trait]
impl MessageStore for SqliteStore {
    async fn append(&self, room: RoomId, msg: NewMessage) -> Result<Message> {
        let _guard = self.write.lock().await;

        let stored = Message {
            room,
            seq: self.latest_seq(room).await? + 1,
            client_msg_id: msg.client_msg_id,
            sender: msg.sender,
            sender_name: msg.sender_name,
            kind: msg.kind,
            text: msg.text,
            server_ts_ms: now_ms(),
        };

        let (sql, values) = Query::insert()
            .into_table(messages::TABLE)
            .columns([
                messages::ROOM_ID,
                messages::SEQ,
                messages::MSG_ID,
                messages::SENDER,
                messages::SENDER_NAME,
                messages::KIND,
                messages::BODY,
                messages::SERVER_TS,
            ])
            .values_panic([
                stored.room.to_string().into(),
                stored.seq.into(),
                stored.client_msg_id.to_string().into(),
                stored.sender.to_string().into(),
                stored.sender_name.clone().into(),
                kind_str(stored.kind).into(),
                stored.text.clone().into(),
                stored.server_ts_ms.into(),
            ])
            .build(SqliteQueryBuilder);

        bind(sqlx::query(&sql), values)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(stored)
    }

    async fn history(&self, room: RoomId, before: Option<Seq>, limit: usize) -> Result<Vec<Message>> {
        // `SelectStatement` 不是 `Send`，必须在 await 之前就生成好 SQL 并丢掉它
        let (sql, values) = {
            let mut query = Query::select();
            query
                .columns(MESSAGE_COLUMNS)
                .from(messages::TABLE)
                .and_where(Expr::col(messages::ROOM_ID).eq(room.to_string()))
                .order_by(messages::SEQ, Order::Desc)
                .limit(limit as u64);
            if let Some(before) = before {
                query.and_where(Expr::col(messages::SEQ).lt(before));
            }
            query.build(SqliteQueryBuilder)
        };
        let rows = bind(sqlx::query(&sql), values)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;

        // 上面按倒序取最近 limit 条，这里翻回升序
        let mut messages = rows
            .iter()
            .map(|row| row_to_message(room, row))
            .collect::<Result<Vec<_>>>()?;
        messages.reverse();
        Ok(messages)
    }

    async fn since(&self, room: RoomId, after: Seq, limit: usize) -> Result<Vec<Message>> {
        let (sql, values) = Query::select()
            .columns(MESSAGE_COLUMNS)
            .from(messages::TABLE)
            .and_where(Expr::col(messages::ROOM_ID).eq(room.to_string()))
            .and_where(Expr::col(messages::SEQ).gt(after))
            .order_by(messages::SEQ, Order::Asc)
            .limit(limit as u64)
            .build(SqliteQueryBuilder);

        let rows = bind(sqlx::query(&sql), values)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;

        rows.iter().map(|row| row_to_message(room, row)).collect()
    }

    async fn latest_seq(&self, room: RoomId) -> Result<Seq> {
        let (sql, values) = Query::select()
            .expr(Expr::col(messages::SEQ).max())
            .from(messages::TABLE)
            .and_where(Expr::col(messages::ROOM_ID).eq(room.to_string()))
            .build(SqliteQueryBuilder);

        let row = bind(sqlx::query(&sql), values)
            .fetch_one(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(row.get::<Option<i64>, _>(0).unwrap_or(0))
    }
}

fn row_to_message(room: RoomId, row: &sqlx::sqlite::SqliteRow) -> Result<Message> {
    let msg_id: String = row.get(1);
    let sender: String = row.get(2);
    Ok(Message {
        room,
        seq: row.get(0),
        client_msg_id: msg_id
            .parse()
            .map_err(|e| ChatError::Storage(format!("消息标识无效: {e}")))?,
        sender: sender
            .parse()
            .map_err(|e| ChatError::Storage(format!("发送者无效: {e}")))?,
        sender_name: row.get(3),
        kind: kind_from_str(&row.get::<String, _>(4)),
        text: row.get(5),
        server_ts_ms: row.get(6),
    })
}

fn kind_str(kind: MessageKind) -> &'static str {
    match kind {
        MessageKind::Text => "text",
        MessageKind::System => "system",
    }
}

fn kind_from_str(text: &str) -> MessageKind {
    match text {
        "system" => MessageKind::System,
        _ => MessageKind::Text,
    }
}

fn db_err(error: sqlx::Error) -> ChatError {
    ChatError::Storage(error.to_string())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 把 sea-query 生成的值按顺序绑到 sqlx 语句上。
fn bind<'a>(
    mut query: sqlx::query::Query<'a, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'a>>,
    values: Values,
) -> sqlx::query::Query<'a, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'a>> {
    for value in values.0 {
        query = match value {
            Value::String(Some(text)) => query.bind(*text),
            Value::BigInt(Some(int)) => query.bind(int),
            Value::Int(Some(int)) => query.bind(i64::from(int)),
            Value::Unsigned(Some(int)) => query.bind(i64::from(int)),
            Value::BigUnsigned(Some(int)) => query.bind(int as i64),
            _ => query.bind(Option::<String>::None),
        };
    }
    query
}

#[cfg(test)]
mod tests {
    use chat_proto::UserId;
    use uuid::Uuid;

    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("libchat-store-{tag}-{}.db", std::process::id()))
    }

    fn incoming(text: &str) -> NewMessage {
        NewMessage {
            client_msg_id: Uuid::new_v4(),
            sender: UserId::new(),
            sender_name: "Alice".into(),
            kind: MessageKind::Text,
            text: text.into(),
        }
    }

    #[tokio::test]
    async fn appends_in_order_and_pages_both_ways() {
        let path = temp_path("pages");
        let _ = std::fs::remove_file(&path);
        let store = SqliteStore::open(&path).await.unwrap();
        let room = store.room();

        for i in 1..=5 {
            let stored = store.append(room, incoming(&format!("m{i}"))).await.unwrap();
            assert_eq!(stored.seq, i);
        }
        assert_eq!(store.latest_seq(room).await.unwrap(), 5);

        let recent = store.history(room, None, 2).await.unwrap();
        assert_eq!(texts(&recent), ["m4", "m5"]);

        let after = store.since(room, 3, 10).await.unwrap();
        assert_eq!(texts(&after), ["m4", "m5"]);

        // 空房间不该报错
        let other = RoomId::new();
        assert!(store.history(other, None, 10).await.unwrap().is_empty());
        assert_eq!(store.latest_seq(other).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn reopens_same_room_with_history_intact() {
        let path = temp_path("reopen");
        let _ = std::fs::remove_file(&path);

        let room = {
            let store = SqliteStore::open(&path).await.unwrap();
            let room = store.room();
            store.append(room, incoming("说过的话")).await.unwrap();
            room
        };

        // 重新打开：房间标识与消息都应从库里恢复
        let reopened = SqliteStore::open(&path).await.unwrap();
        assert_eq!(reopened.room(), room);
        assert_eq!(texts(&reopened.history(room, None, 10).await.unwrap()), ["说过的话"]);
    }

    fn texts(messages: &[Message]) -> Vec<&str> {
        messages.iter().map(|m| m.text.as_str()).collect()
    }
}
