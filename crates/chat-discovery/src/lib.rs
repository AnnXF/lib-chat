//! 局域网房间发现，双通道并用：
//!
//! - **mDNS**：跨机发现的标准做法，但同主机上两个进程互相看不见。
//! - **UDP 广播**：补上同主机与 mDNS 不生效的网络。
//!
//! 两条通道任一可用即可发现房间，互为兜底。

pub mod beacon;
pub mod mdns;

use std::net::SocketAddr;

use chat_core::RoomId;
use chat_core::{ChatError, Result};
use tokio::sync::mpsc;

pub use mdns::SERVICE_TYPE;

/// 协议版本，客户端据此拒绝不兼容的服务端。
pub const PROTOCOL_VERSION: u32 = 1;

/// 浏览到的房间摘要。
#[derive(Clone, Debug)]
pub struct RoomAnnouncement {
    pub room: RoomId,
    pub name: String,
    pub version: u32,
    /// 服务端自签证书指纹，供 TOFU 校验。
    pub fingerprint: String,
    pub addrs: Vec<SocketAddr>,
}

/// 广播一个房间：mDNS 与 UDP 广播同时开启。
pub struct RoomAnnouncer {
    _mdns: Option<mdns::RoomAdvertiser>,
    _beacon: Option<beacon::BeaconResponder>,
}

impl RoomAnnouncer {
    pub fn start(room: RoomId, name: &str, port: u16, fingerprint: &str) -> Result<Self> {
        Self::start_on(beacon::DEFAULT_BEACON_PORT, room, name, port, fingerprint)
    }

    /// 指定广播端口。同机需要跑多套互不干扰的实例时使用。
    pub fn start_on(
        beacon_port: u16,
        room: RoomId,
        name: &str,
        port: u16,
        fingerprint: &str,
    ) -> Result<Self> {
        let mdns = mdns::RoomAdvertiser::start(room, name, port, fingerprint).ok();
        let beacon = beacon::BeaconResponder::start(
            beacon_port,
            room,
            name.to_string(),
            port,
            fingerprint.to_string(),
        )
        .ok();

        if mdns.is_none() && beacon.is_none() {
            return Err(ChatError::Transport("mDNS 与 UDP 广播均不可用".into()));
        }

        Ok(Self {
            _mdns: mdns,
            _beacon: beacon,
        })
    }

    pub fn mdns_fullname(&self) -> Option<&str> {
        self._mdns.as_ref().map(mdns::RoomAdvertiser::fullname)
    }
}

/// 发现局域网房间：合并 mDNS 与 UDP 广播两条通道的结果。
pub struct RoomBrowser {
    rx: mpsc::Receiver<RoomAnnouncement>,
    _mdns: Option<mdns::RoomBrowser>,
    _beacon: Option<beacon::BeaconProbe>,
}

impl RoomBrowser {
    pub fn start() -> Result<Self> {
        Self::start_on(beacon::DEFAULT_BEACON_PORT)
    }

    /// 指定广播端口。同机需要跑多套互不干扰的实例时使用。
    pub fn start_on(beacon_port: u16) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<RoomAnnouncement>(128);

        let mdns = mdns::RoomBrowser::start(tx.clone()).ok();
        let beacon = beacon::BeaconProbe::start(beacon_port, tx.clone()).ok();

        if mdns.is_none() && beacon.is_none() {
            return Err(ChatError::Transport("mDNS 与 UDP 广播均不可用".into()));
        }

        Ok(Self {
            rx,
            _mdns: mdns,
            _beacon: beacon,
        })
    }

    /// 等待下一个被发现的房间。
    pub async fn next_room(&mut self) -> Result<RoomAnnouncement> {
        self.rx.recv().await.ok_or(ChatError::Closed)
    }
}
