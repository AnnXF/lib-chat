//! 账号、好友与会话（房间）元数据。
//!
//! 与消息存储分离：这里只回答「谁是谁、谁和谁是好友、谁在哪个房间」，
//! 消息仍在 `messages` 表里按 `room_id` 组织。

use std::path::Path;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use chat_core::{
    ChatError, MemberRole, RoomId, RoomKind, RoomMember, RoomSummary, Seq, UserId, UserSummary,
};
use sea_query::{ColumnDef, Expr, Query, SqliteQueryBuilder, Table};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use sqlx::Row;
use uuid::Uuid;

/// 登录令牌有效期：30 天。
const SESSION_TTL_SECS: i64 = 30 * 24 * 60 * 60;

mod users {
    pub const TABLE: &str = "users";
    pub const ID: &str = "id";
    pub const USERNAME: &str = "username";
    pub const DISPLAY_NAME: &str = "display_name";
    pub const PASSWORD_HASH: &str = "password_hash";
    pub const CREATED_AT: &str = "created_at";
    pub const BANNED: &str = "banned";
}

mod sessions {
    pub const TABLE: &str = "sessions";
    pub const TOKEN: &str = "token";
    pub const USER_ID: &str = "user_id";
    pub const CREATED_AT: &str = "created_at";
    pub const EXPIRES_AT: &str = "expires_at";
}

mod friends {
    pub const TABLE: &str = "friends";
    pub const USER_ID: &str = "user_id";
    pub const FRIEND_ID: &str = "friend_id";
    pub const CREATED_AT: &str = "created_at";
}

mod rooms {
    pub const TABLE: &str = "rooms";
    pub const ID: &str = "id";
    pub const KIND: &str = "kind";
    pub const NAME: &str = "name";
    pub const OWNER_ID: &str = "owner_id";
    /// 单聊去重键：两个用户标识按字典序拼接；群聊为 NULL。
    pub const DIRECT_KEY: &str = "direct_key";
    pub const CREATED_AT: &str = "created_at";
}

mod room_members {
    pub const TABLE: &str = "room_members";
    pub const ROOM_ID: &str = "room_id";
    pub const USER_ID: &str = "user_id";
    pub const ROLE: &str = "role";
    pub const JOINED_AT: &str = "joined_at";
}

mod read_markers {
    pub const TABLE: &str = "read_markers";
    pub const USER_ID: &str = "user_id";
    pub const ROOM_ID: &str = "room_id";
    pub const SEQ: &str = "seq";
    pub const UPDATED_AT: &str = "updated_at";
}

/// 账号层的错误，交给 Hub 映射成协议错误码。
#[derive(Debug)]
pub enum AccountError {
    Invalid(String),
    UsernameTaken,
    BadCredentials,
    Unauthorized,
    Storage(String),
}

impl std::fmt::Display for AccountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "{message}"),
            Self::UsernameTaken => write!(f, "用户名已存在"),
            Self::BadCredentials => write!(f, "用户名或密码错误"),
            Self::Unauthorized => write!(f, "未登录或登录已过期"),
            Self::Storage(message) => write!(f, "存储错误: {message}"),
        }
    }
}

impl std::error::Error for AccountError {}

impl From<sqlx::Error> for AccountError {
    fn from(error: sqlx::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

impl From<AccountError> for ChatError {
    fn from(error: AccountError) -> Self {
        ChatError::Storage(error.to_string())
    }
}

pub struct Accounts {
    pool: SqlitePool,
}

impl Accounts {
    pub async fn open(path: &Path) -> std::result::Result<Self, AccountError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AccountError::Storage(e.to_string()))?;
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        let accounts = Self { pool };
        accounts.migrate().await?;
        Ok(accounts)
    }

