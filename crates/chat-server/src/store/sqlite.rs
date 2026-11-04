//! 消息落库：SQLite + sea-query。
//!
//! 消息全部落盘，进程内不留副本——房间历史再长也不会把内存撑爆。
//! 房间标识与消息同库保存：库在，房间就在，重启后历史仍读得回来。

use std::path::Path;

use async_trait::async_trait;
use chat_core::{AppendResult, ChatError, MessageStore, NewMessage, Result};
use chat_core::{Attachment, AttachmentId, Message, MessageKind, Quote, RoomId, Seq, UserId};
use sea_query::{
    ColumnDef, Expr, Index, Order, Query, SchemaStatement, SqliteQueryBuilder, Table, Value, Values,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use super::now_ms;

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
    pub const ATTACHMENT_ID: &str = "attachment_id";
    pub const ATTACHMENT_NAME: &str = "attachment_name";
    pub const ATTACHMENT_SIZE: &str = "attachment_size";
    pub const ATTACHMENT_MIME: &str = "attachment_mime";
    pub const ATTACHMENT_SHA256: &str = "attachment_sha256";
    pub const QUOTE_SENDER: &str = "quote_sender";
    pub const QUOTE_TEXT: &str = "quote_text";
    pub const QUOTE_SEQ: &str = "quote_seq";
    pub const RECALLED_AT: &str = "recalled_at";
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
const MESSAGE_COLUMNS: [&str; 16] = [
    messages::SEQ,
    messages::MSG_ID,
    messages::SENDER,
    messages::SENDER_NAME,
    messages::KIND,
    messages::BODY,
    messages::SERVER_TS,
    messages::ATTACHMENT_ID,
    messages::ATTACHMENT_NAME,
    messages::ATTACHMENT_SIZE,
    messages::ATTACHMENT_MIME,
    messages::ATTACHMENT_SHA256,
    messages::QUOTE_SENDER,
    messages::QUOTE_TEXT,
    messages::QUOTE_SEQ,
    messages::RECALLED_AT,
];

pub struct SqliteStore {
    pool: SqlitePool,
    room: RoomId,
}

impl SqliteStore {
    /// 库里引用到的附件摘要，回收孤儿附件用。
    pub async fn referenced_blobs(&self, room: RoomId) -> Result<Vec<String>> {
        let (sql, values) = Query::select()
            .column(messages::ATTACHMENT_SHA256)
            .distinct()
            .from(messages::TABLE)
            .and_where(Expr::col(messages::ROOM_ID).eq(room.to_string()))
            .and_where(Expr::col(messages::ATTACHMENT_SHA256).is_not_null())
            .build(SqliteQueryBuilder);
        let rows = bind(sqlx::query(&sql), values)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(rows.iter().map(|row| row.get::<String, _>(0)).collect())
    }

    /// 少于三个字符的查询：trigram 索引用不上，只能逐条比对。
    async fn search_substring(
        &self,
        room: RoomId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
        let (sql, values) = Query::select()
            .columns(MESSAGE_COLUMNS)
            .from(messages::TABLE)
            .and_where(Expr::col(messages::ROOM_ID).eq(room.to_string()))
            .and_where(Expr::col(messages::BODY).like(&pattern))
            .order_by(messages::SEQ, Order::Desc)
            .limit(limit as u64)
            .build(SqliteQueryBuilder);
        let rows = bind(sqlx::query(&sql), values)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        let mut messages = rows
            .iter()
            .map(|row| row_to_message(room, row))
            .collect::<Result<Vec<_>>>()?;
        messages.reverse();
        Ok(messages)
    }

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
        };
        store.migrate().await?;
        let room = store.load_or_init_room().await?;
        Ok(Self { room, ..store })
    }

    /// 本库对应的房间标识。
    pub fn room(&self) -> RoomId {
        self.room
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    async fn migrate(&self) -> Result<()> {
        super::migrate::apply(&self.pool, MIGRATIONS).await
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
    async fn append(&self, room: RoomId, msg: NewMessage) -> Result<AppendResult> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(db_err)?;
        let result: Result<AppendResult> = async {
            let (sql, values) = Query::select()
                .columns(MESSAGE_COLUMNS)
                .from(messages::TABLE)
                .and_where(Expr::col(messages::ROOM_ID).eq(room.to_string()))
                .and_where(Expr::col(messages::MSG_ID).eq(msg.client_msg_id.to_string()))
                .build(SqliteQueryBuilder);
            if let Some(row) = bind(sqlx::query(&sql), values)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(db_err)?
            {
                let stored = row_to_message(room, &row)?;
                if stored.text != msg.text
                    || stored.kind != msg.kind
                    || stored.quote != msg.quote
                    || stored.attachment.as_ref().map(|a| &a.sha256)
                        != msg.attachment.as_ref().map(|a| &a.sha256)
                {
                    return Err(ChatError::Protocol("消息标识与原始内容不一致".into()));
                }
                return Ok(AppendResult {
                    message: stored,
                    inserted: false,
                });
            }

            let (sql, values) = Query::select()
                .expr(Expr::col(messages::SEQ).max())
                .from(messages::TABLE)
                .and_where(Expr::col(messages::ROOM_ID).eq(room.to_string()))
                .build(SqliteQueryBuilder);
            let latest = bind(sqlx::query(&sql), values)
                .fetch_one(&mut *transaction)
                .await
                .map_err(db_err)?
                .get::<Option<i64>, _>(0)
                .unwrap_or(0);
            let stored = Message {
                room,
                seq: latest + 1,
                client_msg_id: msg.client_msg_id,
                sender: msg.sender,
                sender_name: msg.sender_name,
                kind: msg.kind,
                text: msg.text,
                attachment: msg.attachment,
                quote: msg.quote,
                recalled: false,
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
                    stored.room.to_string().into(),
                    stored.seq.into(),
                    stored.client_msg_id.to_string().into(),
                    stored.sender.to_string().into(),
                    stored.sender_name.clone().into(),
                    kind_str(stored.kind).into(),
                    stored.text.clone().into(),
                    stored.server_ts_ms.into(),
                    stored.attachment.as_ref().map(|a| a.id.to_string()).into(),
                    stored.attachment.as_ref().map(|a| a.name.clone()).into(),
                    stored.attachment.as_ref().map(|a| a.size as i64).into(),
                    stored.attachment.as_ref().map(|a| a.mime.clone()).into(),
                    stored.attachment.as_ref().map(|a| a.sha256.clone()).into(),
                    stored.quote.as_ref().map(|q| q.sender.clone()).into(),
                    stored.quote.as_ref().map(|q| q.text.clone()).into(),
                    stored.quote.as_ref().and_then(|q| q.seq).into(),
                ])
                .build(SqliteQueryBuilder);

            bind(sqlx::query(&sql), values)
                .execute(&mut *transaction)
                .await
                .map_err(db_err)?;
            index_written_body(&mut transaction, &stored.text).await?;
            Ok(AppendResult {
                message: stored,
                inserted: true,
            })
        }
        .await;
        match result {
            Ok(value) => {
                transaction.commit().await.map_err(db_err)?;
                Ok(value)
            }
            Err(error) => {
                transaction.rollback().await.map_err(db_err)?;
                Err(error)
            }
        }
    }

    async fn history(
        &self,
        room: RoomId,
        before: Option<Seq>,
        limit: usize,
    ) -> Result<Vec<Message>> {
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

    async fn search(&self, room: RoomId, query: &str, limit: usize) -> Result<Vec<Message>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        // trigram 分词至少要三个字符才建得出索引项，短查询退回逐字匹配
        if query.chars().count() < 3 {
            return self.search_substring(room, query, limit).await;
        }
        // FTS5 的 MATCH 语法无法用 sea-query 构造，这里直接写 SQL
        let sql = format!(
            "SELECT {} FROM messages WHERE room_id = ? AND rowid IN \
             (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?) \
             ORDER BY seq DESC LIMIT ?",
            MESSAGE_COLUMNS.join(", ")
        );
        let rows = sqlx::query(&sql)
            .bind(room.to_string())
            .bind(phrase(query))
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        let mut messages = rows
            .iter()
            .map(|row| row_to_message(room, row))
            .collect::<Result<Vec<_>>>()?;
        messages.reverse();
        Ok(messages)
    }

    async fn recall(&self, room: RoomId, seq: Seq, sender: UserId) -> Result<bool> {
        // 保留这一行，只清内容并打上时间戳：历史与同步都按 seq 连续，不能留空洞
        let (sql, values) = Query::update()
            .table(messages::TABLE)
            .value(messages::RECALLED_AT, now_ms())
            .value(messages::BODY, "")
            .value(messages::ATTACHMENT_ID, Option::<String>::None)
            .value(messages::ATTACHMENT_NAME, Option::<String>::None)
            .value(messages::ATTACHMENT_SIZE, Option::<i64>::None)
            .value(messages::ATTACHMENT_MIME, Option::<String>::None)
            .value(messages::ATTACHMENT_SHA256, Option::<String>::None)
            .value(messages::QUOTE_SENDER, Option::<String>::None)
            .value(messages::QUOTE_TEXT, Option::<String>::None)
            .value(messages::QUOTE_SEQ, Option::<i64>::None)
            .and_where(Expr::col(messages::ROOM_ID).eq(room.to_string()))
            .and_where(Expr::col(messages::SEQ).eq(seq))
            .and_where(Expr::col(messages::SENDER).eq(sender.to_string()))
            .and_where(Expr::col(messages::RECALLED_AT).is_null())
            .build(SqliteQueryBuilder);
        let result = bind(sqlx::query(&sql), values)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        if result.rows_affected() > 0 {
            drop_indexed_body(&self.pool, room, seq).await?;
        }
        Ok(result.rows_affected() > 0)
    }

    async fn edit(
        &self,
        room: RoomId,
        seq: Seq,
        sender: UserId,
        text: String,
    ) -> Result<Option<Message>> {
        let (sql, values) = Query::update()
            .table(messages::TABLE)
            .value(messages::BODY, text.clone())
            .and_where(Expr::col(messages::ROOM_ID).eq(room.to_string()))
            .and_where(Expr::col(messages::SEQ).eq(seq))
            .and_where(Expr::col(messages::SENDER).eq(sender.to_string()))
            .and_where(Expr::col(messages::RECALLED_AT).is_null())
            .build(SqliteQueryBuilder);
        let result = bind(sqlx::query(&sql), values)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        sqlx::query(
            "UPDATE messages_fts SET body = ? WHERE rowid = \
             (SELECT rowid FROM messages WHERE room_id = ? AND seq = ?)",
        )
        .bind(&text)
        .bind(room.to_string())
        .bind(seq)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        let (sql, values) = Query::select()
            .columns(MESSAGE_COLUMNS)
            .from(messages::TABLE)
            .and_where(Expr::col(messages::ROOM_ID).eq(room.to_string()))
            .and_where(Expr::col(messages::SEQ).eq(seq))
            .build(SqliteQueryBuilder);
        let row = bind(sqlx::query(&sql), values)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        row.map(|row| row_to_message(room, &row)).transpose()
    }

    async fn clear(&self, room: RoomId) -> Result<u64> {
        // 先清索引：消息行删掉之后就找不回 rowid 了
        sqlx::query(
            "DELETE FROM messages_fts WHERE rowid IN \
             (SELECT rowid FROM messages WHERE room_id = ?)",
        )
        .bind(room.to_string())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        let (sql, values) = Query::delete()
            .from_table(messages::TABLE)
            .and_where(Expr::col(messages::ROOM_ID).eq(room.to_string()))
            .build(SqliteQueryBuilder);
        let result = bind(sqlx::query(&sql), values)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(result.rows_affected())
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

    async fn attachment(&self, room: RoomId, id: AttachmentId) -> Result<Option<Attachment>> {
        let (sql, values) = Query::select()
            .columns(MESSAGE_COLUMNS)
            .from(messages::TABLE)
            .and_where(Expr::col(messages::ROOM_ID).eq(room.to_string()))
            .and_where(Expr::col(messages::ATTACHMENT_ID).eq(id.to_string()))
            .build(SqliteQueryBuilder);
        let row = bind(sqlx::query(&sql), values)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        match row {
            Some(row) => Ok(row_to_message(room, &row)?.attachment),
            None => Ok(None),
        }
    }
}

