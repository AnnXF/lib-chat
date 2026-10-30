//! 核心契约：协议 DTO、端口 trait、客户端门面。
//!
//! 上下层只依赖这里，不依赖任何具体传输或存储实现；换传输 / 换存储都不动这一层。
//!
//! - `proto`：客户端与服务端共享的协议定义
//! - 端口：`Connection` / `RequestSink` / `ConnectionAcceptor` / `MessageStore`
//! - [`Client`]：客户端门面，把 `Connection` 收成带类型的方法

mod attachment;
mod client;
mod proto;

pub use attachment::{guess_mime, sha256_hex, validate_attachment};
pub use client::Client;
pub use proto::*;

use std::sync::Arc;

use tokio::sync::mpsc;
use uuid::Uuid;

pub type Result<T, E = ChatError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("传输错误: {0}")]
    Transport(String),
    #[error("协议错误: {0}")]
    Protocol(String),
    #[error("存储错误: {0}")]
    Storage(String),
    #[error("连接已关闭")]
    Closed,
}

pub struct AppendResult {
    pub message: Message,
    pub inserted: bool,
}

// ---------------------------------------------------------------- 连接

/// 客户端与服务端之间的一条连接。
///
/// 实现可以是进程内直连、QUIC——上层无需感知差异。
/// 附件默认不支持：不实现时返回错误，而不是假装成功。
#[async_trait::async_trait]
pub trait Connection: Send + Sync {
    /// 发送请求。响应通过 [`Connection::next_event`] 异步返回。
    async fn request(&self, req: ClientRequest) -> Result<()>;
    /// 等待下一个服务端事件。
    async fn next_event(&self) -> Result<ServerEvent>;
    /// 主动断开连接。
    async fn close(&self) -> Result<()>;

    /// 上传附件内容。`attachment` 里的 `size` 与 `sha256` 必须与 `data` 一致。
    async fn upload(&self, _room: RoomId, _attachment: Attachment, _data: Vec<u8>) -> Result<()> {
        Err(ChatError::Protocol("该传输不支持附件".into()))
    }

    /// 下载附件内容。
    async fn download(&self, _room: RoomId, _id: AttachmentId) -> Result<Vec<u8>> {
        Err(ChatError::Protocol("该传输不支持附件".into()))
    }
}

/// 服务端向某条连接推送事件的口子。
///
/// 容量要能吸收一次突发广播：32 太浅，客户端只落后一帧就可能被判定为慢连接而清退。
pub const EVENT_QUEUE_CAPACITY: usize = 256;
pub type EventStream = mpsc::Receiver<ServerEvent>;

/// 一条连接的请求入口，传输层把收到的请求交给它。
///
/// 附件默认不支持：传输层据此拒绝附件流，而不是假装成功。
#[async_trait::async_trait]
pub trait RequestSink: Send + Sync {
    async fn dispatch(&self, req: ClientRequest) -> Result<()>;
    async fn detach(&self);

    /// 接收一个完整附件：实现方负责校验成员身份、大小与摘要，并落盘。
    async fn upload(&self, _room: RoomId, _attachment: Attachment, _data: Vec<u8>) -> Result<()> {
        Err(ChatError::Protocol("该传输不支持附件".into()))
    }

    /// 读取附件内容。仅本房间成员可取。
    async fn download(&self, _room: RoomId, _id: AttachmentId) -> Result<Vec<u8>> {
        Err(ChatError::Protocol("该传输不支持附件".into()))
    }
}

/// 服务端接入能力，供传输层在接受新连接时调用。
#[async_trait::async_trait]
pub trait ConnectionAcceptor: Send + Sync {
    /// 托管的房间，握手时下发给客户端，客户端据此发起 lobby/join。
    fn room_info(&self) -> RoomInfo;
    async fn accept(&self, device: DeviceId) -> (Arc<dyn RequestSink>, EventStream);
}

// ---------------------------------------------------------------- 存储

/// 一条待写入的新消息（尚未分配 seq）。
#[derive(Clone, Debug)]
pub struct NewMessage {
    pub client_msg_id: Uuid,
    pub sender: UserId,
    pub sender_name: String,
    pub kind: MessageKind,
    pub text: String,
    pub attachment: Option<Attachment>,
    pub quote: Option<Quote>,
}

/// 消息持久化端口。服务端负责分配会话内单调序号。
#[async_trait::async_trait]
pub trait MessageStore: Send + Sync {
    /// 追加消息，返回分配了 `seq` 的完整消息。
    async fn append(&self, room: RoomId, msg: NewMessage) -> Result<AppendResult>;
    /// 取 `before`（不含）之前的最近 `limit` 条，按 seq 升序返回。
    async fn history(
        &self,
        room: RoomId,
        before: Option<Seq>,
        limit: usize,
    ) -> Result<Vec<Message>>;
    /// 取 `after`（不含）之后的、最早的 `limit` 条，按 seq 升序返回。
    async fn since(&self, room: RoomId, after: Seq, limit: usize) -> Result<Vec<Message>>;
    /// 当前最大序号，空房间为 0。
    async fn latest_seq(&self, room: RoomId) -> Result<Seq>;
    /// 按正文搜索消息，最近的结果优先，最多 `limit` 条。
    async fn search(&self, room: RoomId, query: &str, limit: usize) -> Result<Vec<Message>>;
    /// 撤回自己发送的消息；成功返回是否真的删掉了。
    async fn recall(&self, room: RoomId, seq: Seq, sender: UserId) -> Result<bool>;
    /// 编辑自己发送的文本消息，返回更新后的消息。
    async fn edit(
        &self,
        room: RoomId,
        seq: Seq,
        sender: UserId,
        text: String,
    ) -> Result<Option<Message>>;
    /// 管理命令：清空一个房间的全部消息，返回删除条数。
    async fn clear(&self, room: RoomId) -> Result<u64>;
    /// 按附件标识查元数据；房间里没有这条附件时返回 `None`。
    async fn attachment(&self, room: RoomId, id: AttachmentId) -> Result<Option<Attachment>>;
}
