//! 界面持有的视图数据。
//!
//! 与协议 DTO 分开：这里只描述「界面要用到的形状」，不掺网络语义。

use chat_core::{Attachment, Quote, RoomId, Seq};

use crate::service::PublicHosting;

/// 房间从哪来：局域网广播，还是保存下来的公网地址。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RoomSource {
    Local,
    Remote,
}

/// 左侧导航页：聊天 / 通讯录 / 房间与连接。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NavTab {
    Chats,
    Contacts,
    Rooms,
}

/// 顶栏状态的语气：错误红、重连橙、其余灰。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Warn,
    Error,
}

#[derive(Clone)]
pub struct DiscoveredRoom {
    pub(crate) id: RoomId,
    pub(crate) name: String,
    pub(crate) addr: String,
    /// 自签房间的指纹；公网受信 CA 为 `None`。
    pub(crate) fingerprint: Option<String>,
    pub(crate) online: u32,
    pub(crate) source: RoomSource,
}

pub struct RoomView {
    pub(crate) id: RoomId,
    pub(crate) name: String,
    pub(crate) addr: String,
    pub(crate) online: u32,
    pub(crate) joined: bool,
}

/// 本机托管的房间。侧栏默认只显示一行摘要，完整标识与指纹按需展开。
#[derive(Clone)]
pub struct HostedRoom {
    pub(crate) name: String,
    pub(crate) room: RoomId,
    pub(crate) addrs: Vec<String>,
    pub(crate) fingerprint: String,
    /// 公网托管的接入方式；局域网房间为 `None`。
    pub(crate) public: Option<PublicHosting>,
}

impl HostedRoom {
    /// 摘要里的地址：公网给域名或监听端口，局域网优先给非回环地址。
    pub(crate) fn summary_addr(&self) -> String {
        if let Some(public) = &self.public {
            return match &public.domain {
                Some(domain) => format!("{domain}:{}", public.port),
                None => format!("0.0.0.0:{}", public.port),
            };
        }
        self.addrs
            .iter()
            .find(|addr| !addr.starts_with("127.") && !addr.starts_with("[::1]"))
            .or_else(|| self.addrs.first())
            .cloned()
            .unwrap_or_else(|| "监听中".into())
    }

    /// 是不是公网托管；侧栏据此换标题与详情。
    pub(crate) fn is_public(&self) -> bool {
        self.public.is_some()
    }

    /// 客户端可直接使用的地址：公网优先域名，没有域名就退回本机回环。
    pub(crate) fn join_addr(&self) -> String {
        if let Some(public) = &self.public {
            return match &public.domain {
                Some(domain) => format!("{domain}:{}", public.port),
                None => format!("127.0.0.1:{}", public.port),
            };
        }
        self.summary_addr()
    }

    /// 房间标识只露前 8 位，用于本机区分。
    pub(crate) fn short_room(&self) -> String {
        let text = self.room.to_string();
        text.chars().take(8).collect()
    }

    /// 完整指纹按 16 位一组折行，便于逐字核对。
    pub(crate) fn fingerprint_rows(&self) -> String {
        self.fingerprint
            .as_bytes()
            .chunks(16)
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone)]
pub struct ChatLine {
    pub(crate) seq: Option<Seq>,
    pub(crate) sender: String,
    pub(crate) text: String,
    /// 文件消息附带的元数据；文本消息为 `None`。
    pub(crate) attachment: Option<Attachment>,
    /// 这条消息引用了哪条消息。
    pub(crate) quote: Option<Quote>,
    /// 是否由自己发出，决定气泡靠哪一边。
    pub(crate) mine: bool,
    pub(crate) system: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosted(addrs: Vec<&str>) -> HostedRoom {
        HostedRoom {
            name: "房间".into(),
            room: RoomId::new(),
            addrs: addrs.into_iter().map(str::to_string).collect(),
            fingerprint: "a1b2c3d4".repeat(8),
            public: None,
        }
    }

    #[test]
    fn summary_prefers_lan_address_over_loopback() {
        let room = hosted(vec!["127.0.0.1:52000", "192.168.1.5:52000"]);
        assert_eq!(room.summary_addr(), "192.168.1.5:52000");
        let only_loopback = hosted(vec!["127.0.0.1:52000"]);
        assert_eq!(only_loopback.summary_addr(), "127.0.0.1:52000");
        assert_eq!(hosted(Vec::new()).summary_addr(), "监听中");
    }

    #[test]
    fn identity_details_stay_compact_and_readable() {
        let room = hosted(vec!["192.168.1.5:52000"]);
        assert_eq!(room.short_room().len(), 8);
        assert_eq!(room.fingerprint_rows().lines().count(), 4);
        assert!(room.fingerprint_rows().lines().all(|row| row.len() == 16));
    }

    #[test]
    fn public_room_summary_prefers_the_domain() {
        let mut room = hosted(vec!["127.0.0.1:4433"]);
        assert!(!room.is_public());
        room.public = Some(PublicHosting {
            port: 4433,
            domain: Some("chat.example.com".into()),
            trusted_ca: true,
        });
        assert!(room.is_public());
        assert_eq!(room.summary_addr(), "chat.example.com:4433");

        room.public = Some(PublicHosting {
            port: 4433,
            domain: None,
            trusted_ca: false,
        });
        assert_eq!(room.summary_addr(), "0.0.0.0:4433");
    }
}
