use std::time::Instant;

use super::protocol::await_lobby;
use super::*;

/// 连续探测失败这么多次，就当房间已经不在了。
///
/// 房主被强杀时不会留下任何告别信号：广播停了，但没人会通知我们。只凭「收不到广播」
/// 判断会把 WiFi 抖动误伤成房间消失，所以在同一条「连不上」的信号上连续计数。
const PROBE_FAILURES_BEFORE_DROP: u32 = 3;

/// 已发现的房间。候选地址按可达性排序，非回环优先。
#[derive(Clone)]
struct KnownRoom {
    name: String,
    fingerprint: String,
    addrs: Vec<SocketAddr>,
    /// 连续探测失败次数，探到一次就清零。
    failures: u32,
    /// 上次探测时刻：广播每 2 秒就会重复播报，不必每次都重新握手。
    last_probed: Option<Instant>,
}

impl KnownRoom {
    fn new(name: String, fingerprint: String) -> Self {
        Self {
            name,
            fingerprint,
            addrs: Vec::new(),
            failures: 0,
            last_probed: None,
        }
    }

    fn announce(&mut self, announcement: &chat_discovery::RoomAnnouncement) {
        self.name = announcement.name.clone();
        if self.fingerprint != announcement.fingerprint {
            self.fingerprint = announcement.fingerprint.clone();
            self.addrs.clear();
            self.last_probed = None;
            self.failures = 0;
        }
        let mut fresh = announcement.addrs.clone();
        fresh.sort_by_key(|addr| addr.ip().is_loopback());
        fresh.dedup();
        if fresh.iter().any(|addr| !self.addrs.contains(addr)) {
            self.last_probed = None;
        }
        self.addrs.retain(|addr| !fresh.contains(addr));
        fresh.append(&mut self.addrs);
        self.addrs = fresh;
    }
}

type Rooms = Arc<tokio::sync::Mutex<HashMap<RoomId, KnownRoom>>>;

/// 已发现房间的查询入口。
///
/// 后端循环用它回答一个问题：这个房间现在有没有人在托管？——同机的第二个实例不该
/// 再起一个服务器，而要接到已有的那个上。
#[derive(Clone, Default)]
pub(super) struct DiscoveredRooms(Rooms);

impl DiscoveredRooms {
    /// 这个房间正被谁托管？给出一个可用地址与它的指纹。
    ///
    /// 地址来自广播播报，条目要连续探测失败几次才摘掉，所以这里查得到只说明前阵子还有人应答。
    /// 房主刚走时广播会残留在这里，拿到地址后得真连一次才算数。
    pub(super) async fn host_of(&self, room: RoomId) -> Option<(String, String)> {
        let map = self.0.lock().await;
        let entry = map.get(&room)?;
        let addr = entry.addrs.first()?.to_string();
        Some((addr, entry.fingerprint.clone()))
    }
}

/// 持续浏览局域网房间，并周期复查各自人数。
pub(super) async fn browse_loop(
    events: Sender<UiEvent>,
    beacon_port: u16,
    rooms: DiscoveredRooms,
    data_dir: PathBuf,
) {
    let mut browser = match RoomBrowser::start_on(beacon_port) {
        Ok(browser) => browser,
        Err(e) => {
            let _ = events
                .send_async(UiEvent::Error(format!("房间发现不可用: {e}")))
                .await;
            return;
        }
    };

    // 按房间标识去重：mDNS 与 UDP 广播会重复播报同一个房间
    let known = rooms.0;
    let trust = TrustStore::new(&data_dir);

    // 周期复查：房间消失时没有任何通知，只能靠探测发现
    {
        let known = Arc::clone(&known);
        let events = events.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(PROBE_INTERVAL).await;
                let due = take_due(&known, None).await;
                probe_all(&known, due, &events).await;
            }
        });
    }

    while let Ok(announcement) = browser.next_room().await {
        if validate_fingerprint(&announcement.fingerprint).is_err() {
            continue;
        }
        let mut announcement = announcement;
        announcement.fingerprint.make_ascii_lowercase();
        match trust.get(announcement.room) {
            Ok(Some(pinned)) if pinned != announcement.fingerprint => continue,
            Err(error) => {
                let _ = events.send_async(UiEvent::Error(error.to_string())).await;
                continue;
            }
            _ => {}
        }
        {
            let mut map = known.lock().await;
            let entry = map.entry(announcement.room).or_insert_with(|| {
                KnownRoom::new(announcement.name.clone(), announcement.fingerprint.clone())
            });
            entry.announce(&announcement);
        }
        let due = take_due(&known, Some(announcement.room)).await;
        probe_all(&known, due, &events).await;
    }
}

/// 挑出「该复查了」的房间，并记下时刻。`only` 为 `None` 表示所有房间都参与挑选。
async fn take_due(known: &Rooms, only: Option<RoomId>) -> Vec<(RoomId, KnownRoom)> {
    let mut map = known.lock().await;
    let now = Instant::now();
    let mut due = Vec::new();

    for (room, entry) in map.iter_mut() {
        if only.is_some_and(|wanted| wanted != *room) {
            continue;
        }
        // 广播每 2 秒重复播报一次，刚探过的不必再握手一遍
        if entry
            .last_probed
            .is_some_and(|at| now.duration_since(at) < PROBE_INTERVAL)
        {
            continue;
        }
        entry.last_probed = Some(now);
        due.push((*room, entry.clone()));
    }
    due
}