fn row_to_attachment(row: &sqlx::sqlite::SqliteRow) -> Result<Option<Attachment>> {
    let Some(id) = row.get::<Option<String>, _>(7) else {
        return Ok(None);
    };
    Ok(Some(Attachment {
        id: id
            .parse()
            .map_err(|e| ChatError::Storage(format!("附件标识无效: {e}")))?,
        name: row.get::<Option<String>, _>(8).unwrap_or_default(),
        size: row.get::<Option<i64>, _>(9).unwrap_or(0).max(0) as u64,
        mime: row.get::<Option<String>, _>(10).unwrap_or_default(),
        sha256: row.get::<Option<String>, _>(11).unwrap_or_default(),
    }))
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
        attachment: row_to_attachment(row)?,
        quote: row_to_quote(row),
        recalled: row.get::<Option<i64>, _>(15).is_some(),
        server_ts_ms: row.get(6),
    })
}

fn row_to_quote(row: &sqlx::sqlite::SqliteRow) -> Option<Quote> {
    let sender = row.get::<Option<String>, _>(12)?;
    let text = row.get::<Option<String>, _>(13)?;
    Some(Quote {
        sender,
        text,
        seq: row.get::<Option<i64>, _>(14),
    })
}

fn kind_str(kind: MessageKind) -> &'static str {
    match kind {
        MessageKind::Text => "text",
        MessageKind::File => "file",
        MessageKind::System => "system",
    }
}

