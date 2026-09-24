//! mDNS 通道：广播与浏览。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use chat_core::{ChatError, Result};
use chat_proto::RoomId;
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::mpsc;

use crate::{RoomAnnouncement, PROTOCOL_VERSION};

/// mDNS 服务类型。
pub const SERVICE_TYPE: &str = "_libchat._udp.local.";

const TXT_ROOM: &str = "room";
const TXT_NAME: &str = "name";
const TXT_VERSION: &str = "version";
const TXT_FINGERPRINT: &str = "fp";

fn transport_err(e: impl std::fmt::Display) -> ChatError {
    ChatError::Transport(e.to_string())
}

/// 本机可用的局域网 IPv4 地址，过滤回环与未启用接口。
fn local_ipv4_addrs() -> Vec<IpAddr> {
    if_addrs::get_if_addrs()
        .map(|ifs| {
            ifs.into_iter()
                .filter(|i| !i.is_loopback() && i.is_oper_up())
                .map(|i| i.ip())
                .filter(IpAddr::is_ipv4)
                .collect()
        })
        .unwrap_or_default()
}

/// 用 mDNS 广播一个房间。
pub struct RoomAdvertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

impl RoomAdvertiser {
    pub fn start(room: RoomId, name: &str, port: u16, fingerprint: &str) -> Result<Self> {
        let daemon = ServiceDaemon::new().map_err(transport_err)?;

        let mut props = HashMap::new();
        props.insert(TXT_ROOM.to_string(), room.to_string());
        props.insert(TXT_NAME.to_string(), name.to_string());
        props.insert(TXT_VERSION.to_string(), PROTOCOL_VERSION.to_string());
        props.insert(TXT_FINGERPRINT.to_string(), fingerprint.to_string());

        let addrs = local_ipv4_addrs();
        if addrs.is_empty() {
            return Err(ChatError::Transport("未找到可用的局域网 IPv4 地址".into()));
        }
        let ips = addrs
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let short = &room.to_string()[..8];
        let instance = format!("libchat-{short}");
        let host = format!("libchat-{short}.local.");

        let info = ServiceInfo::new(SERVICE_TYPE, &instance, &host, ips, port, props)
            .map_err(transport_err)?;
        let fullname = info.get_fullname().to_string();
        daemon.register(info).map_err(transport_err)?;
        Ok(Self { daemon, fullname })
    }

    pub fn fullname(&self) -> &str {
        &self.fullname
    }
}

impl Drop for RoomAdvertiser {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// 浏览 mDNS 上的房间。
pub struct RoomBrowser {
    daemon: ServiceDaemon,
}

impl RoomBrowser {
    pub fn start(tx: mpsc::UnboundedSender<RoomAnnouncement>) -> Result<Self> {
        let daemon = ServiceDaemon::new().map_err(transport_err)?;
        let events = daemon.browse(SERVICE_TYPE).map_err(transport_err)?;

        tokio::spawn(async move {
            while let Ok(event) = events.recv_async().await {
                if let ServiceEvent::ServiceResolved(info) = event {
                    if let Some(room) = to_announcement(&info) {
                        if tx.send(room).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self { daemon })
    }
}

impl Drop for RoomBrowser {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

fn to_announcement(info: &ResolvedService) -> Option<RoomAnnouncement> {
    let props = info.get_properties().clone().into_property_map_str();
    let addrs = info
        .get_addresses()
        .iter()
        .map(|ip| SocketAddr::new(ip.to_ip_addr(), info.get_port()))
        .collect();
    announcement_from(&props, addrs)
}

/// 把 TXT 属性与地址映射成房间摘要，缺字段或地址为空则视为无效广播。
fn announcement_from(
    props: &HashMap<String, String>,
    addrs: Vec<SocketAddr>,
) -> Option<RoomAnnouncement> {
    let room: RoomId = props.get(TXT_ROOM)?.parse().ok()?;
    let version: u32 = props.get(TXT_VERSION)?.parse().ok()?;
    let name = props
        .get(TXT_NAME)
        .cloned()
        .unwrap_or_else(|| "未命名房间".to_string());
    let fingerprint = props.get(TXT_FINGERPRINT).cloned().unwrap_or_default();

    if addrs.is_empty() {
        return None;
    }
    Some(RoomAnnouncement {
        room,
        name,
        version,
        fingerprint,
        addrs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_props(room: RoomId) -> HashMap<String, String> {
        let mut props = HashMap::new();
        props.insert(TXT_ROOM.to_string(), room.to_string());
        props.insert(TXT_NAME.to_string(), "测试房间".to_string());
        props.insert(TXT_VERSION.to_string(), PROTOCOL_VERSION.to_string());
        props.insert(TXT_FINGERPRINT.to_string(), "deadbeef".to_string());
        props
    }

    #[test]
    fn parses_valid_announcement() {
        let room = RoomId::new();
        let addr: SocketAddr = "10.0.0.5:45678".parse().unwrap();
        let ann = announcement_from(&sample_props(room), vec![addr]).unwrap();

        assert_eq!(ann.room, room);
        assert_eq!(ann.name, "测试房间");
        assert_eq!(ann.version, PROTOCOL_VERSION);
        assert_eq!(ann.fingerprint, "deadbeef");
        assert_eq!(ann.addrs, vec![addr]);
    }

    #[test]
    fn rejects_missing_room_or_version() {
        let room = RoomId::new();
        let addr: SocketAddr = "10.0.0.5:45678".parse().unwrap();

        let mut no_room = sample_props(room);
        no_room.remove(TXT_ROOM);
        assert!(announcement_from(&no_room, vec![addr]).is_none());

        let mut bad_version = sample_props(room);
        bad_version.insert(TXT_VERSION.to_string(), "abc".to_string());
        assert!(announcement_from(&bad_version, vec![addr]).is_none());

        // 无地址的广播不可连接
        assert!(announcement_from(&sample_props(room), vec![]).is_none());
    }
}