    async fn migrate(&self) -> std::result::Result<(), AccountError> {
        let users = Table::create()
            .table(users::TABLE)
            .if_not_exists()
            .col(ColumnDef::new(users::ID).text().not_null().primary_key())
            .col(
                ColumnDef::new(users::USERNAME)
                    .text()
                    .not_null()
                    .unique_key(),
            )
            .col(ColumnDef::new(users::DISPLAY_NAME).text().not_null())
            .col(ColumnDef::new(users::PASSWORD_HASH).text().not_null())
            .col(ColumnDef::new(users::CREATED_AT).big_integer().not_null())
            .col(
                ColumnDef::new(users::BANNED)
                    .integer()
                    .not_null()
                    .default(0),
            )
            .to_owned();
        let sessions = Table::create()
            .table(sessions::TABLE)
            .if_not_exists()
            .col(
                ColumnDef::new(sessions::TOKEN)
                    .text()
                    .not_null()
                    .primary_key(),
            )
            .col(ColumnDef::new(sessions::USER_ID).text().not_null())
            .col(
                ColumnDef::new(sessions::CREATED_AT)
                    .big_integer()
                    .not_null(),
            )
            .col(
                ColumnDef::new(sessions::EXPIRES_AT)
                    .big_integer()
                    .not_null(),
            )
            .to_owned();
        let friends = Table::create()
            .table(friends::TABLE)
            .if_not_exists()
            .col(ColumnDef::new(friends::USER_ID).text().not_null())
            .col(ColumnDef::new(friends::FRIEND_ID).text().not_null())
            .col(ColumnDef::new(friends::CREATED_AT).big_integer().not_null())
            .primary_key(
                sea_query::Index::create()
                    .col(friends::USER_ID)
                    .col(friends::FRIEND_ID),
            )
            .to_owned();
        let rooms = Table::create()
            .table(rooms::TABLE)
            .if_not_exists()
            .col(ColumnDef::new(rooms::ID).text().not_null().primary_key())
            .col(ColumnDef::new(rooms::KIND).text().not_null())
            .col(ColumnDef::new(rooms::NAME).text().not_null())
            .col(ColumnDef::new(rooms::OWNER_ID).text())
            .col(ColumnDef::new(rooms::DIRECT_KEY).text().unique_key())
            .col(ColumnDef::new(rooms::CREATED_AT).big_integer().not_null())
            .to_owned();
        let members = Table::create()
            .table(room_members::TABLE)
            .if_not_exists()
            .col(ColumnDef::new(room_members::ROOM_ID).text().not_null())
            .col(ColumnDef::new(room_members::USER_ID).text().not_null())
            .col(ColumnDef::new(room_members::ROLE).text().not_null())
            .col(
                ColumnDef::new(room_members::JOINED_AT)
                    .big_integer()
                    .not_null(),
            )
            .primary_key(
                sea_query::Index::create()
                    .col(room_members::ROOM_ID)
                    .col(room_members::USER_ID),
            )
            .to_owned();
        let read_markers = Table::create()
            .table(read_markers::TABLE)
            .if_not_exists()
            .col(ColumnDef::new(read_markers::USER_ID).text().not_null())
            .col(ColumnDef::new(read_markers::ROOM_ID).text().not_null())
            .col(ColumnDef::new(read_markers::SEQ).big_integer().not_null())
            .col(
                ColumnDef::new(read_markers::UPDATED_AT)
                    .big_integer()
                    .not_null(),
            )
            .primary_key(
                sea_query::Index::create()
                    .col(read_markers::USER_ID)
                    .col(read_markers::ROOM_ID),
            )
            .to_owned();

        for table in [users, sessions, friends, rooms, members, read_markers] {
            let sql = table.build(SqliteQueryBuilder);
            sqlx::query(&sql).execute(&self.pool).await?;
        }
        // 旧库没有 banned 列，补一次；已存在时 SQLite 会报错，忽略即可
        let _ = sqlx::query("ALTER TABLE users ADD COLUMN banned INTEGER NOT NULL DEFAULT 0")
            .execute(&self.pool)
            .await;
        Ok(())
    }

    pub async fn register(
        &self,
        username: &str,
        password: &str,
        display_name: &str,
    ) -> std::result::Result<(UserSummary, String), AccountError> {
        let username = normalize_username(username)?;
        validate_password(password)?;
        let display_name = normalize_display_name(display_name, &username);

        let (sql, values) = Query::select()
            .column(users::ID)
            .from(users::TABLE)
            .and_where(Expr::col(users::USERNAME).eq(username.clone()))
            .build(SqliteQueryBuilder);
        if bind(sqlx::query(&sql), values)
            .fetch_optional(&self.pool)
            .await?
            .is_some()
        {
            return Err(AccountError::UsernameTaken);
        }

        let hash = hash_password(password)?;
        let id = UserId::new();
        let now = now_secs();
        let (sql, values) = Query::insert()
            .into_table(users::TABLE)
            .columns([
                users::ID,
                users::USERNAME,
                users::DISPLAY_NAME,
                users::PASSWORD_HASH,
                users::CREATED_AT,
            ])
            .values_panic([
                id.to_string().into(),
                username.clone().into(),
                display_name.clone().into(),
                hash.into(),
                now.into(),
            ])
            .build(SqliteQueryBuilder);
        bind(sqlx::query(&sql), values).execute(&self.pool).await?;

        let token = self.create_session(id).await?;
        Ok((
            UserSummary {
                id,
                username,
                display_name,
            },
            token,
        ))
    }

    pub async fn login(
        &self,
        username: &str,
        password: &str,
    ) -> std::result::Result<(UserSummary, String), AccountError> {
        let username = username.trim();
        let (sql, values) = Query::select()
            .columns([
                users::ID,
                users::USERNAME,
                users::DISPLAY_NAME,
                users::PASSWORD_HASH,
                users::BANNED,
            ])
            .from(users::TABLE)
            .and_where(Expr::col(users::USERNAME).eq(username))
            .build(SqliteQueryBuilder);
        let row = bind(sqlx::query(&sql), values)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AccountError::BadCredentials)?;