fn kind_from_str(text: &str) -> MessageKind {
    match text {
        "file" => MessageKind::File,
        "system" => MessageKind::System,
        _ => MessageKind::Text,
    }
}

fn db_err(error: sqlx::Error) -> ChatError {
    ChatError::Storage(error.to_string())
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

/// 库结构的演进顺序；只往后加，不改已经发出去的那几步。
const MIGRATIONS: &[super::migrate::Migration] = &[
    super::migrate::Migration {
        version: 1,
        name: "消息与房间标识",
        schema: v1_schema,
        raw: &[],
    },
    super::migrate::Migration {
        version: 2,
        name: "全文搜索索引",
        schema: empty_schema,
        raw: &[
            // FTS5 虚拟表与 trigram 分词无法用 sea-query 表达。
            // trigram 让中文按三字窗口切分，`MATCH` 才能命中子串。
            "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts \
             USING fts5(body, tokenize='trigram')",
            "INSERT INTO messages_fts(rowid, body) \
             SELECT rowid, body FROM messages WHERE recalled_at IS NULL AND body != ''",
        ],
    },
];

fn empty_schema() -> Vec<SchemaStatement> {
    Vec::new()
}

fn v1_schema() -> Vec<SchemaStatement> {
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
        .col(ColumnDef::new(messages::ATTACHMENT_ID).text())
        .col(ColumnDef::new(messages::ATTACHMENT_NAME).text())
        .col(ColumnDef::new(messages::ATTACHMENT_SIZE).big_integer())
        .col(ColumnDef::new(messages::ATTACHMENT_MIME).text())
        .col(ColumnDef::new(messages::ATTACHMENT_SHA256).text())
        .col(ColumnDef::new(messages::QUOTE_SENDER).text())
        .col(ColumnDef::new(messages::QUOTE_TEXT).text())
        .col(ColumnDef::new(messages::QUOTE_SEQ).big_integer())
        .col(ColumnDef::new(messages::RECALLED_AT).big_integer())
        .primary_key(Index::create().col(messages::ROOM_ID).col(messages::SEQ))
        .to_owned();

    let meta_table = Table::create()
        .table(room_meta::TABLE)
        .if_not_exists()
        .col(
            ColumnDef::new(room_meta::ID)
                .integer()
                .not_null()
                .primary_key(),
        )
        .col(ColumnDef::new(room_meta::ROOM_ID).text().not_null())
        .to_owned();

    let index = Index::create()
        .name("idx_messages_room_msg_id")
        .table(messages::TABLE)
        .col(messages::ROOM_ID)
        .col(messages::MSG_ID)
        .unique()
        .if_not_exists()
        .to_owned();

    vec![
        super::migrate::table(messages_table),
        super::migrate::table(meta_table),
        super::migrate::index(index),
    ]
}

/// 把刚写入的消息正文放进全文索引；文件消息没有正文，跳过。
async fn index_written_body(tx: &mut sqlx::SqliteConnection, text: &str) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    sqlx::query("INSERT INTO messages_fts(rowid, body) VALUES (last_insert_rowid(), ?)")
        .bind(text)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    Ok(())
}

