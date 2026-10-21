//! QUIC 传输适配器：把服务端逻辑暴露到局域网与公网，并给客户端提供远程连接。
//!
//! 两种身份校验方式，由 [`ServerIdentity`] 选择：
//!
//! | 方式 | 适用 | 依据 |
//! |---|---|---|
//! | [`ServerIdentity::Fingerprint`] | 局域网自签证书 | 房间固定的 SHA-256 指纹 |
//! | [`ServerIdentity::PublicCa`] | 公网域名 | 受信 CA 证书链 + 主机名 |

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chat_core::{
    Attachment, AttachmentId, ClientRequest, DeviceId, RoomId, RoomInfo, ServerEvent,
    MAX_ATTACHMENT_BYTES,
};
use chat_core::{ChatError, Connection, ConnectionAcceptor, EventStream, RequestSink, Result};
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use quinn::rustls::RootCertStore;
use quinn::{ClientConfig, Endpoint, ServerConfig, TransportConfig};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

/// ALPN 标识，用于隔离本协议与其他 QUIC 流量。
pub const ALPN: &[u8] = b"libchat/1";

/// 协议版本，握手时校验。
pub const PROTOCOL_VERSION: u32 = 1;

mod trust;
pub use trust::{validate_fingerprint, TrustStore};

/// 单帧上限，防止畸形长度导致巨额分配。
const MAX_FRAME: usize = 4 * 1024 * 1024;
/// 附件分块大小。分块传输避免一次性占满帧上限。
const BLOB_CHUNK: usize = 64 * 1024;
/// 单次附件传输的总期限，避免对端失联时无限等待。
const BLOB_TIMEOUT: Duration = Duration::from_secs(120);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// 握手未完成就断开的标记：探测超时、界面取消都会走到这里，属于正常现象。
const HANDSHAKE_ABORT: &str = "握手未完成";

fn handshake_abort(step: &str) -> ChatError {
    ChatError::Protocol(format!("{HANDSHAKE_ABORT}: {step}"))
}

/// 对端退出、探测取消或连接被关闭都属于常规结束，不必当成故障刷日志。
fn is_routine_end(error: &ChatError) -> bool {
    match error {
        ChatError::Closed | ChatError::Transport(_) => true,
        ChatError::Protocol(message) => message.starts_with(HANDSHAKE_ABORT),
        _ => false,
    }
}

fn transport_err(e: impl std::fmt::Display) -> ChatError {
    ChatError::Transport(e.to_string())
}

/// 连接活性参数。
///
/// quinn 默认 `max_idle_timeout = 30s` 且**不发送保活包**。而安静的房间不产生任何网络
/// 流量——房主的人数定时器只走进程内通道，不发包——于是所有连接会在 30 秒后几乎同时被
/// 判死，用户看到的现象就是「屋里的所有人突然被弹出房间」。
///
/// 这里主动发 PING 续命，并把判死时间放宽以容忍 WiFi 抖动与短暂休眠。判死时间仍然保留，
/// 用于识别真掉线的对端：对端若已死，PING 收不到回应，仍会在该时间内断开。
#[derive(Clone, Copy, Debug)]
pub struct Liveness {
    /// 完全收不到对端数据多久后判定连接已死。
    pub idle_timeout: Duration,
    /// 保活 PING 间隔，需明显小于 `idle_timeout`，留出丢包重试的余量。
    pub keep_alive_interval: Duration,
}

impl Default for Liveness {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(60),
            keep_alive_interval: Duration::from_secs(15),
        }
    }
}

impl Liveness {
    fn transport(&self) -> Arc<TransportConfig> {
        let mut transport = TransportConfig::default();
        transport.max_idle_timeout(Some(
            self.idle_timeout
                .try_into()
                .expect("空闲超时必须为正且不超过 QUIC 上限"),
        ));
        transport.keep_alive_interval(Some(self.keep_alive_interval));
        Arc::new(transport)
    }
}

/// 证书指纹（SHA-256 十六进制小写）。
fn fingerprint_of(cert: &CertificateDer<'_>) -> String {
    let digest = Sha256::digest(cert.as_ref());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------- 线协议

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ClientFrame {
    Hello { device: DeviceId, version: u32 },
    Request(ClientRequest),
}

// 帧是逐条序列化后立刻写出，不会成批囤积；装箱只多一次分配。
#[allow(clippy::large_enum_variant)]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ServerFrame {
    Welcome { version: u32, room: RoomInfo },
    Event(ServerEvent),
}