        let hash: String = row.get(3);
        verify_password(password, &hash)?;
        if row.get::<i64, _>(4) != 0 {
            return Err(AccountError::BadCredentials);
        }
        let user = UserSummary {
            id: row
                .get::<String, _>(0)
                .parse()
                .map_err(|e| AccountError::Storage(format!("用户标识无效: {e}")))?,
            username: row.get(1),
            display_name: row.get(2),
        };
        let token = self.create_session(user.id).await?;
        Ok((user, token))
    }

    pub async fn resume(
        &self,
        token: &str,
    ) -> std::result::Result<Option<UserSummary>, AccountError> {
        let now = now_secs();
        let (sql, values) = Query::select()
            .columns([
                (users::TABLE, users::ID),
                (users::TABLE, users::USERNAME),
                (users::TABLE, users::DISPLAY_NAME),
            ])
            .from(users::TABLE)
            .inner_join(
                sessions::TABLE,
                Expr::col((sessions::TABLE, sessions::USER_ID))
                    .eq(Expr::col((users::TABLE, users::ID))),
            )
            .and_where(Expr::col((sessions::TABLE, sessions::TOKEN)).eq(token))
            .and_where(Expr::col((sessions::TABLE, sessions::EXPIRES_AT)).gt(now))
            .and_where(Expr::col((users::TABLE, users::BANNED)).eq(0))
            .build(SqliteQueryBuilder);
        let row = bind(sqlx::query(&sql), values)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| {
            Ok::<_, AccountError>(UserSummary {
                id: row
                    .get::<String, _>(0)
                    .parse()
                    .map_err(|e| AccountError::Storage(format!("用户标识无效: {e}")))?,
                username: row.get(1),
                display_name: row.get(2),
            })
        })
        .transpose()
    }

    async fn create_session(&self, user: UserId) -> std::result::Result<String, AccountError> {
        let token = Uuid::new_v4().simple().to_string();
        let now = now_secs();
        let (sql, values) = Query::insert()
            .into_table(sessions::TABLE)
            .columns([
                sessions::TOKEN,
                sessions::USER_ID,
                sessions::CREATED_AT,
                sessions::EXPIRES_AT,
            ])
            .values_panic([
                token.clone().into(),
                user.to_string().into(),
                now.into(),
                (now + SESSION_TTL_SECS).into(),
            ])
            .build(SqliteQueryBuilder);
        bind(sqlx::query(&sql), values).execute(&self.pool).await?;
        Ok(token)
    }

    pub async fn add_friend(
        &self,
        user: UserId,
        username: &str,
    ) -> std::result::Result<Vec<UserSummary>, AccountError> {
        let username = username.trim();
        let (sql, values) = Query::select()
            .column(users::ID)
            .from(users::TABLE)
            .and_where(Expr::col(users::USERNAME).eq(username))
            .build(SqliteQueryBuilder);
        let row = bind(sqlx::query(&sql), values)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| AccountError::Invalid("找不到这个用户名".into()))?;
        let friend: UserId = row
            .get::<String, _>(0)
            .parse()
            .map_err(|e| AccountError::Storage(format!("用户标识无效: {e}")))?;
        if friend == user {
            return Err(AccountError::Invalid("不能添加自己为好友".into()));
        }

        let now = now_secs();
        for (a, b) in [(user, friend), (friend, user)] {
            let (sql, values) = Query::insert()
                .into_table(friends::TABLE)
                .columns([friends::USER_ID, friends::FRIEND_ID, friends::CREATED_AT])
                .values_panic([a.to_string().into(), b.to_string().into(), now.into()])
                .on_conflict(
                    sea_query::OnConflict::columns([friends::USER_ID, friends::FRIEND_ID])
                        .do_nothing()
                        .to_owned(),
                )
                .build(SqliteQueryBuilder);
            bind(sqlx::query(&sql), values).execute(&self.pool).await?;
        }
        self.friends(user).await
    }

    pub async fn friends(
        &self,
        user: UserId,
    ) -> std::result::Result<Vec<UserSummary>, AccountError> {
        let (sql, values) = Query::select()
            .columns([
                (users::TABLE, users::ID),
                (users::TABLE, users::USERNAME),
                (users::TABLE, users::DISPLAY_NAME),
            ])
            .from(friends::TABLE)
            .inner_join(
                users::TABLE,
                Expr::col((users::TABLE, users::ID))
                    .eq(Expr::col((friends::TABLE, friends::FRIEND_ID))),
            )
            .and_where(Expr::col((friends::TABLE, friends::USER_ID)).eq(user.to_string()))
            .order_by((users::TABLE, users::DISPLAY_NAME), sea_query::Order::Asc)
            .build(SqliteQueryBuilder);
        let rows = bind(sqlx::query(&sql), values)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_to_user).collect()
    }

    pub async fn remove_friend(
        &self,
        user: UserId,
        friend: UserId,
    ) -> std::result::Result<(), AccountError> {
        for (a, b) in [(user, friend), (friend, user)] {
            let (sql, values) = Query::delete()
                .from_table(friends::TABLE)
                .and_where(Expr::col(friends::USER_ID).eq(a.to_string()))
                .and_where(Expr::col(friends::FRIEND_ID).eq(b.to_string()))
                .build(SqliteQueryBuilder);
            bind(sqlx::query(&sql), values).execute(&self.pool).await?;
        }
        Ok(())
    }

    pub async fn find_user(
        &self,
        username: &str,
    ) -> std::result::Result<Option<UserSummary>, AccountError> {
        let (sql, values) = Query::select()
            .columns([
                (users::TABLE, users::ID),
                (users::TABLE, users::USERNAME),
                (users::TABLE, users::DISPLAY_NAME),
            ])
            .from(users::TABLE)
            .and_where(Expr::col(users::USERNAME).eq(username.trim().to_ascii_lowercase()))
            .build(SqliteQueryBuilder);
        let row = bind(sqlx::query(&sql), values)
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_to_user).transpose()
    }

    /// 按用户名或昵称搜索用户，排除自己，最多 20 条。
    pub async fn search_users(
        &self,
        user: UserId,
        query: &str,
    ) -> std::result::Result<Vec<UserSummary>, AccountError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let pattern = format!("%{query}%");
        let (sql, values) = Query::select()
            .columns([
                (users::TABLE, users::ID),
                (users::TABLE, users::USERNAME),
                (users::TABLE, users::DISPLAY_NAME),
            ])
            .from(users::TABLE)
            .and_where(Expr::col((users::TABLE, users::ID)).ne(user.to_string()))
            .and_where(
                Expr::col((users::TABLE, users::USERNAME))
                    .like(&pattern)
                    .or(Expr::col((users::TABLE, users::DISPLAY_NAME)).like(&pattern)),
            )
            .limit(20)
            .build(SqliteQueryBuilder);
        let rows = bind(sqlx::query(&sql), values)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_to_user).collect()
    }

    pub async fn create_group(
        &self,
        owner: UserId,
        name: &str,
        members: &[UserId],
    ) -> std::result::Result<RoomSummary, AccountError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(AccountError::Invalid("群名称不能为空".into()));
        }
        let name: String = name.chars().take(40).collect();
        let mut member_ids: Vec<UserId> = members
            .iter()
            .copied()
            .filter(|member| *member != owner)
            .collect();
        member_ids.sort_by_key(|member| member.to_string());
        member_ids.dedup();

        // 只允许把好友拉进群
        let known: std::collections::HashSet<UserId> = self
            .friends(owner)
            .await?
            .into_iter()
            .map(|friend| friend.id)
            .collect();
        if member_ids.iter().any(|member| !known.contains(member)) {
            return Err(AccountError::Invalid("只能把好友拉进群聊".into()));
        }

        let room = RoomId::new();
        let now = now_secs();
        let (sql, values) = Query::insert()
            .into_table(rooms::TABLE)
            .columns([
                rooms::ID,
                rooms::KIND,
                rooms::NAME,
                rooms::OWNER_ID,
                rooms::CREATED_AT,
            ])
            .values_panic([
                room.to_string().into(),
                "group".into(),
                name.clone().into(),
                owner.to_string().into(),
                now.into(),
            ])
            .build(SqliteQueryBuilder);
        bind(sqlx::query(&sql), values).execute(&self.pool).await?;

        for member in std::iter::once(owner).chain(member_ids.iter().copied()) {
            let role = if member == owner { "owner" } else { "member" };
            self.insert_member(room, member, role, now).await?;
        }

        self.room_summary(room, owner)
            .await?
            .ok_or_else(|| AccountError::Storage("群聊创建后读不回来".into()))
    }

    pub async fn open_direct(
        &self,
        a: UserId,
        b: UserId,
    ) -> std::result::Result<RoomSummary, AccountError> {
        if a == b {
            return Err(AccountError::Invalid("不能和自己开单聊".into()));
        }
        let key = direct_key(a, b);
        if let Some(room) = self.room_by_key(&key).await? {
            return self
                .room_summary(room, a)
                .await?
                .ok_or_else(|| AccountError::Storage("单聊房间缺少成员".into()));
        }

        // 只允许和好友单聊
        if !self.are_friends(a, b).await? {
            return Err(AccountError::Invalid("只能和好友单聊".into()));
        }

        let room = RoomId::new();
        let now = now_secs();
        let (sql, values) = Query::insert()
            .into_table(rooms::TABLE)
            .columns([
                rooms::ID,
                rooms::KIND,
                rooms::NAME,
                rooms::DIRECT_KEY,
                rooms::CREATED_AT,
            ])
            .values_panic([
                room.to_string().into(),
                "direct".into(),
                "".into(),
                key.into(),
                now.into(),
            ])
            .build(SqliteQueryBuilder);
        bind(sqlx::query(&sql), values).execute(&self.pool).await?;
        self.insert_member(room, a, "member", now).await?;
        self.insert_member(room, b, "member", now).await?;

        self.room_summary(room, a)
            .await?
            .ok_or_else(|| AccountError::Storage("单聊创建后读不回来".into()))
    }

    pub async fn rooms(&self, user: UserId) -> std::result::Result<Vec<RoomSummary>, AccountError> {
        let (sql, values) = Query::select()
            .columns([
                (rooms::TABLE, rooms::ID),
                (rooms::TABLE, rooms::KIND),
                (rooms::TABLE, rooms::NAME),
            ])
            .from(room_members::TABLE)
            .inner_join(
                rooms::TABLE,
                Expr::col((rooms::TABLE, rooms::ID))
                    .eq(Expr::col((room_members::TABLE, room_members::ROOM_ID))),
            )
            .and_where(Expr::col((room_members::TABLE, room_members::USER_ID)).eq(user.to_string()))
            .build(SqliteQueryBuilder);
        let rows = bind(sqlx::query(&sql), values)
            .fetch_all(&self.pool)
            .await?;

        let mut result = Vec::with_capacity(rows.len());
        for row in rows {
            let room: RoomId = row
                .get::<String, _>(0)
                .parse()
                .map_err(|e| AccountError::Storage(format!("房间标识无效: {e}")))?;
            let kind = match row.get::<String, _>(1).as_str() {
                "direct" => RoomKind::Direct,
                _ => RoomKind::Group,
            };
            let name = if kind == RoomKind::Direct {
                self.direct_display_name(room, user).await?
            } else {
                row.get::<String, _>(2)
            };
            let member_count = self.member_count(room).await?;
            result.push(RoomSummary {
                room,
                kind,
                name,
                member_count,
                last_seq: 0,
                last_text: None,
                last_at: None,
                read_seq: 0,
            });
        }
        Ok(result)
    }

    /// 房间名；匿名大厅摘要与地址簿探测用，不校验成员。
    pub async fn room_name(
        &self,
        room: RoomId,
    ) -> std::result::Result<Option<String>, AccountError> {
        let (sql, values) = Query::select()
            .column(rooms::NAME)
            .from(rooms::TABLE)
            .and_where(Expr::col(rooms::ID).eq(room.to_string()))
            .build(SqliteQueryBuilder);
        let row = bind(sqlx::query(&sql), values)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| row.get::<String, _>(0)))
    }

    /// 只查默认房间，不存在时返回 `None`，不会建库或建表。
    pub async fn find_default_room(&self) -> std::result::Result<Option<RoomId>, AccountError> {
        let (sql, values) = Query::select()
            .column(rooms::ID)
            .from(rooms::TABLE)
            .and_where(Expr::col(rooms::KIND).eq("group"))
            .and_where(Expr::col(rooms::OWNER_ID).is_null())
            .limit(1)
            .build(SqliteQueryBuilder);
        let row = bind(sqlx::query(&sql), values)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| {
            row.get::<String, _>(0)
                .parse()
                .map_err(|e| AccountError::Storage(format!("房间标识无效: {e}")))
        })
        .transpose()
    }

    /// 系统默认房间：所有登录账号都能看到并进入，局域网托管也用它。
    pub async fn ensure_default_room(
        &self,
        name: &str,
    ) -> std::result::Result<RoomId, AccountError> {
        let (sql, values) = Query::select()
            .column(rooms::ID)
            .from(rooms::TABLE)
            .and_where(Expr::col(rooms::KIND).eq("group"))
            .and_where(Expr::col(rooms::OWNER_ID).is_null())
            .limit(1)
            .build(SqliteQueryBuilder);
        if let Some(row) = bind(sqlx::query(&sql), values)
            .fetch_optional(&self.pool)
            .await?
        {
            return row
                .get::<String, _>(0)
                .parse()
                .map_err(|e| AccountError::Storage(format!("房间标识无效: {e}")));
        }

        let room = RoomId::new();
        let now = now_secs();
        let (sql, values) = Query::insert()
            .into_table(rooms::TABLE)
            .columns([rooms::ID, rooms::KIND, rooms::NAME, rooms::CREATED_AT])
            .values_panic([
                room.to_string().into(),
                "group".into(),
                name.chars().take(40).collect::<String>().into(),
                now.into(),
            ])
            .build(SqliteQueryBuilder);
        bind(sqlx::query(&sql), values).execute(&self.pool).await?;
        Ok(room)
    }

    /// 确保用户是房间成员（登录后自动进入默认房间）。
    pub async fn ensure_member(
        &self,
        room: RoomId,
        user: UserId,
    ) -> std::result::Result<(), AccountError> {
        self.insert_member(room, user, "member", now_secs()).await
    }

    /// 成员与角色，按加入顺序。
    pub async fn members_with_roles(
        &self,
        room: RoomId,
    ) -> std::result::Result<Vec<RoomMember>, AccountError> {
        let (sql, values) = Query::select()
            .columns([
                (users::TABLE, users::ID),
                (users::TABLE, users::USERNAME),
                (users::TABLE, users::DISPLAY_NAME),
                (room_members::TABLE, room_members::ROLE),
            ])
            .from(room_members::TABLE)
            .inner_join(
                users::TABLE,
                Expr::col((users::TABLE, users::ID))
                    .eq(Expr::col((room_members::TABLE, room_members::USER_ID))),
            )
            .and_where(Expr::col((room_members::TABLE, room_members::ROOM_ID)).eq(room.to_string()))
            .order_by(
                (room_members::TABLE, room_members::JOINED_AT),
                sea_query::Order::Asc,
            )
            .build(SqliteQueryBuilder);
        let rows = bind(sqlx::query(&sql), values)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                let role = match row.get::<String, _>(3).as_str() {
                    "owner" => MemberRole::Owner,
                    _ => MemberRole::Member,
                };
                Ok(RoomMember {
                    user: row_to_user(row)?,
                    role,
                })
            })
            .collect()
    }

    pub async fn member_role(
        &self,
        room: RoomId,
        user: UserId,
    ) -> std::result::Result<Option<MemberRole>, AccountError> {
        let (sql, values) = Query::select()
            .column(room_members::ROLE)
            .from(room_members::TABLE)
            .and_where(Expr::col(room_members::ROOM_ID).eq(room.to_string()))
            .and_where(Expr::col(room_members::USER_ID).eq(user.to_string()))
            .build(SqliteQueryBuilder);
        let row = bind(sqlx::query(&sql), values)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| match row.get::<String, _>(0).as_str() {
            "owner" => MemberRole::Owner,
            _ => MemberRole::Member,
        }))
    }

    pub async fn is_owner(
        &self,
        room: RoomId,
        user: UserId,
    ) -> std::result::Result<bool, AccountError> {
        Ok(self.member_role(room, user).await? == Some(MemberRole::Owner))
    }

    pub async fn remove_member(
        &self,
        room: RoomId,
        user: UserId,
    ) -> std::result::Result<(), AccountError> {
        let (sql, values) = Query::delete()
            .from_table(room_members::TABLE)
            .and_where(Expr::col(room_members::ROOM_ID).eq(room.to_string()))
            .and_where(Expr::col(room_members::USER_ID).eq(user.to_string()))
            .build(SqliteQueryBuilder);
        bind(sqlx::query(&sql), values).execute(&self.pool).await?;
        Ok(())
    }

    /// 解散群聊：删除成员与房间记录，消息按房间保留在消息库中。
    pub async fn dissolve_group(&self, room: RoomId) -> std::result::Result<(), AccountError> {
        let (sql, values) = Query::delete()
            .from_table(room_members::TABLE)
            .and_where(Expr::col(room_members::ROOM_ID).eq(room.to_string()))
            .build(SqliteQueryBuilder);
        bind(sqlx::query(&sql), values).execute(&self.pool).await?;
        let (sql, values) = Query::delete()
            .from_table(rooms::TABLE)
            .and_where(Expr::col(rooms::ID).eq(room.to_string()))
            .build(SqliteQueryBuilder);
        bind(sqlx::query(&sql), values).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn read_seq(
        &self,
        user: UserId,
        room: RoomId,
    ) -> std::result::Result<Option<Seq>, AccountError> {
        let (sql, values) = Query::select()
            .column(read_markers::SEQ)
            .from(read_markers::TABLE)
            .and_where(Expr::col(read_markers::USER_ID).eq(user.to_string()))
            .and_where(Expr::col(read_markers::ROOM_ID).eq(room.to_string()))
            .build(SqliteQueryBuilder);
        let row = bind(sqlx::query(&sql), values)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| row.get(0)))
    }

    pub async fn set_read_seq(
        &self,
        user: UserId,
        room: RoomId,
        seq: Seq,
    ) -> std::result::Result<(), AccountError> {
        let (sql, values) = Query::insert()
            .into_table(read_markers::TABLE)
            .columns([
                read_markers::USER_ID,
                read_markers::ROOM_ID,
                read_markers::SEQ,
                read_markers::UPDATED_AT,
            ])
            .values_panic([
                user.to_string().into(),
                room.to_string().into(),
                seq.into(),
                now_secs().into(),
            ])
            .on_conflict(
                sea_query::OnConflict::columns([read_markers::USER_ID, read_markers::ROOM_ID])
                    .update_columns([read_markers::SEQ, read_markers::UPDATED_AT])
                    .to_owned(),
            )
            .build(SqliteQueryBuilder);
        bind(sqlx::query(&sql), values).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn is_member(
        &self,
        room: RoomId,
        user: UserId,
    ) -> std::result::Result<bool, AccountError> {
        let (sql, values) = Query::select()
            .expr(Expr::col(room_members::USER_ID).count())
            .from(room_members::TABLE)
            .and_where(Expr::col(room_members::ROOM_ID).eq(room.to_string()))
            .and_where(Expr::col(room_members::USER_ID).eq(user.to_string()))
            .build(SqliteQueryBuilder);
        let row = bind(sqlx::query(&sql), values)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>(0) > 0)
    }

    pub async fn members(
        &self,
        room: RoomId,
    ) -> std::result::Result<Vec<UserSummary>, AccountError> {
        let (sql, values) = Query::select()
            .columns([
                (users::TABLE, users::ID),
                (users::TABLE, users::USERNAME),
                (users::TABLE, users::DISPLAY_NAME),
            ])
            .from(room_members::TABLE)
            .inner_join(
                users::TABLE,
                Expr::col((users::TABLE, users::ID))
                    .eq(Expr::col((room_members::TABLE, room_members::USER_ID))),
            )
            .and_where(Expr::col((room_members::TABLE, room_members::ROOM_ID)).eq(room.to_string()))
            .order_by(
                (room_members::TABLE, room_members::JOINED_AT),
                sea_query::Order::Asc,
            )
            .build(SqliteQueryBuilder);
        let rows = bind(sqlx::query(&sql), values)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_to_user).collect()
    }

    pub async fn room_summary(
        &self,
        room: RoomId,
        viewer: UserId,
    ) -> std::result::Result<Option<RoomSummary>, AccountError> {
        let (sql, values) = Query::select()
            .columns([rooms::ID, rooms::KIND, rooms::NAME])
            .from(rooms::TABLE)
            .and_where(Expr::col(rooms::ID).eq(room.to_string()))
            .build(SqliteQueryBuilder);
        let Some(row) = bind(sqlx::query(&sql), values)
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        if !self.is_member(room, viewer).await? {
            return Ok(None);
        }
        let kind = match row.get::<String, _>(1).as_str() {
            "direct" => RoomKind::Direct,
            _ => RoomKind::Group,
        };
        let name = row.get::<String, _>(2);
        let case = if kind == RoomKind::Direct {
            self.direct_display_name(room, viewer).await?
        } else {
            name
        };
        Ok(Some(RoomSummary {
            room,
            kind,
            name: case,
            member_count: self.member_count(room).await?,
            last_seq: 0,
            last_text: None,
            last_at: None,
            read_seq: 0,
        }))
    }

    /// 单聊在列表里显示对方的名字；群聊用群名。
    async fn direct_display_name(
        &self,
        room: RoomId,
        viewer: UserId,
    ) -> std::result::Result<String, AccountError> {
        let members = self.members(room).await?;
        Ok(members
            .into_iter()
            .find(|member| member.id != viewer)
            .map(|member| member.display_name)
            .unwrap_or_else(|| "单聊".into()))
    }

    async fn member_count(&self, room: RoomId) -> std::result::Result<u32, AccountError> {
        let (sql, values) = Query::select()
            .expr(Expr::col(room_members::USER_ID).count())
            .from(room_members::TABLE)
            .and_where(Expr::col(room_members::ROOM_ID).eq(room.to_string()))
            .build(SqliteQueryBuilder);
        let row = bind(sqlx::query(&sql), values)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>(0).max(0) as u32)
    }

    async fn insert_member(
        &self,
        room: RoomId,
        user: UserId,
        role: &str,
        joined_at: i64,
    ) -> std::result::Result<(), AccountError> {
        let (sql, values) = Query::insert()
            .into_table(room_members::TABLE)
            .columns([
                room_members::ROOM_ID,
                room_members::USER_ID,
                room_members::ROLE,
                room_members::JOINED_AT,
            ])
            .values_panic([
                room.to_string().into(),
                user.to_string().into(),
                role.into(),
                joined_at.into(),
            ])
            .on_conflict(
                sea_query::OnConflict::columns([room_members::ROOM_ID, room_members::USER_ID])
                    .do_nothing()
                    .to_owned(),
            )
            .build(SqliteQueryBuilder);
        bind(sqlx::query(&sql), values).execute(&self.pool).await?;
        Ok(())
    }

    async fn room_by_key(&self, key: &str) -> std::result::Result<Option<RoomId>, AccountError> {
        let (sql, values) = Query::select()
            .column(rooms::ID)
            .from(rooms::TABLE)
            .and_where(Expr::col(rooms::DIRECT_KEY).eq(key))
            .build(SqliteQueryBuilder);
        let row = bind(sqlx::query(&sql), values)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| {
            row.get::<String, _>(0)
                .parse()
                .map_err(|e| AccountError::Storage(format!("房间标识无效: {e}")))
        })
        .transpose()
    }

    async fn are_friends(&self, a: UserId, b: UserId) -> std::result::Result<bool, AccountError> {
        let (sql, values) = Query::select()
            .expr(Expr::col(friends::FRIEND_ID).count())
            .from(friends::TABLE)
            .and_where(Expr::col(friends::USER_ID).eq(a.to_string()))
            .and_where(Expr::col(friends::FRIEND_ID).eq(b.to_string()))
            .build(SqliteQueryBuilder);
        let row = bind(sqlx::query(&sql), values)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>(0) > 0)
    }
}

