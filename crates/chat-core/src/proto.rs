//! 客户端与服务端共享的协议定义。与具体传输方式无关。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 会话内单调递增的消息序号，由服务端分配，构成全序。
pub type Seq = i64;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize,
        )]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::parse_str(s)?))
            }
        }
    };
}

id_type!(UserId);
id_type!(DeviceId);
id_type!(RoomId);
id_type!(AttachmentId);

/// 单个附件的上限。超过就拒绝，避免一次传输把房间拖死。
pub const MAX_ATTACHMENT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Text,
    File,
    System,
}

/// 附件元数据。内容本身不随消息下发，按 `id` 单独取。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: AttachmentId,
    /// 原始文件名，仅用于展示与保存时的默认名。
    pub name: String,
    pub size: u64,
    /// 按扩展名猜测的 MIME；猜不出时用 `application/octet-stream`。
    pub mime: String,
    /// 内容 SHA-256（十六进制小写），用于校验与去重。
    pub sha256: String,
}

/// 被引用的消息摘要：只保留展示所需的最少信息，不递归展开引用链。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quote {
    pub sender: String,
    /// 文本消息为正文，文件消息为文件名。
    pub text: String,
    /// 被引用消息的序号，便于定位与去重。
    pub seq: Option<Seq>,
}

/// 会话类型：群聊或一对一单聊。单聊复用房间与消息模型。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomKind {
    Group,
    Direct,
}

/// 账号的公开信息，绝不包含密码或令牌。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserSummary {
    pub id: UserId,
    pub username: String,
    pub display_name: String,
}

/// 会话列表里的一行。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomSummary {
    pub room: RoomId,
    pub kind: RoomKind,
    pub name: String,
    pub member_count: u32,
    /// 房间当前最新序号，客户端据此判断是否有新消息。
    pub last_seq: Seq,
    /// 最后一条消息摘要；空会话为 `None`。群聊带发送者名。
    pub last_text: Option<String>,
    /// 最后活动时间（Unix 秒），空会话为 `None`。
    pub last_at: Option<i64>,
    /// 当前账号在这个房间已读到的最新序号，由服务端持久化。
    pub read_seq: Seq,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberRole {
    Owner,
    Member,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomMember {
    pub user: UserSummary,
    pub role: MemberRole,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub room: RoomId,
    pub seq: Seq,
    pub client_msg_id: Uuid,
    pub sender: UserId,
    pub sender_name: String,
    pub kind: MessageKind,
    /// 文本内容；文件消息为空串。
    pub text: String,
    /// 文件消息附带元数据。
    pub attachment: Option<Attachment>,
    /// 这条消息引用了哪条消息；没有引用为 `None`。
    pub quote: Option<Quote>,
    pub server_ts_ms: i64,
}

/// 大厅摘要：未加入房间者也能获取，不含任何消息内容。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LobbyInfo {
    pub room: RoomId,
    pub name: String,
    /// 当前在线人数。房间没有「总人数」的概念：连接断开即离开。
    pub online_count: u32,
}

