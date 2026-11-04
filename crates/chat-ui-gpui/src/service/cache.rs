//! 客户端消息缓存：重启或断网时先把上次的聊天记录摆出来，再增量补齐。
//!
//! 只缓存每个房间最近 [`KEEP_PER_ROOM`] 条，库很小；
//! 服务端历史到了以后按 `seq` 去重合并，重复的不会出现两次。

use std::path::Path;
use std::sync::{Arc, OnceLock};

use chat_core::{Attachment, AttachmentId, Quote, RoomId, Seq, UserId};
use sea_query::{ColumnDef, Expr, OnConflict, Query, SqliteQueryBuilder, Table};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

use super::HistoryMessage;

/// 每个房间保留多少条。
const KEEP_PER_ROOM: i64 = 200;

mod messages {
    pub const TABLE: &str = "cached_messages";
    pub const ROOM: &str = "room";
    pub const SEQ: &str = "seq";
    pub const SENDER_ID: &str = "sender_id";
    pub const SENDER: &str = "sender";
    pub const AT: &str = "at";
    pub const TEXT: &str = "text";
    pub const RECALLED: &str = "recalled";
    pub const ATTACHMENT_ID: &str = "attachment_id";
    pub const ATTACHMENT_NAME: &str = "attachment_name";
    pub const ATTACHMENT_SIZE: &str = "attachment_size";
    pub const ATTACHMENT_MIME: &str = "attachment_mime";
    pub const ATTACHMENT_SHA256: &str = "attachment_sha256";
    pub const QUOTE_SENDER: &str = "quote_sender";
    pub const QUOTE_TEXT: &str = "quote_text";
    pub const QUOTE_SEQ: &str = "quote_seq";
}

mod rooms {
    pub const TABLE: &str = "cached_rooms";
    pub const ROOM: &str = "room";
    pub const AT: &str = "at";
}

pub(super) struct MessagesCache {
    pool: SqlitePool,
}

/// 进程内只有一份缓存；服务启动时初始化，会话直接取用。
static SHARED: OnceLock<Option<Arc<MessagesCache>>> = OnceLock::new();

/// 初始化共享缓存。目录不可写时退化成「没有缓存」，不影响聊天。
pub(super) fn init(data_dir: &Path) {
    let path = data_dir.join("client-cache.db");
    let opened = match tokio::runtime::Handle::try_current() {
        Ok(runtime) => std::thread::spawn(move || runtime.block_on(MessagesCache::open(&path)))
            .join()
            .ok()
            .and_then(|opened| opened.ok())
            .map(Arc::new),
        Err(_) => None,
    };
    let _ = SHARED.set(opened);
}

/// 取共享缓存；没初始化过就等于没有缓存。
pub(super) fn shared() -> Option<Arc<MessagesCache>> {
    SHARED.get().and_then(|cache| cache.clone())
}