/// 附件流的第一帧：说明这条流要做什么。
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum BlobRequest {
    Upload {
        room: RoomId,
        attachment: Attachment,
    },
    Download {
        room: RoomId,
        id: AttachmentId,
    },
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum BlobReply {
    Done,
    Begin { size: u64 },
    Error { message: String },
}

async fn write_frame<T: serde::Serialize>(send: &mut quinn::SendStream, frame: &T) -> Result<()> {
    let payload = serde_json::to_vec(frame).map_err(|e| ChatError::Protocol(e.to_string()))?;
    if payload.len() > MAX_FRAME {
        return Err(ChatError::Protocol("帧过大".into()));
    }
    send.write_all(&(payload.len() as u32).to_be_bytes())
        .await
        .map_err(transport_err)?;
    send.write_all(&payload).await.map_err(transport_err)?;
    Ok(())
}

async fn read_frame<T: serde::de::DeserializeOwned>(recv: &mut quinn::RecvStream) -> Result<T> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await.map_err(transport_err)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(ChatError::Protocol("帧过大".into()));
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await.map_err(transport_err)?;
    serde_json::from_slice(&buf).map_err(|e| ChatError::Protocol(e.to_string()))
}

/// 写一块附件数据。长度为 0 表示传输结束。
async fn write_chunk(send: &mut quinn::SendStream, data: &[u8]) -> Result<()> {
    send.write_all(&(data.len() as u32).to_be_bytes())
        .await
        .map_err(transport_err)?;
    send.write_all(data).await.map_err(transport_err)?;
    Ok(())
}

async fn read_chunk(recv: &mut quinn::RecvStream) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await.map_err(transport_err)?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 {
        return Ok(Vec::new());
    }
    if len > BLOB_CHUNK * 2 {
        return Err(ChatError::Protocol("附件分块过大".into()));
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await.map_err(transport_err)?;
    Ok(buf)
}

// ---------------------------------------------------------------- 证书

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedIdentity {
    certificate: Vec<u8>,
    private_key: Vec<u8>,
}

/// 房间使用的自签证书。
pub struct SelfSignedCert {
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    fingerprint: String,
}

impl SelfSignedCert {
    pub fn generate() -> Result<Self> {
        let issued = rcgen::generate_simple_self_signed(vec!["libchat.local".to_string()])
            .map_err(|e| ChatError::Transport(format!("生成证书失败: {e}")))?;
        let cert = issued.cert.der().clone();
        let key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(issued.signing_key.serialize_der()));
        let fingerprint = fingerprint_of(&cert);
        Ok(Self {
            cert,
            key,
            fingerprint,
        })
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn load_or_create(path: &Path, allow_new: bool) -> Result<Self> {
        if path.exists() {
            let data = std::fs::read(path).map_err(transport_err)?;
            let identity: PersistedIdentity =
                serde_json::from_slice(&data).map_err(transport_err)?;
            let cert = CertificateDer::from(identity.certificate);
            let fingerprint = fingerprint_of(&cert);
            let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.private_key));
            return Ok(Self {
                cert,
                key,
                fingerprint,
            });
        }
        if !allow_new {
            return Err(ChatError::Transport(
                "现有房间的证书身份丢失，不能沿用原房间标识重新生成".into(),
            ));
        }
        let identity = Self::generate()?;
        let data = serde_json::to_vec(&PersistedIdentity {
            certificate: identity.cert.as_ref().to_vec(),
            private_key: identity.key.secret_der().to_vec(),
        })
        .map_err(transport_err)?;
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        let mut file = options.open(path).map_err(transport_err)?;
        file.write_all(&data).map_err(transport_err)?;
        file.sync_all().map_err(transport_err)?;
        Ok(identity)
    }
}

// ---------------------------------------------------------------- 客户端校验

/// 对比完整证书指纹，不做 CA 链校验。
#[derive(Debug)]
struct FingerprintVerifier {
    expected: String,
    provider: Arc<quinn::rustls::crypto::CryptoProvider>,
}