fn normalize_username(raw: &str) -> std::result::Result<String, AccountError> {
    let username = raw.trim();
    if username.len() < 3 || username.len() > 32 {
        return Err(AccountError::Invalid("用户名长度应为 3-32 个字符".into()));
    }
    if !username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(AccountError::Invalid(
            "用户名只能包含字母、数字和下划线".into(),
        ));
    }
    Ok(username.to_ascii_lowercase())
}

fn normalize_display_name(raw: &str, username: &str) -> String {
    let name = raw.trim();
    if name.is_empty() {
        username.to_string()
    } else {
        name.chars().take(24).collect()
    }
}

fn validate_password(password: &str) -> std::result::Result<(), AccountError> {
    if password.chars().count() < 8 {
        return Err(AccountError::Invalid("密码至少 8 位".into()));
    }
    Ok(())
}

fn hash_password(password: &str) -> std::result::Result<String, AccountError> {
    let salt = SaltString::encode_b64(Uuid::new_v4().as_bytes())
        .map_err(|e| AccountError::Storage(format!("生成盐失败: {e}")))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| AccountError::Storage(format!("密码哈希失败: {e}")))
}

fn verify_password(password: &str, hash: &str) -> std::result::Result<(), AccountError> {
    let parsed =
        PasswordHash::new(hash).map_err(|e| AccountError::Storage(format!("密码哈希无效: {e}")))?;
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| AccountError::BadCredentials)
}

