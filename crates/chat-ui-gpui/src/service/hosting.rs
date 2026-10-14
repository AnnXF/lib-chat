use super::*;
use chat_core::MessageStore;
use store_sqlite::SqliteStore;

/// 本机房间的库文件：按广播端口分开，一个端口一个房间。
fn room_db_path(data_dir: &Path, beacon_port: u16) -> PathBuf {
    data_dir.join(format!("room-{beacon_port}.db"))
}

/// 看一眼本机是否已经有这个端口的房间；有就返回它存了多少条消息。
///
/// 只做探测，绝不新建文件——没房间时返回 `None`，不会凭空留下一个空库。
pub(super) async fn local_room_messages(data_dir: &Path, beacon_port: u16) -> Option<u64> {
    let path = room_db_path(data_dir, beacon_port);
    if !path.exists() {
        return None;
    }
    let store = SqliteStore::open(&path).await.ok()?;
    let room = store.room();
    let latest = store.latest_seq(room).await.ok()?;
    Some(latest.max(0) as u64)
}

/// 清空本机房间：连库文件一起删掉，之后重建会拿到新的房间标识与空白历史。
pub(super) fn reset_local_room(data_dir: &Path, beacon_port: u16) -> Result<(), String> {
    let path = room_db_path(data_dir, beacon_port);
    // WAL 模式还会留下 -wal / -shm，一起清掉才算真的重来
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.clone().into_os_string();
        name.push(suffix);
        let file = PathBuf::from(name);
        if file.exists() {
            std::fs::remove_file(&file).map_err(|e| format!("清空本机房间失败: {e}"))?;
        }
    }
    Ok(())
}

/// 本机托管的房间。切换房间或重新建房时必须回收，否则会留下幽灵房间。
pub(super) struct Hosting {
    pub(super) room: RoomId,
    pub(super) endpoint: Arc<QuicServer>,
    /// 仅用于保活：持有它才能持续广播房间。
    pub(super) _announcer: RoomAnnouncer,
    /// 定时上报是否终止（重建房/关闭时置 false）。
    pub(super) alive: Arc<AtomicBool>,
    /// 界面当前是否在展示这个房间；否则上报会污染别的房间的人数。
    pub(super) visible: Arc<AtomicBool>,
}

impl Hosting {
    pub(super) fn shutdown(self, events: &Sender<UiEvent>) {
        self.alive.store(false, Ordering::Relaxed);
        self.visible.store(false, Ordering::Relaxed);
        self.endpoint.close();
        // 这个房间是我们自己撤下的，不必等房间发现那边探三次失败（十来秒）才从列表里下去。
        // 那一行留在列表里就是可点的，点一下只能换来一次连接超时。
        let _ = events.send(UiEvent::RoomGone { room: self.room });
        // announcer 随 self 一起析构，撤销 mDNS 注册并停止广播应答
    }
}

pub(super) struct Started {
    pub(super) server: Arc<Server>,
    pub(super) endpoint: Arc<QuicServer>,
    pub(super) announcer: RoomAnnouncer,
    pub(super) addr: String,
    pub(super) fingerprint: String,
}

pub(super) async fn start_host(
    name: &str,
    events: &Sender<UiEvent>,
    beacon_port: u16,
    liveness: Liveness,
    data_dir: &Path,
) -> Result<Started, String> {
    // 库文件按广播端口分开：同机不同端口各是各的房间，同一个端口重启后仍是同一个房间
    let path = room_db_path(data_dir, beacon_port);
    let store = SqliteStore::open(&path)
        .await
        .map_err(|e| format!("打开消息库失败: {e}"))?;
    let server = Server::with_room(store.room(), name, Arc::new(store));
    let endpoint = Arc::new(
        QuicServer::bind_with(SocketAddr::from(([0, 0, 0, 0], 0)), liveness)
            .map_err(|e| e.to_string())?,
    );
    let port = endpoint.local_addr().port();
    let fingerprint = endpoint.fingerprint().to_string();

    let acceptor: Arc<dyn ConnectionAcceptor> = Arc::new(Acceptor(Arc::clone(&server)));
    tokio::spawn(Arc::clone(&endpoint).serve(acceptor));

    let announcer = RoomAnnouncer::start_on(beacon_port, server.room(), name, port, &fingerprint)
        .map_err(|e| format!("房间广播失败: {e}"))?;

    let _ = events.send(UiEvent::Hosting {
        name: name.to_string(),
        addrs: local_addrs(port),
        fingerprint: fingerprint.clone(),
    });

    Ok(Started {
        server,
        endpoint,
        announcer,
        addr: format!("127.0.0.1:{port}"),
        fingerprint,
    })
}

/// 本机可被局域网访问的地址，附带回环地址便于单机自测。
fn local_addrs(port: u16) -> Vec<String> {
    let mut addrs = vec![format!("127.0.0.1:{port}")];
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        for interface in ifs {
            if !interface.is_loopback() && interface.is_oper_up() && interface.ip().is_ipv4() {
                addrs.push(format!("{}:{port}", interface.ip()));
            }
        }
    }
    addrs
}