impl quinn::rustls::client::danger::ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &quinn::rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: quinn::rustls::pki_types::UnixTime,
    ) -> std::result::Result<quinn::rustls::client::danger::ServerCertVerified, quinn::rustls::Error>
    {
        let actual = fingerprint_of(end_entity);
        if self.expected != actual {
            return Err(quinn::rustls::Error::General(format!(
                "证书指纹不匹配: 期望 {}, 实际 {actual}",
                self.expected
            )));
        }
        Ok(quinn::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &quinn::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        quinn::rustls::client::danger::HandshakeSignatureValid,
        quinn::rustls::Error,
    > {
        quinn::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &quinn::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        quinn::rustls::client::danger::HandshakeSignatureValid,
        quinn::rustls::Error,
    > {
        quinn::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<quinn::rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn crypto_provider() -> Arc<quinn::rustls::crypto::CryptoProvider> {
    Arc::new(quinn::rustls::crypto::ring::default_provider())
}

// ---------------------------------------------------------------- 身份校验

/// 客户端如何确认对端身份。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerIdentity {
    /// 局域网自签证书：比对固定指纹，不做 CA 链校验。
    Fingerprint(String),
    /// 公网证书：校验受信 CA 链与主机名。
    PublicCa(PublicCa),
}

/// 公网证书的校验参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicCa {
    /// 证书上的主机名，域名与 IP 均可。
    pub server_name: String,
    /// 额外信任的根证书（PEM）。为空时只信任内置公共根。
    pub roots_pem: Option<Vec<u8>>,
}

impl ServerIdentity {
    pub fn fingerprint(value: impl Into<String>) -> Self {
        Self::Fingerprint(value.into())
    }

    pub fn public_ca(server_name: impl Into<String>) -> Self {
        Self::PublicCa(PublicCa {
            server_name: server_name.into(),
            roots_pem: None,
        })
    }

    /// SNI 与证书主机名。指纹模式下证书是自签的，名字只用于满足握手要求。
    fn server_name(&self) -> &str {
        match self {
            Self::Fingerprint(_) => "libchat.local",
            Self::PublicCa(ca) => &ca.server_name,
        }
    }
}

impl PublicCa {
    /// 信任额外的根证书，用于自建 CA 或私有集群。
    pub fn with_roots_pem(mut self, pem: Vec<u8>) -> Self {
        self.roots_pem = Some(pem);
        self
    }
}

fn root_store(extra_pem: Option<&[u8]>) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let Some(pem) = extra_pem else {
        return Ok(roots);
    };
    let certs = rustls_pemfile::certs(&mut &pem[..])
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|e| ChatError::Transport(format!("解析根证书失败: {e}")))?;
    if certs.is_empty() {
        return Err(ChatError::Transport("根证书文件里没有证书".into()));
    }
    for cert in certs {
        roots
            .add(cert)
            .map_err(|e| ChatError::Transport(format!("根证书无效: {e}")))?;
    }
    Ok(roots)
}

fn client_config(identity: &ServerIdentity, liveness: Liveness) -> Result<ClientConfig> {
    let provider = crypto_provider();
    let builder = quinn::rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&quinn::rustls::version::TLS13])
        .map_err(transport_err)?;
    let mut tls = match identity {
        ServerIdentity::Fingerprint(expected) => {
            validate_fingerprint(expected)?;
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(FingerprintVerifier {
                    expected: expected.to_ascii_lowercase(),
                    provider,
                }))
                .with_no_client_auth()
        }
        ServerIdentity::PublicCa(ca) => builder
            .with_root_certificates(root_store(ca.roots_pem.as_deref())?)
            .with_no_client_auth(),
    };
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(transport_err)?;
    let mut config = ClientConfig::new(Arc::new(crypto));
    config.transport_config(liveness.transport());
    Ok(config)
}