fn direct_key(a: UserId, b: UserId) -> String {
    let (a, b) = (a.to_string(), b.to_string());
    if a < b {
        format!("{a}:{b}")
    } else {
        format!("{b}:{a}")
    }
}

fn row_to_user(row: sqlx::sqlite::SqliteRow) -> std::result::Result<UserSummary, AccountError> {
    Ok(UserSummary {
        id: row
            .get::<String, _>(0)
            .parse()
            .map_err(|e| AccountError::Storage(format!("用户标识无效: {e}")))?,
        username: row.get(1),
        display_name: row.get(2),
    })
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn bind<'q>(
    query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    values: sea_query::Values,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    let mut query = query;
    for value in values.0 {
        query = match value {
            sea_query::Value::String(Some(text)) => query.bind(*text),
            sea_query::Value::BigInt(Some(number)) => query.bind(number),
            sea_query::Value::BigUnsigned(Some(number)) => query.bind(number as i64),
            sea_query::Value::Int(Some(number)) => query.bind(number as i64),
            sea_query::Value::Unsigned(Some(number)) => query.bind(number as i64),
            _ => query.bind(Option::<String>::None),
        };
    }
    query
}

impl Accounts {
    /// 管理命令：封禁/解封账号。
    pub async fn set_banned(
        &self,
        username: &str,
        banned: bool,
    ) -> std::result::Result<bool, AccountError> {
        let (sql, values) = Query::update()
            .table(users::TABLE)
            .value(users::BANNED, if banned { 1 } else { 0 })
            .and_where(Expr::col(users::USERNAME).eq(username.trim().to_ascii_lowercase()))
            .build(SqliteQueryBuilder);
        let result = bind(sqlx::query(&sql), values).execute(&self.pool).await?;
        Ok(result.rows_affected() > 0)
    }

