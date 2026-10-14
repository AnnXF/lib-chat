use super::*;
use store_sqlite::SqliteStore;

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
    pub(super) fn shutdown(self) {
        self.alive.store(false, Ordering::Relaxed);
        self.visible.store(false, Ordering::Relaxed);
        self.endpoint.close();
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
    let path = data_dir.join(format!("room-{beacon_port}.db"));
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