/// 拆开 `主机:端口`。IPv6 字面量须写成 `[::1]:4433`。
pub fn parse_host_port(text: &str) -> Result<(String, u16)> {
    let text = text.trim();
    let (host, port) = match text.strip_prefix('[') {
        Some(rest) => rest
            .split_once("]:")
            .ok_or_else(|| ChatError::Transport(format!("地址「{text}」缺少方括号或端口")))?,
        None => text
            .rsplit_once(':')
            .ok_or_else(|| ChatError::Transport(format!("地址「{text}」须为主机:端口")))?,
    };
    if host.is_empty() {
        return Err(ChatError::Transport(format!("地址「{text}」缺少主机名")));
    }
    let port = port
        .parse::<u16>()
        .map_err(|e| ChatError::Transport(format!("地址「{text}」的端口无效: {e}")))?;
    Ok((host.to_string(), port))
}

/// 把 `host:port` 解析成可尝试的地址列表。域名按系统解析顺序给出全部结果。
async fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    let host = host.trim();
    if host.is_empty() {
        return Err(ChatError::Transport("连接地址为空".into()));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| {
            ChatError::Transport(format!(
                "解析 {host}:{port} 失败（域名可能不存在或未配置 DNS）: {e}"
            ))
        })?
        .collect();
    if addrs.is_empty() {
        return Err(ChatError::Transport(format!("{host} 没有可用地址")));
    }
    Ok(addrs)
}

// ---------------------------------------------------------------- 服务端

/// QUIC 服务端：接受连接并把请求转给 [`ConnectionAcceptor`]。
pub struct QuicServer {
    endpoint: Endpoint,
    local_addr: SocketAddr,
    fingerprint: String,
}

impl QuicServer {
    pub fn bind(addr: SocketAddr) -> Result<Self> {
        Self::bind_with(addr, Liveness::default())
    }

    /// 指定活性参数绑定。测试用短超时验证保活行为时会用到。
    pub fn bind_with(addr: SocketAddr, liveness: Liveness) -> Result<Self> {
        Self::bind_with_cert(addr, liveness, SelfSignedCert::generate()?)
    }

    pub fn bind_with_cert(
        addr: SocketAddr,
        liveness: Liveness,
        cert: SelfSignedCert,
    ) -> Result<Self> {
        let fingerprint = cert.fingerprint().to_string();
        Self::bind_with_chain(
            addr,
            liveness,
            vec![cert.cert.clone()],
            cert.key.clone_key(),
            fingerprint,
        )
    }