impl MessagesCache {
    pub(super) async fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .map_err(|e| e.to_string())?;
        let cache = Self { pool };
        cache.migrate().await.map_err(|e| e.to_string())?;
        Ok(cache)
    }

    async fn migrate(&self) -> Result<(), sqlx::Error> {
        let table = Table::create()
            .table(messages::TABLE)
            .if_not_exists()
            .col(ColumnDef::new(messages::ROOM).text().not_null())
            .col(ColumnDef::new(messages::SEQ).big_integer().not_null())
            .col(ColumnDef::new(messages::SENDER_ID).text().not_null())
            .col(ColumnDef::new(messages::SENDER).text().not_null())
            .col(ColumnDef::new(messages::AT).big_integer().not_null())
            .col(ColumnDef::new(messages::TEXT).text().not_null())
            .col(
                ColumnDef::new(messages::RECALLED)
                    .integer()
                    .not_null()
                    .default(0),
            )
            .col(ColumnDef::new(messages::ATTACHMENT_ID).text())
            .col(ColumnDef::new(messages::ATTACHMENT_NAME).text())
            .col(ColumnDef::new(messages::ATTACHMENT_SIZE).big_integer())
            .col(ColumnDef::new(messages::ATTACHMENT_MIME).text())
            .col(ColumnDef::new(messages::ATTACHMENT_SHA256).text())
            .col(ColumnDef::new(messages::QUOTE_SENDER).text())
            .col(ColumnDef::new(messages::QUOTE_TEXT).text())
            .col(ColumnDef::new(messages::QUOTE_SEQ).big_integer())
            .primary_key(
                sea_query::Index::create()
                    .col(messages::ROOM)
                    .col(messages::SEQ),
            )
            .to_owned();
        sqlx::query(&table.build(SqliteQueryBuilder))
            .execute(&self.pool)
            .await?;

        let rooms_table = Table::create()
            .table(rooms::TABLE)
            .if_not_exists()
            .col(ColumnDef::new(rooms::ROOM).text().not_null().primary_key())
            .col(ColumnDef::new(rooms::AT).big_integer().not_null())
            .to_owned();
        sqlx::query(&rooms_table.build(SqliteQueryBuilder))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 写入一批消息；同一条重复写会被主键挡掉。
    pub(super) async fn save(&self, room: RoomId, messages: &[HistoryMessage]) {
        for message in messages {
            let attachment = message.attachment.as_ref();
            let quote = message.quote.as_ref();
            let (sql, values) = Query::insert()
                .into_table(messages::TABLE)
                .columns([
                    messages::ROOM,
                    messages::SEQ,
                    messages::SENDER_ID,
                    messages::SENDER,
                    messages::AT,
                    messages::TEXT,
                    messages::RECALLED,
                    messages::ATTACHMENT_ID,
                    messages::ATTACHMENT_NAME,
                    messages::ATTACHMENT_SIZE,
                    messages::ATTACHMENT_MIME,
                    messages::ATTACHMENT_SHA256,
                    messages::QUOTE_SENDER,
                    messages::QUOTE_TEXT,
                    messages::QUOTE_SEQ,
                ])
                .values_panic([
                    room.to_string().into(),
                    message.seq.into(),
                    message.sender_id.to_string().into(),
                    message.sender.clone().into(),
                    message.at.into(),
                    message.text.clone().into(),
                    i64::from(message.recalled).into(),
                    attachment.map(|a| a.id.to_string()).into(),
                    attachment.map(|a| a.name.clone()).into(),
                    attachment.map(|a| a.size as i64).into(),
                    attachment.map(|a| a.mime.clone()).into(),
                    attachment.map(|a| a.sha256.clone()).into(),
                    quote.map(|q| q.sender.clone()).into(),
                    quote.map(|q| q.text.clone()).into(),
                    quote.and_then(|q| q.seq).into(),
                ])
                .on_conflict(
                    OnConflict::columns([messages::ROOM, messages::SEQ])
                        .do_nothing()
                        .to_owned(),
                )
                .build(SqliteQueryBuilder);
            if let Err(error) = bind(sqlx::query(&sql), values).execute(&self.pool).await {
                crate::service::log(&format!("写消息缓存失败: {error}"));
                return;
            }
        }
        self.trim(room).await;
        let _ = self.touch(room).await;
    }

    /// 只留最近 `KEEP_PER_ROOM` 条，库不会无限长大。
    async fn trim(&self, room: RoomId) {
        let (sql, values) = Query::select()
            .expr(Expr::col(messages::SEQ).max())
            .from(messages::TABLE)
            .and_where(Expr::col(messages::ROOM).eq(room.to_string()))
            .build(SqliteQueryBuilder);
        let Ok(row) = bind(sqlx::query(&sql), values).fetch_one(&self.pool).await else {
            return;
        };
        let Some(latest) = row.get::<Option<i64>, _>(0) else {
            return;
        };
        let (sql, values) = Query::delete()
            .from_table(messages::TABLE)
            .and_where(Expr::col(messages::ROOM).eq(room.to_string()))
            .and_where(Expr::col(messages::SEQ).lte(latest - KEEP_PER_ROOM))
            .build(SqliteQueryBuilder);
        let _ = bind(sqlx::query(&sql), values).execute(&self.pool).await;
    }

    async fn touch(&self, room: RoomId) -> Result<(), sqlx::Error> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let (sql, values) = Query::insert()
            .into_table(rooms::TABLE)
            .columns([rooms::ROOM, rooms::AT])
            .values_panic([room.to_string().into(), now.into()])
            .on_conflict(
                OnConflict::column(rooms::ROOM)
                    .update_column(rooms::AT)
                    .to_owned(),
            )
            .build(SqliteQueryBuilder);
        bind(sqlx::query(&sql), values).execute(&self.pool).await?;
        Ok(())
    }

    /// 读某个房间缓存的最近 `limit` 条，按 seq 升序。
    pub(super) async fn load(&self, room: RoomId, limit: usize) -> Vec<HistoryMessage> {
        let (sql, values) = Query::select()
            .columns([
                messages::SEQ,
                messages::SENDER_ID,
                messages::SENDER,
                messages::AT,
                messages::TEXT,
                messages::RECALLED,
                messages::ATTACHMENT_ID,
                messages::ATTACHMENT_NAME,
                messages::ATTACHMENT_SIZE,
                messages::ATTACHMENT_MIME,
                messages::ATTACHMENT_SHA256,
                messages::QUOTE_SENDER,
                messages::QUOTE_TEXT,
                messages::QUOTE_SEQ,
            ])
            .from(messages::TABLE)
            .and_where(Expr::col(messages::ROOM).eq(room.to_string()))
            .order_by(messages::SEQ, sea_query::Order::Desc)
            .limit(limit as u64)
            .build(SqliteQueryBuilder);
        let rows = match bind(sqlx::query(&sql), values).fetch_all(&self.pool).await {
            Ok(rows) => rows,
            Err(error) => {
                crate::service::log(&format!("读消息缓存失败: {error}"));
                return Vec::new();
            }
        };
        let mut messages: Vec<HistoryMessage> = rows
            .iter()
            .map(|row| HistoryMessage {
                seq: row.get(0),
                sender_id: row
                    .get::<String, _>(1)
                    .parse()
                    .unwrap_or_else(|_| UserId::new()),
                sender: row.get(2),
                at: row.get(3),
                text: row.get(4),
                recalled: row.get::<i64, _>(5) != 0,
                forwarded: false,
                attachment: row.get::<Option<String>, _>(6).map(|id| Attachment {
                    id: id.parse().unwrap_or_else(|_| AttachmentId::new()),
                    name: row.get::<Option<String>, _>(7).unwrap_or_default(),
                    size: row.get::<Option<i64>, _>(8).unwrap_or(0).max(0) as u64,
                    mime: row.get::<Option<String>, _>(9).unwrap_or_default(),
                    sha256: row.get::<Option<String>, _>(10).unwrap_or_default(),
                }),
                quote: row.get::<Option<String>, _>(11).map(|sender| Quote {
                    sender,
                    text: row.get::<Option<String>, _>(12).unwrap_or_default(),
                    seq: row.get::<Option<i64>, _>(13),
                }),
            })
            .collect();
        messages.reverse();
        messages
    }

    /// 撤回后同步缓存，免得重启还看得到旧内容。
    pub(super) async fn apply_recall(&self, room: RoomId, seq: Seq) {
        let (sql, values) = Query::update()
            .table(messages::TABLE)
            .value(messages::TEXT, "")
            .value(messages::RECALLED, 1_i64)
            .value(messages::ATTACHMENT_ID, Option::<String>::None)
            .value(messages::ATTACHMENT_NAME, Option::<String>::None)
            .value(messages::ATTACHMENT_SIZE, Option::<i64>::None)
            .value(messages::ATTACHMENT_MIME, Option::<String>::None)
            .value(messages::ATTACHMENT_SHA256, Option::<String>::None)
            .value(messages::QUOTE_SENDER, Option::<String>::None)
            .value(messages::QUOTE_TEXT, Option::<String>::None)
            .value(messages::QUOTE_SEQ, Option::<i64>::None)
            .and_where(Expr::col(messages::ROOM).eq(room.to_string()))
            .and_where(Expr::col(messages::SEQ).eq(seq))
            .build(SqliteQueryBuilder);
        let _ = bind(sqlx::query(&sql), values).execute(&self.pool).await;
    }

    pub(super) async fn apply_edit(&self, room: RoomId, seq: Seq, text: &str) {
        let (sql, values) = Query::update()
            .table(messages::TABLE)
            .value(messages::TEXT, text.to_string())
            .and_where(Expr::col(messages::ROOM).eq(room.to_string()))
            .and_where(Expr::col(messages::SEQ).eq(seq))
            .build(SqliteQueryBuilder);
        let _ = bind(sqlx::query(&sql), values).execute(&self.pool).await;
    }
}

