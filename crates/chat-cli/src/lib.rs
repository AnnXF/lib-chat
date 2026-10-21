//! 命令行形态的可复用部分：独立服务端托管与地址解析。
//!
//! `serve` 是公网部署入口：同一份服务端代码，换成受信 CA 证书监听公网地址；
//! 不给证书时退回自签，用于内网或本机自测。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use chat_core::{ConnectionAcceptor, MessageStore, RoomId};
use chat_discovery::RoomAnnouncer;
use chat_server::{Acceptor, DiskBlobs, Server, SqliteStore};
use chat_transport_quic::{Liveness, QuicServer, SelfSignedCert};

pub type CliResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// 服务端证书来源。
pub enum ServerTls {
    /// 自签证书：指纹随启动信息给出，客户端按指纹连接。
    SelfSigned,
    /// 受信 CA 签发的证书链与私钥。
    Pem { cert: PathBuf, key: PathBuf },
}

pub struct ServeConfig {
    pub listen: SocketAddr,
    pub name: String,
    pub data_dir: PathBuf,
    pub tls: ServerTls,
    /// 给出端口时同时做局域网广播；公网部署通常不需要。
    pub announce_port: Option<u16>,
}

/// 已启动的服务端。drop 前调用 [`Serving::shutdown`] 可以把端口与数据库交还出去。
pub struct Serving {
    pub room: RoomId,
    pub addr: SocketAddr,
    pub fingerprint: String,
    pub server: Arc<Server>,
    quic: Arc<QuicServer>,
    store: Arc<SqliteStore>,
    /// 仅用于保活广播。
    _announcer: Option<RoomAnnouncer>,
}

impl Serving {
    /// 停止监听并关闭消息库。重启后同一数据目录仍是同一个房间。
    pub async fn shutdown(self) {
        self.quic.close();
        self.quic.wait_idle().await;
        self.store.close().await;
    }
}

/// 启动一个独立服务端：消息落 SQLite，附件落磁盘。
pub async fn serve(config: ServeConfig) -> CliResult<Serving> {
    std::fs::create_dir_all(&config.data_dir)?;
    let db_path = config.data_dir.join("room.db");
    // 库还在时不能悄悄换证书身份，否则客户端固定过的指纹就失效了
    let allow_new_identity = !db_path.exists();
    let store = Arc::new(SqliteStore::open(&db_path).await?);
    let blobs = Arc::new(DiskBlobs::new(config.data_dir.join("room.files"))?);
    let server = Server::with_blobs(
        store.room(),
        &config.name,
        Arc::clone(&store) as Arc<dyn MessageStore>,
        blobs,
    );

    let quic = Arc::new(match &config.tls {
        ServerTls::SelfSigned => QuicServer::bind_with_cert(
            config.listen,
            Liveness::default(),
            SelfSignedCert::load_or_create(
                &config.data_dir.join("room.identity"),
                allow_new_identity,
            )?,
        )?,
        ServerTls::Pem { cert, key } => QuicServer::bind_with_pem(
            config.listen,
            Liveness::default(),
            &std::fs::read(cert)?,
            &std::fs::read(key)?,
        )?,
    });
    let addr = quic.local_addr();
    let fingerprint = quic.fingerprint().to_string();

    let acceptor: Arc<dyn ConnectionAcceptor> = Arc::new(Acceptor(Arc::clone(&server)));
    tokio::spawn(Arc::clone(&quic).serve(acceptor));

    let announcer = match config.announce_port {
        Some(port) => Some(RoomAnnouncer::start_on(
            port,
            server.room(),
            &config.name,
            addr.port(),
            &fingerprint,
        )?),
        None => None,
    };

    Ok(Serving {
        room: server.room(),
        addr,
        fingerprint,
        server,
        quic,
        store,
        _announcer: announcer,
    })
}

/// 默认数据目录：`$HOME/.lib-chat/rooms`。
pub fn default_data_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("无法确定家目录，请指定 --data-dir")?;
    Ok(PathBuf::from(home).join(".lib-chat").join("rooms"))
}