    /// 用 PEM 证书链与私钥绑定。公网部署交给受信 CA 签发时使用。
    pub fn bind_with_pem(
        addr: SocketAddr,
        liveness: Liveness,
        cert_pem: &[u8],
        key_pem: &[u8],
    ) -> Result<Self> {
        let certs = rustls_pemfile::certs(&mut &cert_pem[..])
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|e| ChatError::Transport(format!("解析证书失败: {e}")))?;
        let Some(leaf) = certs.first() else {
            return Err(ChatError::Transport("证书文件里没有证书".into()));
        };
        let fingerprint = fingerprint_of(leaf);
        let key = rustls_pemfile::private_key(&mut &key_pem[..])
            .map_err(|e| ChatError::Transport(format!("解析私钥失败: {e}")))?
            .ok_or_else(|| ChatError::Transport("私钥文件里没有私钥".into()))?;
        Self::bind_with_chain(addr, liveness, certs, key, fingerprint)
    }

    fn bind_with_chain(
        addr: SocketAddr,
        liveness: Liveness,
        certs: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
        fingerprint: String,
    ) -> Result<Self> {
        let mut tls = quinn::rustls::ServerConfig::builder_with_provider(crypto_provider())
            .with_protocol_versions(&[&quinn::rustls::version::TLS13])
            .map_err(transport_err)?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(transport_err)?;
        tls.alpn_protocols = vec![ALPN.to_vec()];

        let crypto =
            quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(transport_err)?;
        let mut config = ServerConfig::with_crypto(Arc::new(crypto));
        config.transport_config(liveness.transport());
        let endpoint = Endpoint::server(config, addr).map_err(transport_err)?;
        let local_addr = endpoint.local_addr().map_err(transport_err)?;

        Ok(Self {
            endpoint,
            local_addr,
            fingerprint,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// 证书指纹，随 mDNS 一起广播给客户端。
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// 接受连接直到 endpoint 关闭。
    pub async fn serve(self: Arc<Self>, acceptor: Arc<dyn ConnectionAcceptor>) {
        while let Some(incoming) = self.endpoint.accept().await {
            let acceptor = Arc::clone(&acceptor);
            tokio::spawn(async move {
                if let Err(error) = handle_connection(incoming, acceptor).await {
                    // 探测、切房、对端退出都会让连接半途结束，默认不打扰；
                    // 需要排查时用 LIBCHAT_DEBUG=1 看全部结果。
                    if !is_routine_end(&error) || std::env::var_os("LIBCHAT_DEBUG").is_some() {
                        eprintln!("连接结束: {error}");
                    }
                }
            });
        }
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"bye");
    }

    pub async fn wait_idle(&self) {
        self.endpoint.wait_idle().await;
    }
}

async fn handle_connection(
    incoming: quinn::Incoming,
    acceptor: Arc<dyn ConnectionAcceptor>,
) -> Result<()> {
    let conn = tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming)
        .await
        .map_err(|_| handshake_abort("等待连接建立"))?
        .map_err(transport_err)?;
    let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.accept_bi())
        .await
        .map_err(|_| handshake_abort("等待请求流"))?
        .map_err(transport_err)?;

    let device = match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame::<ClientFrame>(&mut recv))
        .await
        .map_err(|_| handshake_abort("等待 hello"))??
    {
        ClientFrame::Hello { device, version } if version == PROTOCOL_VERSION => device,
        ClientFrame::Hello { version, .. } => {
            return Err(ChatError::Protocol(format!("协议版本不兼容: {version}")));
        }
        ClientFrame::Request(_) => {
            return Err(ChatError::Protocol("首帧必须是 hello".into()));
        }
    };

    let room = acceptor.room_info();
    let (sink, mut events) = acceptor.accept(device).await;
    if let Err(error) = write_frame(
        &mut send,
        &ServerFrame::Welcome {
            version: PROTOCOL_VERSION,
            room,
        },
    )
    .await
    {
        sink.detach().await;
        return Err(error);
    }

    // 附件走独立双向流，和聊天请求互不阻塞
    let blob_conn = conn.clone();
    let blob_sink = Arc::clone(&sink);
    tokio::spawn(async move {
        while let Ok((mut send, mut recv)) = blob_conn.accept_bi().await {
            let sink = Arc::clone(&blob_sink);
            tokio::spawn(async move {
                if let Err(error) = serve_blob_stream(&mut send, &mut recv, sink.as_ref()).await {
                    let _ = write_frame(
                        &mut send,
                        &BlobReply::Error {
                            message: error.to_string(),
                        },
                    )
                    .await;
                    let _ = send.finish();
                }
            });
        }
    });

    let writer = tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            if !matches!(
                tokio::time::timeout(
                    Duration::from_secs(5),
                    write_frame(&mut send, &ServerFrame::Event(ev))
                )
                .await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
        let _ = send.finish();
    });
    tokio::pin!(writer);
    loop {
        tokio::select! {
            _ = &mut writer => break,
            request = read_frame::<ClientFrame>(&mut recv) => match request {
                Ok(ClientFrame::Request(req)) => {
                    if sink.dispatch(req).await.is_err() {
                        break;
                    }
                }
                Ok(ClientFrame::Hello { .. }) => {},
                Err(_) => break,
            }
        }
    }

    conn.close(0u32.into(), b"closed");
    sink.detach().await;
    writer.abort();
    Ok(())
}

/// 处理一条附件流：上传或下载。
async fn serve_blob_stream(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    sink: &dyn RequestSink,
) -> Result<()> {
    match read_frame::<BlobRequest>(recv).await? {
        BlobRequest::Upload { room, attachment } => {
            let mut data = Vec::with_capacity(attachment.size.min(MAX_ATTACHMENT_BYTES) as usize);
            loop {
                let chunk = read_chunk(recv).await?;
                if chunk.is_empty() {
                    break;
                }
                if (data.len() + chunk.len()) as u64 > MAX_ATTACHMENT_BYTES {
                    return Err(ChatError::Protocol("附件超过大小上限".into()));
                }
                data.extend_from_slice(&chunk);
            }
            sink.upload(room, attachment, data).await?;
            write_frame(send, &BlobReply::Done).await?;
        }
        BlobRequest::Download { room, id } => {
            let data = sink.download(room, id).await?;
            write_frame(
                send,
                &BlobReply::Begin {
                    size: data.len() as u64,
                },
            )
            .await?;
            for chunk in data.chunks(BLOB_CHUNK) {
                write_chunk(send, chunk).await?;
            }
            write_chunk(send, &[]).await?;
        }
    }
    let _ = send.finish();
    Ok(())
}

