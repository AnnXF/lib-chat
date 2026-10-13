//! 界面持有的视图数据。
//!
//! 与协议 DTO 分开：这里只描述「界面要用到的形状」，不掺网络语义。

use chat_proto::RoomId;
#[derive(Clone)]
pub struct DiscoveredRoom {
    pub(crate) id: RoomId,
    pub(crate) name: String,
    pub(crate) addr: String,
    pub(crate) fingerprint: String,
    pub(crate) online: u32,
}

pub struct RoomView {
    pub(crate) id: RoomId,
    pub(crate) name: String,
    pub(crate) addr: String,
    pub(crate) online: u32,
    pub(crate) joined: bool,
}

pub struct ChatLine {
    pub(crate) sender: String,
    pub(crate) text: String,
    /// 是否由自己发出，决定气泡靠哪一边。
    pub(crate) mine: bool,
    pub(crate) system: bool,
}
