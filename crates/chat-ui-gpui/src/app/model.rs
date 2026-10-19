//! 界面持有的视图数据。
//!
//! 与协议 DTO 分开：这里只描述「界面要用到的形状」，不掺网络语义。

use chat_core::{RoomId, Seq};
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

/// 本机托管的房间。侧栏默认只显示一行摘要，完整标识与指纹按需展开。
#[derive(Clone)]
pub struct HostedRoom {
    pub(crate) name: String,
    pub(crate) room: RoomId,
    pub(crate) addrs: Vec<String>,
    pub(crate) fingerprint: String,
}

impl HostedRoom {
    /// 摘要里的地址：优先给局域网地址，没有才退回回环。
    pub(crate) fn summary_addr(&self) -> &str {
        self.addrs
            .iter()
            .find(|addr| !addr.starts_with("127.") && !addr.starts_with("[::1]"))
            .or_else(|| self.addrs.first())
            .map(String::as_str)
            .unwrap_or("监听中")
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

pub struct ChatLine {
    pub(crate) seq: Option<Seq>,
    pub(crate) sender: String,
    pub(crate) text: String,
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
}