fn bind<'q>(
    query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    values: sea_query::Values,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    values
        .0
        .into_iter()
        .fold(query, |query, value| match value {
            sea_query::Value::String(Some(value)) => query.bind(*value),
            sea_query::Value::BigInt(Some(value)) => query.bind(value),
            sea_query::Value::Int(Some(value)) => query.bind(i64::from(value)),
            sea_query::Value::Unsigned(Some(value)) => query.bind(i64::from(value)),
            sea_query::Value::BigUnsigned(Some(value)) => query.bind(value as i64),
            _ => query.bind(Option::<String>::None),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("libchat-cache-{tag}-{}.db", uuid::Uuid::new_v4()))
    }

    fn message(seq: i64, text: &str) -> HistoryMessage {
        HistoryMessage {
            seq,
            sender_id: UserId::new(),
            sender: "Alice".into(),
            at: seq,
            text: text.into(),
            attachment: None,
            quote: None,
            recalled: false,
            forwarded: false,
        }
    }

    #[tokio::test]
    async fn keeps_the_latest_messages_in_order_and_drops_the_oldest() {
        let path = temp_path("trim");
        let cache = MessagesCache::open(&path).await.unwrap();
        let room = RoomId::new();
        let batch: Vec<_> = (1..=(KEEP_PER_ROOM + 20))
            .map(|seq| message(seq, &format!("第 {seq} 条")))
            .collect();
        cache.save(room, &batch).await;

        let loaded = cache.load(room, 1000).await;
        assert_eq!(loaded.len() as i64, KEEP_PER_ROOM);
        assert_eq!(loaded.first().unwrap().seq, 21);
        assert_eq!(loaded.last().unwrap().seq, KEEP_PER_ROOM + 20);
        drop(cache);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn reopening_the_database_keeps_the_history() {
        let path = temp_path("reopen");
        let room = RoomId::new();
        {
            let cache = MessagesCache::open(&path).await.unwrap();
            cache
                .save(room, &[message(1, "第一句"), message(2, "第二句")])
                .await;
        }
        // 相当于重开客户端：同一个库文件再读一次
        let cache = MessagesCache::open(&path).await.unwrap();
        let loaded = cache.load(room, 50).await;
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].text, "第一句");
        assert_eq!(loaded[1].text, "第二句");
        drop(cache);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn recall_and_edit_replace_the_cached_copy() {
        let path = temp_path("edit");
        let cache = MessagesCache::open(&path).await.unwrap();
        let room = RoomId::new();
        cache
            .save(room, &[message(1, "原文"), message(2, "要撤回的")])
            .await;

        cache.apply_edit(room, 1, "改过了").await;
        cache.apply_recall(room, 2).await;

        let loaded = cache.load(room, 10).await;
        assert_eq!(loaded[0].text, "改过了");
        assert_eq!(loaded[1].text, "");
        assert!(loaded[1].recalled);
        drop(cache);
        let _ = std::fs::remove_file(&path);
    }
}