// ---------------------------------------------------------------- 客户端
/// 使用经带外核对或已固定的指纹建立 QUIC 连接。
pub async fn connect(addr: SocketAddr, expected_fingerprint: &str) -> Result<QuicConnection> {
    connect_with(addr, expected_fingerprint, Liveness::default()).await
}

/// 指定活性参数连接。测试用短超时验证保活行为时会用到。
pub async fn connect_with(
    addr: SocketAddr,
    expected_fingerprint: &str,
    liveness: Liveness,
) -> Result<QuicConnection> {
    connect_to(
        &addr.ip().to_string(),
        addr.port(),
        &ServerIdentity::fingerprint(expected_fingerprint),
        liveness,
    )
    .await
}

/// 按主机名（或 IP）、端口与身份校验方式建立连接。
///
/// 域名解析出的每个地址都会尝试一次：本机同时有 IPv6 与 IPv4 记录、
/// 或服务端只监听其中一族时，靠这一步挑出真正能连上的那个。
pub async fn connect_to(
    host: &str,
    port: u16,
    identity: &ServerIdentity,
    liveness: Liveness,
) -> Result<QuicConnection> {
    let addrs = resolve(host, port).await?;
    let config = client_config(identity, liveness)?;
    let mut last_error = None;

    for addr in addrs {
        let bind = if addr.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        };
        let mut endpoint = match Endpoint::client(bind.parse().expect("合法绑定地址")) {
            Ok(endpoint) => endpoint,
            Err(e) => {
                last_error = Some(transport_err(e));
                continue;
            }
        };
        endpoint.set_default_client_config(config.clone());
        match handshake(endpoint, addr, identity.server_name()).await {
            Ok(conn) => return Ok(conn),
            Err(e) => last_error = Some(e),
        }
    }

    Err(last_error.unwrap_or_else(|| ChatError::Transport("没有可用的连接地址".into())))
}

/// 在一条连接上完成 hello / welcome 握手，之后把事件读进通道。
async fn handshake(
    endpoint: Endpoint,
    addr: SocketAddr,
    server_name: &str,
) -> Result<QuicConnection> {
    let conn = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        endpoint.connect(addr, server_name).map_err(transport_err)?,
    )
    .await
    .map_err(|_| ChatError::Protocol("连接超时".into()))?
    .map_err(transport_err)?;
    let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.open_bi())
        .await
        .map_err(|_| ChatError::Protocol("打开请求流超时".into()))?
        .map_err(transport_err)?;

    let device = DeviceId::new();
    write_frame(
        &mut send,
        &ClientFrame::Hello {
            device,
            version: PROTOCOL_VERSION,
        },
    )
    .await?;
    let room = match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame::<ServerFrame>(&mut recv))
        .await
        .map_err(|_| ChatError::Protocol("等待欢迎消息超时".into()))??
    {
        ServerFrame::Welcome { version, room } if version == PROTOCOL_VERSION => room,
        _ => return Err(ChatError::Protocol("握手失败".into())),
    };

    let (tx, events) = tokio::sync::mpsc::channel(chat_core::EVENT_QUEUE_CAPACITY);
    let reader_conn = conn.clone();
    tokio::spawn(async move {
        loop {
            match read_frame::<ServerFrame>(&mut recv).await {
                Ok(ServerFrame::Event(ev)) => {
                    if !matches!(
                        tokio::time::timeout(Duration::from_secs(5), tx.send(ev)).await,
                        Ok(Ok(()))
                    ) {
                        reader_conn.close(0u32.into(), b"slow client");
                        break;
                    }
                }
                Ok(ServerFrame::Welcome { .. }) => {}
                Err(_) => break,
            }
        }
    });

    Ok(QuicConnection {
        room,
        write: Mutex::new(send),
        events: Mutex::new(events),
        conn,
        endpoint,
        closed: AtomicBool::new(false),
    })
}