    /// 管理命令：所有用户。
    pub async fn admin_users(&self) -> std::result::Result<Vec<UserSummary>, AccountError> {
        let (sql, values) = Query::select()
            .columns([
                (users::TABLE, users::ID),
                (users::TABLE, users::USERNAME),
                (users::TABLE, users::DISPLAY_NAME),
            ])
            .from(users::TABLE)
            .order_by((users::TABLE, users::USERNAME), sea_query::Order::Asc)
            .build(SqliteQueryBuilder);
        let rows = bind(sqlx::query(&sql), values)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_to_user).collect()
    }

    /// 管理命令：所有房间。
    pub async fn admin_rooms(&self) -> std::result::Result<Vec<RoomSummary>, AccountError> {
        let (sql, values) = Query::select()
            .columns([
                (rooms::TABLE, rooms::ID),
                (rooms::TABLE, rooms::KIND),
                (rooms::TABLE, rooms::NAME),
            ])
            .from(rooms::TABLE)
            .order_by((rooms::TABLE, rooms::CREATED_AT), sea_query::Order::Asc)
            .build(SqliteQueryBuilder);
        let rows = bind(sqlx::query(&sql), values)
            .fetch_all(&self.pool)
            .await?;
        let mut result = Vec::with_capacity(rows.len());
        for row in rows {
            let room: RoomId = row
                .get::<String, _>(0)
                .parse()
                .map_err(|e| AccountError::Storage(format!("房间标识无效: {e}")))?;
            let kind = match row.get::<String, _>(1).as_str() {
                "direct" => RoomKind::Direct,
                _ => RoomKind::Group,
            };
            result.push(RoomSummary {
                room,
                kind,
                name: row.get(2),
                member_count: self.member_count(room).await?,
                last_seq: 0,
                last_text: None,
                last_at: None,
                read_seq: 0,
            });
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn accounts(tag: &str) -> (Accounts, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("libchat-accounts-{tag}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        (Accounts::open(&path).await.unwrap(), path)
    }

    #[tokio::test]
    async fn registers_logs_in_and_resumes() {
        let (accounts, path) = accounts("auth").await;
        let (alice, token) = accounts
            .register("Alice", "password123", "爱丽丝")
            .await
            .unwrap();
        assert_eq!(alice.username, "alice");
        assert_eq!(alice.display_name, "爱丽丝");
        assert!(accounts.register("alice", "password123", "").await.is_err());
        assert!(accounts.login("alice", "wrongpass").await.is_err());
        assert_eq!(
            accounts.resume(&token).await.unwrap().map(|user| user.id),
            Some(alice.id)
        );
        let (again, _) = accounts.login("alice", "password123").await.unwrap();
        assert_eq!(again.id, alice.id);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn friends_are_bidirectional_and_groups_validate_membership() {
        let (accounts, path) = accounts("friends").await;
        let (alice, _) = accounts.register("alice", "password123", "").await.unwrap();
        let (bob, _) = accounts.register("bob", "password123", "").await.unwrap();
        let (carol, _) = accounts.register("carol", "password123", "").await.unwrap();

        accounts.add_friend(alice.id, "bob").await.unwrap();
        assert_eq!(accounts.friends(alice.id).await.unwrap().len(), 1);
        assert_eq!(accounts.friends(bob.id).await.unwrap().len(), 1);

        // 群聊只能拉好友
        assert!(accounts
            .create_group(alice.id, "项目群", &[carol.id])
            .await
            .is_err());
        let group = accounts
            .create_group(alice.id, "项目群", &[bob.id])
            .await
            .unwrap();
        assert_eq!(group.member_count, 2);
        assert_eq!(accounts.members(group.room).await.unwrap().len(), 2);
        assert!(accounts.is_member(group.room, alice.id).await.unwrap());
        assert!(!accounts.is_member(group.room, carol.id).await.unwrap());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn direct_rooms_are_reused() {
        let (accounts, path) = accounts("direct").await;
        let (alice, _) = accounts.register("alice", "password123", "").await.unwrap();
        let (bob, _) = accounts.register("bob", "password123", "").await.unwrap();
        accounts.add_friend(alice.id, "bob").await.unwrap();

        let first = accounts.open_direct(alice.id, bob.id).await.unwrap();
        let second = accounts.open_direct(bob.id, alice.id).await.unwrap();
        assert_eq!(first.room, second.room);
        assert_eq!(first.kind, RoomKind::Direct);
        assert_eq!(second.name, "alice");
        assert_eq!(accounts.rooms(alice.id).await.unwrap().len(), 1);
        let _ = std::fs::remove_file(path);
    }
}