/// 服务端当前托管的房间信息，握手时下发给客户端。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomInfo {
    pub room: RoomId,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientRequest {
    // ---------------------------------------------------------------- 账号
    /// 注册并发下登录令牌。用户名唯一，密码至少 8 位。
    Register {
        username: String,
        password: String,
        display_name: String,
    },
    Login {
        username: String,
        password: String,
    },
    /// 用本地保存的令牌恢复登录，避免每次启动都输密码。
    Resume {
        token: String,
    },
    Logout,

    // ---------------------------------------------------------------- 好友
    ListFriends,
    /// 按用户名添加好友；双方直接建立关系，MVP 不做申请审批。
    AddFriend {
        username: String,
    },
    /// 解除好友关系，双方列表同时移除。
    RemoveFriend {
        user: UserId,
    },
    /// 按用户名或昵称搜索用户，最多 20 条。
    SearchUsers {
        query: String,
    },
    /// 在当前房间内按正文搜索消息，最多 50 条。
    SearchMessages {
        room: RoomId,
        query: String,
    },
    /// 撤回自己发送的一条消息。
    RecallMessage {
        room: RoomId,
        seq: Seq,
    },
    /// 编辑自己发送的文本消息。
    EditMessage {
        room: RoomId,
        seq: Seq,
        text: String,
    },
    /// 更新当前账号在房间里的已读位置。
    MarkRead {
        room: RoomId,
        seq: Seq,
    },

    // ---------------------------------------------------------------- 房间
    /// 我加入的所有会话（群聊 + 单聊）。
    ListRooms,
    /// 创建群聊并把选中的好友直接拉进来。
    CreateGroup {
        name: String,
        members: Vec<UserId>,
    },
    /// 打开与某人的单聊；已有就直接复用。
    OpenDirect {
        user: UserId,
    },
    /// 查询房间成员。
    RoomMembers {
        room: RoomId,
    },
    /// 主动退出群聊；群主需先解散。
    LeaveGroup {
        room: RoomId,
    },
    /// 群主把成员移出群聊。
    RemoveMember {
        room: RoomId,
        user: UserId,
    },
    /// 群主解散群聊。
    DissolveGroup {
        room: RoomId,
    },

    // ---------------------------------------------------------------- 房间内
    /// 查询大厅摘要，无需加入。
    Lobby {
        room: RoomId,
    },
    Join {
        room: RoomId,
        profile: Profile,
    },
    Leave {
        room: RoomId,
    },
    Send {
        room: RoomId,
        client_msg_id: Uuid,
        text: String,
        /// 引用某条消息时带上摘要；普通消息为 `None`。
        quote: Option<Quote>,
    },
    /// 文件消息：附件内容必须已经上传完成，服务端只校验并广播元数据。
    SendFile {
        room: RoomId,
        client_msg_id: Uuid,
        attachment: Attachment,
    },
    History {
        room: RoomId,
        before: Option<Seq>,
        limit: u32,
    },
    /// 补齐缺口：取 `after` 之后的增量，用于重连后追平断线期间的消息。
    Sync {
        room: RoomId,
        after: Seq,
        limit: u32,
    },
}

// 消息事件本来就带着整条消息，装箱只会给每条消息多一次分配；
// 其余变体很小，内联更划算。
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    /// 登录/注册/恢复成功；`token` 由客户端保存，下次用 `Resume`。
    Authenticated {
        user: UserSummary,
        token: String,
    },
    /// 当前连接还没有登录，除注册/登录外的请求都会被拒绝。
    AuthRequired,
    Friends {
        users: Vec<UserSummary>,
    },
    /// 用户搜索结果。
    UserSearch {
        users: Vec<UserSummary>,
    },
    /// 房间消息搜索结果。
    SearchResults {
        room: RoomId,
        messages: Vec<Message>,
    },
    /// 某条消息被撤回，客户端应从列表移除。
    MessageRecalled {
        room: RoomId,
        seq: Seq,
    },
    /// 某条消息被编辑。
    MessageEdited {
        message: Message,
    },
    /// 有人在消息里 @ 了当前用户。
    Mention {
        room: RoomId,
        from: UserSummary,
        text: String,
    },
    /// 好友在线状态变化。
    FriendPresence {
        user: UserId,
        online: bool,
    },
    Rooms {
        rooms: Vec<RoomSummary>,
    },
    RoomMembers {
        room: RoomId,
        members: Vec<RoomMember>,
    },
    Lobby {
        info: LobbyInfo,
    },
    Joined {
        room: RoomId,
        you: UserId,
        online_count: u32,
        latest_seq: Seq,
    },
    Left {
        room: RoomId,
    },
    Message {
        message: Message,
    },
    History {
        room: RoomId,
        messages: Vec<Message>,
        has_more: bool,
    },
    /// [`ClientRequest::Sync`] 的应答，按 seq 升序。`has_more` 表示之后还有更新。
    Sync {
        room: RoomId,
        messages: Vec<Message>,
        has_more: bool,
    },
    Presence {
        room: RoomId,
        online_count: u32,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotJoined,
    AlreadyJoined,
    RoomMismatch,
    InvalidRequest,
    /// 未登录或令牌失效。
    Unauthorized,
    /// 用户名已存在、重复添加好友等。
    Conflict,
    /// 当前账号没有权限执行该操作。
    Forbidden,
    /// 附件超过 [`MAX_ATTACHMENT_BYTES`]。
    TooLarge,
    /// 附件不存在或尚未上传完成。
    NotFound,
    Internal,
}
