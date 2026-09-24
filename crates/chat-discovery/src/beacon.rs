//! UDP 广播通道。
//!
//! mDNS 在同一台机器上的两个进程之间互相看不见（组播套接字分发所致），
//! 因此在固定端口上再走一条广播通道：询问者向广播地址与本机回环各发一次探测，
//! 房主直接应答。这样同机多实例与跨机都能被发现。

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chat_core::{ChatError, Result};
use chat_proto::RoomId;
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::sync::mpsc;

use crate::{RoomAnnouncement, PROTOCOL_VERSION};

/// 广播探测使用的固定 UDP 端口。
pub const BEACON_PORT: u16 = 52100;

/// 报文魔数，用于丢弃无关流量。
const MAGIC: u32 = 0x4C43_4841;

/// 探测间隔。
const PROBE_INTERVAL: Duration = Duration::from_secs(2);
/// 每轮探测后等待应答的时长。
const REPLY_WINDOW: Duration = Duration::from_millis(400);
/// 单条报文上限。
const MAX_PACKET: usize = 1024;

fn transport_err(e: impl std::fmt::Display) -> ChatError {
    ChatError::Transport(e.to_string())
}

#[derive(Serialize, Deserialize)]
struct Probe {
    magic: u32,
    version: u32,
}

#[derive(Serialize, Deserialize)]
struct Reply {
    magic: u32,
    version: u32,
    room: String,
    name: String,
    port: u16,
    fp: String,
}

/// 绑定可复用的 UDP 套接字，使同机多个实例都能收到广播。
fn bind_reusable(port: u16) -> Result<UdpSocket> {
    let socket =
        Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(transport_err)?;
    socket.set_reuse_address(true).map_err(transport_err)?;
    #[cfg(unix)]
    socket.set_reuse_port(true).map_err(transport_err)?;
    socket.set_broadcast(true).map_err(transport_err)?;

    let addr: socket2::SockAddr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into();
    socket.bind(&addr).map_err(transport_err)?;
    Ok(socket.into())
}

/// 应答广播探测，把本机房间告知询问者。
pub struct BeaconResponder {
    running: Arc<AtomicBool>,
    _thread: JoinHandle<()>,
}

impl BeaconResponder {
    pub fn start(room: RoomId, name: String, port: u16, fingerprint: String) -> Result<Self> {
        let socket = bind_reusable(BEACON_PORT)?;
        socket
            .set_read_timeout(Some(Duration::from_millis(400)))
            .map_err(transport_err)?;

        let running = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&running);

        let thread = std::thread::Builder::new()
            .name("beacon-responder".into())
            .spawn(move || {
                let reply = Reply {
                    magic: MAGIC,
                    version: PROTOCOL_VERSION,
                    room: room.to_string(),
                    name,
                    port,
                    fp: fingerprint,
                };
                let Ok(payload) = serde_json::to_vec(&reply) else {
                    return;
                };

                let mut buf = [0u8; MAX_PACKET];
                while flag.load(Ordering::Relaxed) {
                    if let Ok((len, from)) = socket.recv_from(&mut buf) {
                        if let Ok(probe) = serde_json::from_slice::<Probe>(&buf[..len]) {
                            if probe.magic == MAGIC && probe.version == PROTOCOL_VERSION {
                                let _ = socket.send_to(&payload, from);
                            }
                        }
                    }
                }
            })
            .map_err(transport_err)?;

        Ok(Self {
            running,
            _thread: thread,
        })
    }
}

impl Drop for BeaconResponder {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

/// 周期性广播探测并收集应答。
pub struct BeaconProbe {
    running: Arc<AtomicBool>,
    _thread: JoinHandle<()>,
}

impl BeaconProbe {
    pub fn start(tx: mpsc::UnboundedSender<RoomAnnouncement>) -> Result<Self> {
        let socket = bind_reusable(0)?;
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .map_err(transport_err)?;

        let running = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&running);

        let thread = std::thread::Builder::new()
            .name("beacon-probe".into())
            .spawn(move || {
                let probe = Probe {
                    magic: MAGIC,
                    version: PROTOCOL_VERSION,
                };
                let Ok(payload) = serde_json::to_vec(&probe) else {
                    return;
                };
                // 广播覆盖跨机，回环覆盖同机
                let targets = [
                    SocketAddr::from((Ipv4Addr::BROADCAST, BEACON_PORT)),
                    SocketAddr::from((Ipv4Addr::LOCALHOST, BEACON_PORT)),
                ];

                let mut buf = [0u8; MAX_PACKET];
                while flag.load(Ordering::Relaxed) {
                    for target in targets {
                        let _ = socket.send_to(&payload, target);
                    }

                    // 同一个房间可能经广播与回环各回一次，按指纹合并地址
                    let mut found: Vec<RoomAnnouncement> = Vec::new();
                    let deadline = Instant::now() + REPLY_WINDOW;
                    while Instant::now() < deadline {
                        let Ok((len, from)) = socket.recv_from(&mut buf) else {
                            break;
                        };
                        let Ok(reply) = serde_json::from_slice::<Reply>(&buf[..len]) else {
                            continue;
                        };
                        if reply.magic != MAGIC || reply.version != PROTOCOL_VERSION {
                            continue;
                        }
                        let Ok(room) = reply.room.parse::<RoomId>() else {
                            continue;
                        };
                        let addr = SocketAddr::new(from.ip(), reply.port);

                        match found
                            .iter_mut()
                            .find(|announcement| announcement.fingerprint == reply.fp)
                        {
                            Some(existing) => {
                                if !existing.addrs.contains(&addr) {
                                    existing.addrs.push(addr);
                                }
                            }
                            None => found.push(RoomAnnouncement {
                                room,
                                name: reply.name,
                                version: reply.version,
                                fingerprint: reply.fp,
                                addrs: vec![addr],
                            }),
                        }
                    }

                    for announcement in found {
                        if tx.send(announcement).is_err() {
                            return;
                        }
                    }

                    std::thread::sleep(PROBE_INTERVAL.saturating_sub(REPLY_WINDOW));
                }
            })
            .map_err(transport_err)?;

        Ok(Self {
            running,
            _thread: thread,
        })
    }
}

impl Drop for BeaconProbe {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn discovers_room_on_same_host() {
        let room = RoomId::new();
        let _responder =
            BeaconResponder::start(room, "广播测试".into(), 45678, "cafebabe".into()).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _probe = BeaconProbe::start(tx).unwrap();

        let found = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let announcement = rx.recv().await.unwrap();
                if announcement.room == room {
                    break announcement;
                }
            }
        })
        .await
        .expect("未在超时内通过 UDP 广播发现房间");

        assert_eq!(found.name, "广播测试");
        assert_eq!(found.fingerprint, "cafebabe");
        assert!(found.addrs.iter().any(|a| a.port() == 45678));
    }
}
