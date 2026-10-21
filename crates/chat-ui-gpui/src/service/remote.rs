//! 公网服务器地址簿。
//!
//! 局域网房间靠广播自动出现；公网没有广播，用「连过一次就记住」替代：
//! 保存地址，启动后周期探测，拿到房间名与在线人数后照常列进侧栏。

use super::protocol::await_lobby;
use super::*;

/// 地址簿文件。只存地址，房间信息每次探测现取。
const SERVERS_FILE: &str = "servers.json";
/// 连续探测失败这么多次，就把探测到的房间从侧栏摘掉。
const FAILURES_BEFORE_DROP: u32 = 3;
/// 公网探测间隔：比局域网宽松，避免无谓握手。
const REMOTE_PROBE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Stored {
    servers: Vec<String>,
}

/// 保存过的公网服务器地址。
#[derive(Clone)]
pub(super) struct RemoteServers {
    path: PathBuf,
    entries: Arc<tokio::sync::Mutex<Vec<String>>>,
}

impl RemoteServers {
    pub(super) fn new(data_dir: &Path) -> Self {
        let path = data_dir.join(SERVERS_FILE);
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Stored>(&bytes).ok())
            .map(|stored| stored.servers)
            .unwrap_or_default();
        Self {
            path,
            entries: Arc::new(tokio::sync::Mutex::new(entries)),
        }
    }

    /// 记下一个连上过的地址；重复地址不重复保存。
    pub(super) async fn remember(&self, addr: &str) {
        let addr = addr.trim().to_string();
        if parse_host_port(&addr).is_err() {
            return;
        }
        let mut entries = self.entries.lock().await;
        if entries.iter().any(|known| known == &addr) {
            return;
        }
        entries.push(addr);
        let bytes = match serde_json::to_vec(&Stored {
            servers: entries.clone(),
        }) {
            Ok(bytes) => bytes,
            Err(_) => return,
        };
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // 先写临时文件再改名：半截写入不会让整份地址簿作废
        let temp = self.path.with_extension("json.tmp");
        if std::fs::write(&temp, bytes).is_ok() {
            let _ = std::fs::rename(&temp, &self.path);
        }
    }

    #[cfg(test)]
    pub(super) async fn entries(&self) -> Vec<String> {
        self.entries.lock().await.clone()
    }
}

/// 周期探测地址簿里的服务器，把房间名与在线人数推给界面。
pub(super) async fn probe_loop(
    events: Sender<UiEvent>,
    servers: RemoteServers,
    liveness: Liveness,
) {
    let mut failures: HashMap<String, u32> = HashMap::new();
    let mut known: HashMap<String, RoomId> = HashMap::new();

    loop {
        let entries = servers.entries.lock().await.clone();
        let mut handles = Vec::with_capacity(entries.len());
        for addr in entries {
            handles.push(tokio::spawn(async move {
                let result = probe(&addr, liveness).await;
                (addr, result)
            }));
        }

        for handle in handles {
            let Ok((addr, result)) = handle.await else {
                continue;
            };
            match result {
                Some((room, name, online)) => {
                    failures.remove(&addr);
                    known.insert(addr.clone(), room);
                    let _ = events
                        .send_async(UiEvent::RemoteRoom {
                            room,
                            name,
                            addr,
                            online,
                        })
                        .await;
                }
                None => {
                    let count = failures.entry(addr.clone()).or_insert(0);
                    *count += 1;
                    if *count >= FAILURES_BEFORE_DROP
                        && let Some(room) = known.remove(&addr)
                    {
                        let _ = events.send_async(UiEvent::RoomGone { room }).await;
                    }
                }
            }
        }

        tokio::time::sleep(REMOTE_PROBE_INTERVAL).await;
    }
}

/// 连一下、读大厅摘要、断开。失败返回 `None`，由调用方累计次数。
async fn probe(addr: &str, liveness: Liveness) -> Option<(RoomId, String, u32)> {
    let (host, port) = parse_host_port(addr).ok()?;
    let conn = tokio::time::timeout(
        PROBE_TIMEOUT,
        connect_to(&host, port, &public_identity(host.clone()), liveness),
    )
    .await
    .ok()?
    .ok()?;
    let room = conn.room_info().room;
    let client = Client::new(Arc::new(conn));
    client.lobby(room).await.ok()?;
    let info = await_lobby(&client).await?;
    let _ = client.close().await;
    (info.room == room).then_some((room, info.name, info.online_count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remembered_servers_survive_reopen_and_are_not_duplicated() {
        let dir = std::env::temp_dir().join(format!("libchat-servers-{}", uuid::Uuid::new_v4()));
        let servers = RemoteServers::new(&dir);
        servers.remember("chat.example.com:4433").await;
        servers.remember("chat.example.com:4433").await;
        servers.remember("bad-address").await;
        assert_eq!(
            servers.entries().await,
            vec!["chat.example.com:4433".to_string()]
        );

        let reopened = RemoteServers::new(&dir);
        assert_eq!(
            reopened.entries().await,
            vec!["chat.example.com:4433".to_string()]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn broken_address_book_starts_empty_instead_of_failing() {
        let dir = std::env::temp_dir().join(format!("libchat-servers-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SERVERS_FILE), b"broken").unwrap();
        assert!(RemoteServers::new(&dir).entries().await.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
