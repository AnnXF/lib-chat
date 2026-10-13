use super::protocol::await_lobby;
use super::*;

/// 已发现的房间。候选地址按可达性排序，非回环优先。
#[derive(Clone)]
struct KnownRoom {
    name: String,
    fingerprint: String,
    addrs: Vec<SocketAddr>,
}

/// 持续浏览局域网房间，并周期复查各自人数。
pub(super) async fn browse_loop(events: Sender<UiEvent>, beacon_port: u16) {
    let mut browser = match RoomBrowser::start_on(beacon_port) {
        Ok(browser) => browser,
        Err(e) => {
            let _ = events.send(UiEvent::Error(format!("房间发现不可用: {e}")));
            return;
        }
    };

    // 按房间标识去重：mDNS 与 UDP 广播会重复播报同一个房间
    let known: Arc<tokio::sync::Mutex<HashMap<RoomId, KnownRoom>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    {
        let known = Arc::clone(&known);
        let events = events.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(PROBE_INTERVAL).await;
                let rooms: Vec<(RoomId, KnownRoom)> = known
                    .lock()
                    .await
                    .iter()
                    .map(|(room, entry)| (*room, entry.clone()))
                    .collect();
                for (room, entry) in rooms {
                    probe_room(room, &entry, &events).await;
                }
            }
        });
    }

    while let Ok(announcement) = browser.next_room().await {
        let entry = {
            let mut known = known.lock().await;
            let entry = known.entry(announcement.room).or_insert_with(|| KnownRoom {
                name: announcement.name.clone(),
                fingerprint: announcement.fingerprint.clone(),
                addrs: Vec::new(),
            });
            entry.name = announcement.name.clone();
            entry.fingerprint = announcement.fingerprint.clone();
            for addr in &announcement.addrs {
                if !entry.addrs.contains(addr) {
                    entry.addrs.push(*addr);
                }
            }
            // 非回环优先：跨机必须用它，同机也照样可用
            entry.addrs.sort_by_key(|addr| addr.ip().is_loopback());
            entry.clone()
        };
        probe_room(announcement.room, &entry, &events).await;
    }
}

/// 连一下房间只为读大厅摘要，随后断开——未加入者拿不到任何消息。
///
/// 依次尝试候选地址，只播报第一个能拿到摘要的地址，避免同一房间出现多条记录。
async fn probe_room(room: RoomId, known: &KnownRoom, events: &Sender<UiEvent>) {
    let expected = (!known.fingerprint.is_empty()).then_some(known.fingerprint.as_str());

    for addr in &known.addrs {
        let Ok(conn) = connect(*addr, expected).await else {
            continue;
        };
        let conn = Arc::new(conn);
        let dyn_conn: Arc<dyn Connection> = conn.clone();
        let client = Client::new(dyn_conn);
        if client.lobby(room).await.is_err() {
            continue;
        }
        let info = await_lobby(&client).await;
        let _ = client.close().await;

        if let Some(info) = info {
            let _ = events.send(UiEvent::RoomFound {
                room,
                name: if info.name.is_empty() {
                    known.name.clone()
                } else {
                    info.name
                },
                addr: addr.to_string(),
                fingerprint: known.fingerprint.clone(),
                online: info.online_count,
            });
            return;
        }
    }
}