/// 并发探测一批房间，并按结果淘汰连续失败到阈值的那些。
async fn probe_all(known: &Rooms, rooms: Vec<(RoomId, KnownRoom)>, events: &Sender<UiEvent>) {
    // 各探各的：某个地址不可达时只拖慢它自己，不会把整批房间一起卡住
    let mut handles = Vec::with_capacity(rooms.len());
    for (room, entry) in rooms {
        let events = events.clone();
        handles.push(tokio::spawn(async move {
            (room, probe_room(room, &entry, &events).await)
        }));
    }

    for handle in handles {
        let Ok((room, alive)) = handle.await else {
            continue;
        };
        let mut map = known.lock().await;
        let too_many_failures = {
            let Some(entry) = map.get_mut(&room) else {
                continue;
            };
            if alive {
                entry.failures = 0;
                false
            } else {
                entry.failures += 1;
                entry.failures >= PROBE_FAILURES_BEFORE_DROP
            }
        };
        if too_many_failures {
            map.remove(&room);
            drop(map);
            let _ = events.send_async(UiEvent::RoomGone { room }).await;
        }
    }
}

/// 探测一个房间：连一下、读大厅摘要、断开。返回是否探到摘要。
///
/// 每个地址都带超时：房主被强杀时地址会在广播残留期间一直不可达，而 QUIC 连接要等到
/// 空闲超时才失败（默认 60 秒）——没有超时的话，一次探测就能把整个发现功能拖死。
async fn probe_room(room: RoomId, known: &KnownRoom, events: &Sender<UiEvent>) -> bool {
    if validate_fingerprint(&known.fingerprint).is_err() {
        return false;
    }
    for addr in &known.addrs {
        let Ok(Ok(conn)) =
            tokio::time::timeout(PROBE_TIMEOUT, connect(*addr, &known.fingerprint)).await
        else {
            continue;
        };
        if conn.room_info().room != room {
            continue;
        }
        let dyn_conn: Arc<dyn Connection> = Arc::new(conn);
        let client = Client::new(dyn_conn);
        if client.lobby(room).await.is_err() {
            continue;
        }
        let info = await_lobby(&client).await;
        let _ = client.close().await;

        if let Some(info) = info.filter(|info| info.room == room) {
            let _ = events
                .send_async(UiEvent::RoomFound {
                    room,
                    name: if info.name.is_empty() {
                        known.name.clone()
                    } else {
                        info.name
                    },
                    addr: addr.to_string(),
                    fingerprint: known.fingerprint.clone(),
                    online: info.online_count,
                })
                .await;
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn changed_host_replaces_stale_addresses() {
        let room = RoomId::new();
        let rooms = DiscoveredRooms::default();
        let first = chat_discovery::RoomAnnouncement {
            room,
            name: "房间".into(),
            version: chat_discovery::PROTOCOL_VERSION,
            fingerprint: "first".into(),
            addrs: vec!["127.0.0.1:40001".parse().unwrap()],
        };
        rooms.0.lock().await.insert(
            room,
            KnownRoom::new(first.name.clone(), first.fingerprint.clone()),
        );
        rooms
            .0
            .lock()
            .await
            .get_mut(&room)
            .unwrap()
            .announce(&first);
        assert_eq!(rooms.host_of(room).await.unwrap().0, "127.0.0.1:40001");

        let second = chat_discovery::RoomAnnouncement {
            fingerprint: "second".into(),
            addrs: vec!["127.0.0.1:40002".parse().unwrap()],
            ..first
        };
        rooms
            .0
            .lock()
            .await
            .get_mut(&room)
            .unwrap()
            .announce(&second);
        assert_eq!(
            rooms.host_of(room).await.unwrap(),
            ("127.0.0.1:40002".into(), "second".into())
        );
    }

    #[tokio::test]
    async fn same_identity_new_port_is_preferred_for_reconnect() {
        let room = RoomId::new();
        let rooms = DiscoveredRooms::default();
        let original = chat_discovery::RoomAnnouncement {
            room,
            name: "房间".into(),
            version: chat_discovery::PROTOCOL_VERSION,
            fingerprint: "a".repeat(64),
            addrs: vec!["127.0.0.1:40001".parse().unwrap()],
        };
        let mut known = KnownRoom::new(original.name.clone(), original.fingerprint.clone());
        known.announce(&original);
        known.last_probed = Some(Instant::now());
        let reopened = chat_discovery::RoomAnnouncement {
            addrs: vec!["127.0.0.1:40002".parse().unwrap()],
            ..original
        };
        known.announce(&reopened);
        assert!(known.last_probed.is_none());
        rooms.0.lock().await.insert(room, known);
        assert_eq!(rooms.host_of(room).await.unwrap().0, "127.0.0.1:40002");
    }

    /// 回归：不可达地址必须**很快**放弃。
    ///
    /// 曾经没有超时，`connect` 会一直挂到 QUIC 空闲超时（默认 60 秒）才失败；而探测是
    /// 串行的，一个被强杀的房间就能让房间发现功能彻底停摆。
    #[tokio::test]
    async fn gives_up_quickly_on_unreachable_room() {
        let (events, _rx) = flume::unbounded();
        let dead = KnownRoom {
            name: "已经没人了".into(),
            fingerprint: String::new(),
            // 192.0.2.0/24 是 TEST-NET-1，保证不会有主机应答
            addrs: vec!["192.0.2.1:45678".parse().unwrap()],
            failures: 0,
            last_probed: None,
        };

        let started = Instant::now();
        let alive = probe_room(RoomId::new(), &dead, &events).await;
        let elapsed = started.elapsed();

        assert!(!alive, "不可达地址不应算探测成功");
        assert!(
            elapsed < PROBE_TIMEOUT * 2,
            "探测必须带超时，否则会拖住整个发现循环；实际耗时 {elapsed:?}"
        );
    }
}