/// 撤回时把正文从索引里删掉。
async fn drop_indexed_body(pool: &SqlitePool, room: RoomId, seq: Seq) -> Result<()> {
    sqlx::query(
        "DELETE FROM messages_fts WHERE rowid = \
         (SELECT rowid FROM messages WHERE room_id = ? AND seq = ?)",
    )
    .bind(room.to_string())
    .bind(seq)
    .execute(pool)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// 把用户输入包成一个 FTS5 短语，避免其中的符号被当成查询语法。
fn phrase(query: &str) -> String {
    format!("\"{}\"", query.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use chat_core::{Message, MessageKind, MessageStore, NewMessage, RoomId, UserId};
    use uuid::Uuid;

    use super::SqliteStore;

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
            attachment: None,
            quote: None,
        }
    }

    #[tokio::test]
    async fn appends_in_order_and_pages_both_ways() {
        let path = temp_path("pages");
        let _ = std::fs::remove_file(&path);
        let store = SqliteStore::open(&path).await.unwrap();
        let room = store.room();

        for i in 1..=5 {
            let stored = store
                .append(room, incoming(&format!("m{i}")))
                .await
                .unwrap();
            assert_eq!(stored.message.seq, i);
            assert!(stored.inserted);
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
        assert_eq!(
            texts(&reopened.history(room, None, 10).await.unwrap()),
            ["说过的话"]
        );
    }

    #[tokio::test]
    async fn repeated_message_keeps_original_seq_after_reopen() {
        let path = temp_path("dedup");
        let _ = std::fs::remove_file(&path);
        let request = incoming("一次");
        let (room, first) = {
            let store = SqliteStore::open(&path).await.unwrap();
            let room = store.room();
            let first = store.append(room, request.clone()).await.unwrap();
            assert!(first.inserted);
            (room, first.message)
        };
        let store = SqliteStore::open(&path).await.unwrap();
        let mut renamed = request.clone();
        renamed.sender = UserId::new();
        renamed.sender_name = "换了昵称".into();
        let repeated = store.append(room, renamed).await.unwrap();
        assert!(!repeated.inserted);
        assert_eq!(repeated.message.seq, first.seq);
        assert_eq!(repeated.message.sender_name, "Alice");
        assert_eq!(store.latest_seq(room).await.unwrap(), 1);
        let mut collision = request;
        collision.text = "不同内容".into();
        assert!(store.append(room, collision).await.is_err());
        assert_eq!(
            store
                .append(room, incoming("可继续写"))
                .await
                .unwrap()
                .message
                .seq,
            2
        );
    }

    #[tokio::test]
    async fn concurrent_stores_assign_unique_sequences_and_deduplicate() {
        let path = std::env::temp_dir().join(format!("libchat-concurrent-{}.db", Uuid::new_v4()));
        let first = SqliteStore::open(&path).await.unwrap();
        let second = SqliteStore::open(&path).await.unwrap();
        let room = first.room();
        assert_eq!(second.room(), room);
        let request = incoming("仅一次");
        let (a, b) = tokio::join!(
            first.append(room, request.clone()),
            second.append(room, request)
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a.message.seq, b.message.seq);
        assert_ne!(a.inserted, b.inserted);
        assert_eq!(first.latest_seq(room).await.unwrap(), 1);
        let (a, b) = tokio::join!(
            first.append(room, incoming("不同的 A")),
            second.append(room, incoming("不同的 B")),
        );
        assert!(a.unwrap().inserted);
        assert!(b.unwrap().inserted);
        assert_eq!(second.latest_seq(room).await.unwrap(), 3);
        first.close().await;
        second.close().await;
        let _ = std::fs::remove_file(path);
    }

    fn texts(messages: &[Message]) -> Vec<&str> {
        messages.iter().map(|m| m.text.as_str()).collect()
    }
}

#[cfg(test)]
mod search_tests {
    use chat_core::{MessageStore, NewMessage, RoomId, UserId};
    use uuid::Uuid;

    use super::SqliteStore;

    fn path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("libchat-fts-{tag}-{}.db", std::process::id()))
    }

    fn message(text: &str) -> NewMessage {
        NewMessage {
            client_msg_id: Uuid::new_v4(),
            sender: UserId::new(),
            sender_name: "alice".into(),
            kind: chat_core::MessageKind::Text,
            text: text.to_string(),
            attachment: None,
            quote: None,
        }
    }

    /// 中文按 trigram 建索引：任意三字以上的子串都能命中。
    #[tokio::test]
    async fn searches_chinese_by_substring_through_the_full_text_index() {
        let path = path("zh");
        let _ = std::fs::remove_file(&path);
        let store = SqliteStore::open(&path).await.unwrap();
        let room = store.room();

        store
            .append(room, message("今天的部署流程有问题"))
            .await
            .unwrap();
        store.append(room, message("午饭吃什么")).await.unwrap();
        store.append(room, message("部署完了再吃饭")).await.unwrap();

        let hits = store.search(room, "部署", 10).await.unwrap();
        assert_eq!(hits.len(), 2, "两个字走逐字匹配");
        let hits = store.search(room, "部署流程", 10).await.unwrap();
        assert_eq!(hits.len(), 1, "四个字走全文索引");
        assert_eq!(hits[0].text, "今天的部署流程有问题");
        // 结果按时间正序
        let hits = store.search(room, "吃饭", 10).await.unwrap();
        assert_eq!(hits.last().unwrap().text, "部署完了再吃饭");

        store.close().await;
        let _ = std::fs::remove_file(&path);
    }

    /// 撤回与编辑都要同步索引，否则会搜到已经不存在的内容。
    #[tokio::test]
    async fn recalled_and_edited_text_leave_the_index_in_sync() {
        let path = path("sync");
        let _ = std::fs::remove_file(&path);
        let store = SqliteStore::open(&path).await.unwrap();
        let room: RoomId = store.room();

        let first = store
            .append(room, message("要撤回的敏感内容"))
            .await
            .unwrap();
        let second = store.append(room, message("要改写的旧内容")).await.unwrap();

        store
            .recall(room, first.message.seq, first.message.sender)
            .await
            .unwrap();
        assert!(
            store.search(room, "敏感内容", 10).await.unwrap().is_empty(),
            "撤回后不该再搜到"
        );

        let sender = second.message.sender;
        store
            .edit(room, second.message.seq, sender, "改写后的新内容".into())
            .await
            .unwrap();
        assert!(store.search(room, "旧内容", 10).await.unwrap().is_empty());
        assert_eq!(store.search(room, "改写后", 10).await.unwrap().len(), 1);

        store.close().await;
        let _ = std::fs::remove_file(&path);
    }
}