/// QUIC 客户端连接。
pub struct QuicConnection {
    room: RoomInfo,
    write: Mutex<quinn::SendStream>,
    events: Mutex<EventStream>,
    /// 保留连接句柄，附件走额外的双向流。
    conn: quinn::Connection,
    endpoint: Endpoint,
    closed: AtomicBool,
}

impl QuicConnection {
    /// 握手时获知的房间信息。
    pub fn room_info(&self) -> &RoomInfo {
        &self.room
    }
}

#[async_trait::async_trait]
impl Connection for QuicConnection {
    async fn request(&self, req: ClientRequest) -> Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(ChatError::Closed);
        }
        let mut send = self.write.lock().await;
        write_frame(&mut send, &ClientFrame::Request(req)).await
    }

    async fn next_event(&self) -> Result<ServerEvent> {
        let mut events = self.events.lock().await;
        events.recv().await.ok_or(ChatError::Closed)
    }

    async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        self.endpoint.close(0u32.into(), b"bye");
        Ok(())
    }

    async fn upload(&self, room: RoomId, attachment: Attachment, data: Vec<u8>) -> Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(ChatError::Closed);
        }
        let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE_TIMEOUT, self.conn.open_bi())
            .await
            .map_err(|_| ChatError::Protocol("打开附件流超时".into()))?
            .map_err(transport_err)?;
        let transfer = async {
            write_frame(&mut send, &BlobRequest::Upload { room, attachment }).await?;
            for chunk in data.chunks(BLOB_CHUNK) {
                write_chunk(&mut send, chunk).await?;
            }
            write_chunk(&mut send, &[]).await?;
            match read_frame::<BlobReply>(&mut recv).await? {
                BlobReply::Done => Ok(()),
                BlobReply::Error { message } => Err(ChatError::Protocol(message)),
                _ => Err(ChatError::Protocol("附件上传应答异常".into())),
            }
        };
        tokio::time::timeout(BLOB_TIMEOUT, transfer)
            .await
            .map_err(|_| ChatError::Protocol("附件上传超时".into()))?
    }

    async fn download(&self, room: RoomId, id: AttachmentId) -> Result<Vec<u8>> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(ChatError::Closed);
        }
        let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE_TIMEOUT, self.conn.open_bi())
            .await
            .map_err(|_| ChatError::Protocol("打开附件流超时".into()))?
            .map_err(transport_err)?;
        let transfer = async {
            write_frame(&mut send, &BlobRequest::Download { room, id }).await?;
            match read_frame::<BlobReply>(&mut recv).await? {
                BlobReply::Begin { size } => {
                    let mut data = Vec::with_capacity(size.min(MAX_ATTACHMENT_BYTES) as usize);
                    loop {
                        let chunk = read_chunk(&mut recv).await?;
                        if chunk.is_empty() {
                            break;
                        }
                        if (data.len() + chunk.len()) as u64 > MAX_ATTACHMENT_BYTES {
                            return Err(ChatError::Protocol("附件超过大小上限".into()));
                        }
                        data.extend_from_slice(&chunk);
                    }
                    Ok(data)
                }
                BlobReply::Error { message } => Err(ChatError::Protocol(message)),
                _ => Err(ChatError::Protocol("附件下载应答异常".into())),
            }
        };
        tokio::time::timeout(BLOB_TIMEOUT, transfer)
            .await
            .map_err(|_| ChatError::Protocol("附件下载超时".into()))?
    }
}
#[cfg(test)]
mod tests {
    use super::parse_host_port;

    #[test]
    fn parses_host_names_ips_and_ipv6_literals() {
        assert_eq!(
            parse_host_port("chat.example.com:4433").unwrap(),
            ("chat.example.com".into(), 4433)
        );
        assert_eq!(
            parse_host_port("127.0.0.1:52000").unwrap(),
            ("127.0.0.1".into(), 52000)
        );
        assert_eq!(parse_host_port("[::1]:4433").unwrap(), ("::1".into(), 4433));
    }

    #[test]
    fn rejects_addresses_without_a_port_or_host() {
        assert!(parse_host_port("chat.example.com").is_err());
        assert!(parse_host_port(":4433").is_err());
        assert!(parse_host_port("chat.example.com:http").is_err());
        assert!(parse_host_port("[::1]4433").is_err());
    }
}
