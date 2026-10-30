//! 局域网与公网聊天工具的命令行形态，用于在没有界面时验证整条链路。
//!
//! ```text
//! libchat host [--port 0] [--name 房间名]
//! libchat serve [--listen 0.0.0.0:4433] [--name 房间名] [--cert 证书 --key 私钥]
//! libchat join [--server 主机:端口] [--fingerprint 指纹 | --ca 根证书] [--name 昵称]
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use chat_cli::{default_data_dir, serve, ServeConfig, ServerTls};
use chat_core::Client;
use chat_core::{Connection, ConnectionAcceptor};
use chat_core::{RoomId, ServerEvent};
use chat_discovery::{RoomAnnouncement, RoomAnnouncer, RoomBrowser, PROTOCOL_VERSION};

use chat_transport_quic::{
    connect_to, parse_host_port, validate_fingerprint, Liveness, PublicCa, QuicServer,
    ServerIdentity, TrustStore,
};
use tokio::io::{AsyncBufReadExt, BufReader};

type AnyErr = Box<dyn std::error::Error + Send + Sync>;

fn opt(args: &[String], key: &str) -> Option<String> {
    let pos = args.iter().position(|a| a == key)?;
    args.get(pos + 1).cloned()
}

/// 本机可被局域网访问的 IPv4 地址。
fn local_addrs(port: u16) -> Vec<SocketAddr> {
    if_addrs::get_if_addrs()
        .map(|ifs| {
            ifs.into_iter()
                .filter(|i| !i.is_loopback() && i.is_oper_up() && i.ip().is_ipv4())
                .map(|i| SocketAddr::new(i.ip(), port))
                .collect()
        })
        .unwrap_or_default()
}

fn usage() {
    eprintln!(
        "用法:\n  \
         libchat host [--port 0] [--name 房间名]\n  \
         libchat serve [--listen 0.0.0.0:4433] [--name 房间名] [--cert 证书链.pem --key 私钥.pem] [--data-dir 目录] [--announce 广播端口]\n  \
         libchat register --server 主机:端口 [--fingerprint SHA-256 | --ca 根证书.pem] --user 账号 --password 密码 [--name 昵称]\n  \
         libchat admin users|rooms|health [--data-dir 目录]\n  \
         libchat admin ban|unban --user 账号 [--data-dir 目录]\n  \
         libchat admin clear --room 房间标识 [--data-dir 目录]\n  \
         libchat join [--server 主机:端口 | --addr 主机:端口] [--fingerprint SHA-256 | --ca 根证书.pem] --user 账号 (--password 密码 | --token 令牌 | --register) [--name 昵称] [--data-dir 目录]"
    );
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("host") => host(&args[1..]).await,
        Some("serve") => serve_cmd(&args[1..]).await,
        Some("register") => register_cmd(&args[1..]).await,
        Some("admin") => admin_cmd(&args[1..]).await,
        Some("join") => join(&args[1..]).await,
        _ => {
            usage();
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("错误: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn serve_cmd(args: &[String]) -> Result<(), AnyErr> {
    let listen: SocketAddr = opt(args, "--listen")
        .unwrap_or_else(|| "0.0.0.0:4433".into())
        .parse()?;
    let name = opt(args, "--name").unwrap_or_else(|| "公网聊天室".into());
    let data_dir = match opt(args, "--data-dir") {
        Some(path) => PathBuf::from(path),
        None => default_data_dir()?,
    };
    let tls = match (opt(args, "--cert"), opt(args, "--key")) {
        (Some(cert), Some(key)) => ServerTls::Pem {
            cert: cert.into(),
            key: key.into(),
        },
        (None, None) => ServerTls::SelfSigned,
        _ => return Err("--cert 与 --key 必须同时给出".into()),
    };
    let self_signed = matches!(tls, ServerTls::SelfSigned);
    let announce_port = opt(args, "--announce")
        .map(|p| p.parse::<u16>())
        .transpose()?;

    let serving = serve(ServeConfig {
        listen,
        name: name.clone(),
        data_dir,
        tls,
        announce_port,
    })
    .await?;

    println!("房间: {}（{name}）", serving.room);
    println!("监听: {}", serving.addr);
    println!("证书 SHA-256: {}", serving.fingerprint);
    if self_signed {
        println!(
            "加入: libchat join --server {} --fingerprint {} --user <账号> --password <密码>",
            serving.addr, serving.fingerprint
        );
    } else {
        println!(
            "加入: libchat join --server {} --user <账号> --password <密码>（受信 CA 直接校验，私有 CA 加 --ca）",
            serving.addr
        );
    }
    if announce_port.is_some() {
        println!("已在本机广播房间，局域网客户端可直接发现");
    }
    println!("按 Ctrl+C 退出");

    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        println!("[状态] 在线 {}", serving.hub.online_count().await);
    }
}

async fn host(args: &[String]) -> Result<(), AnyErr> {
    let name = opt(args, "--name").unwrap_or_else(|| "局域网聊天室".into());
    let port: u16 = opt(args, "--port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);

    let data_dir = match opt(args, "--data-dir") {
        Some(path) => PathBuf::from(path),
        None => default_data_dir()?,
    };
    let hub = chat_server::Hub::open(&data_dir, &name).await?;
    let quic = Arc::new(QuicServer::bind(SocketAddr::from(([0, 0, 0, 0], port)))?);
    let addr = quic.local_addr();
    let fingerprint = quic.fingerprint().to_string();

    let acceptor: Arc<dyn ConnectionAcceptor> =
        Arc::new(chat_server::HubAcceptor(Arc::clone(&hub)));
    tokio::spawn(Arc::clone(&quic).serve(acceptor));

    let _advertiser =
        match RoomAnnouncer::start(hub.default_room(), &name, addr.port(), &fingerprint) {
            Ok(a) => {
                println!("已广播房间（mDNS + UDP），局域网客户端可直接发现");
                Some(a)
            }
            Err(e) => {
                eprintln!("广播失败，客户端请用 --addr 手动连接: {e}");
                None
            }
        };

    println!("房间: {name}");
    println!("监听: {addr}");
    println!("指纹: {fingerprint}");
    let lan = local_addrs(addr.port());
    if lan.is_empty() {
        eprintln!("未找到局域网 IPv4 地址，客户端请依赖 mDNS 发现");
    }
    for a in &lan {
        println!("加入: libchat join --addr {a} --fingerprint {fingerprint} --user <账号> --password <密码>");
    }
    println!("按 Ctrl+C 退出");

    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        println!("[状态] 在线 {}", hub.online_count().await);
    }
}

fn discovered_target(
    ann: RoomAnnouncement,
    pinned_fingerprint: Option<String>,
) -> Result<(String, u16, String), AnyErr> {
    if ann.version != PROTOCOL_VERSION {
        return Err(format!("协议版本不兼容: {}", ann.version).into());
    }
    let addr = *ann.addrs.first().ok_or("房间未提供可用地址")?;
    let fingerprint = pinned_fingerprint
        .or_else(|| (!ann.fingerprint.is_empty()).then_some(ann.fingerprint))
        .ok_or("发现的房间缺少证书指纹")?;
    Ok((addr.ip().to_string(), addr.port(), fingerprint))
}

/// 公网连接默认走受信 CA；给了指纹就退回局域网那套固定指纹校验。
fn identity_for(
    host: &str,
    fingerprint: Option<String>,
    ca: Option<&str>,
) -> Result<ServerIdentity, AnyErr> {
    if let Some(fingerprint) = fingerprint {
        validate_fingerprint(&fingerprint)?;
        return Ok(ServerIdentity::fingerprint(
            fingerprint.to_ascii_lowercase(),
        ));
    }
    let mut public = PublicCa {
        server_name: host.to_string(),
        roots_pem: None,
    };
    if let Some(path) = ca {
        public.roots_pem = Some(std::fs::read(path)?);
    }
    Ok(ServerIdentity::PublicCa(public))
}

async fn join(args: &[String]) -> Result<(), AnyErr> {
    let name = opt(args, "--name").unwrap_or_else(|| "我".into());
    let data_dir = match opt(args, "--data-dir") {
        Some(path) => PathBuf::from(path),
        None => default_data_dir()?,
    };
    let trust = TrustStore::new(&data_dir);
    let explicitly_pinned = opt(args, "--fingerprint");
    let ca = opt(args, "--ca");

    let (host, port, identity, expected_room) =
        match opt(args, "--server").or_else(|| opt(args, "--addr")) {
            Some(text) => {
                let (host, port) = parse_host_port(&text)?;
                let identity = identity_for(&host, explicitly_pinned.clone(), ca.as_deref())?;
                (host, port, identity, None)
            }
            None => {
                println!("正在搜索局域网房间...");
                let mut browser = RoomBrowser::start()?;
                let ann = tokio::time::timeout(Duration::from_secs(15), browser.next_room())
                    .await
                    .map_err(|_| "搜索房间超时，可用 --server 手动指定")??;
                let room = ann.room;
                let room_name = ann.name.clone();
                let announced = ann.fingerprint.clone();
                let (host, port, fingerprint) = discovered_target(ann, explicitly_pinned.clone())?;
                if announced != fingerprint && explicitly_pinned.is_none() {
                    return Err("发现的房间指纹不一致".into());
                }
                println!("发现房间「{room_name}」于 {host}:{port}");
                (
                    host,
                    port,
                    ServerIdentity::fingerprint(fingerprint),
                    Some(room),
                )
            }
        };

    // 固定指纹是带外核对过的身份，首次连接要确认、之后按房间记住；
    // 公网受信 CA 由证书链保证身份，不需要这一步。
    if let (ServerIdentity::Fingerprint(fingerprint), Some(room)) = (&identity, expected_room) {
        match trust.get(room)? {
            Some(pinned) if pinned != *fingerprint => {
                return Err(format!("房间 {room} 的指纹已变更，拒绝连接").into());
            }
            Some(_) => {}
            None if explicitly_pinned.is_none() => {
                println!("首次连接房间 {room} 于 {host}:{port}\n证书 SHA-256: {fingerprint}\n请向房主通过其他渠道核对房间标识和完整指纹。输入 yes 确认: ");
                let mut lines = BufReader::new(tokio::io::stdin()).lines();
                if lines.next_line().await?.as_deref() != Some("yes") {
                    return Err("已取消首次信任".into());
                }
            }
            None => {}
        }
    }

    let conn = Arc::new(connect_to(&host, port, &identity, Liveness::default()).await?);
    let room: RoomId = conn.room_info().room;
    if expected_room.is_some_and(|expected| expected != room) {
        return Err("公告房间标识与实际连接不一致".into());
    }
    if let ServerIdentity::Fingerprint(fingerprint) = &identity {
        if let Some(existing) = trust.get(room)? {
            if existing != *fingerprint {
                return Err(format!("房间 {room} 的指纹已变更，拒绝连接").into());
            }
        }
    }
    println!("已连接房间「{}」（{}）", conn.room_info().name, room);

    let conn_dyn: Arc<dyn Connection> = conn.clone();
    let client = Client::new(conn_dyn);

    // Hub 要求所有连接先登录；局域网发现到房间后也一样
    let user = opt(args, "--user").ok_or("需要 --user（账号）与 --password，或用 --token")?;
    let password = opt(args, "--password").unwrap_or_default();
    if let Some(token) = opt(args, "--token") {
        client.resume(token).await?;
    } else if args.iter().any(|arg| arg == "--register") {
        client.register(&user, &password, &name).await?;
    } else {
        if password.is_empty() {
            return Err("需要 --password（或用 --token/--register）".into());
        }
        client.login(&user, &password).await?;
    }
    loop {
        match client.next_event().await? {
            ServerEvent::Authenticated { user, .. } => {
                println!("已登录为 {}（{}）", user.display_name, user.username);
                break;
            }
            ServerEvent::Error { message, .. } => return Err(message.into()),
            ServerEvent::AuthRequired => return Err("登录失败：未通过认证".into()),
            _ => {}
        }
    }

    // 未加入先说清人数：大厅只给摘要，不给消息
    client.lobby(room).await?;
    let Some(ServerEvent::Lobby { info }) = next_lobby(&client).await else {
        return Err("房间大厅不可用，拒绝建立信任".into());
    };
    if info.room != room {
        return Err("大厅房间标识与连接不一致".into());
    }
    if let ServerIdentity::Fingerprint(fingerprint) = &identity {
        trust.pin(room, fingerprint)?;
    }
    println!("当前在线 {} 人，输入 join 进入房间", info.online_count);

    // 事件打印任务
    let events_client = client.clone();
    tokio::spawn(async move {
        loop {
            match events_client.next_event().await {
                Ok(ServerEvent::Message { message }) => {
                    println!("[{}] {}", message.sender_name, message.text)
                }
                Ok(ServerEvent::Presence { online_count, .. }) => {
                    println!("[系统] 在线 {online_count} 人")
                }
                Ok(ServerEvent::Joined { online_count, .. }) => {
                    println!("[系统] 已加入，当前在线 {online_count} 人")
                }
                Ok(ServerEvent::Left { .. }) => println!("[系统] 已退出房间"),
                // 未加入（或刚退出）时服务端推的是摘要，同样要显示人数变化
                Ok(ServerEvent::Lobby { info }) => {
                    println!("[系统] 在线 {} 人", info.online_count)
                }
                Ok(ServerEvent::Error { message, .. }) => println!("[错误] {message}"),
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    print!("> ");
    std::io::Write::flush(&mut std::io::stdout())?;
    while let Some(line) = lines.next_line().await? {
        let text = line.trim();
        if text.is_empty() {
            print!("> ");
            std::io::Write::flush(&mut std::io::stdout())?;
            continue;
        }
        match text {
            "join" => client.join(room, name.clone()).await?,
            "leave" => client.leave(room).await?,
            "quit" | "exit" => break,
            other => client.send_text(room, other).await?,
        }
        print!("> ");
        std::io::Write::flush(&mut std::io::stdout())?;
    }

    client.close().await.ok();
    Ok(())
}

async fn next_lobby(client: &Client) -> Option<ServerEvent> {
    for _ in 0..16 {
        let ev = tokio::time::timeout(Duration::from_secs(2), client.next_event())
            .await
            .ok()?
            .ok()?;
        if matches!(ev, ServerEvent::Lobby { .. }) {
            return Some(ev);
        }
    }
    None
}

/// 连接服务端注册账号，打印可复用的登录令牌。
async fn register_cmd(args: &[String]) -> Result<(), AnyErr> {
    let server = opt(args, "--server")
        .or_else(|| opt(args, "--addr"))
        .ok_or("需要 --server 主机:端口")?;
    let (host, port) = parse_host_port(&server)?;
    let identity = identity_for(
        &host,
        opt(args, "--fingerprint"),
        opt(args, "--ca").as_deref(),
    )?;
    let username = opt(args, "--user").ok_or("需要 --user")?;
    let password = opt(args, "--password").ok_or("需要 --password")?;
    let display = opt(args, "--name").unwrap_or_else(|| username.clone());

    let quic = connect_to(&host, port, &identity, Liveness::default()).await?;
    println!("已连接 {}", quic.room_info().name);
    let conn: Arc<dyn Connection> = Arc::new(quic);
    let client = Client::new(conn);
    client.register(&username, &password, &display).await?;
    loop {
        match client.next_event().await? {
            ServerEvent::Authenticated { user, token } => {
                println!("注册成功: {} ({})", user.display_name, user.username);
                println!("令牌: {token}");
                println!(
                    "加入: libchat join --server {server} --user {} --password <密码>",
                    user.username
                );
                break;
            }
            ServerEvent::Error { message, .. } => return Err(message.into()),
            ServerEvent::AuthRequired => return Err("注册失败：服务端未返回登录态".into()),
            _ => {}
        }
    }
    Ok(())
}

/// 本地服务端管理：直接操作数据目录，不需要服务端进程。
async fn admin_cmd(args: &[String]) -> Result<(), AnyErr> {
    let action = args
        .first()
        .map(String::as_str)
        .ok_or("需要 users/rooms/ban/unban/clear/health")?;
    let dir = match opt(args, "--data-dir") {
        Some(path) => PathBuf::from(path),
        None => default_data_dir()?,
    };
    let accounts = chat_server::Accounts::open(&dir.join("accounts.db")).await?;
    match action {
        "users" => {
            for user in accounts.admin_users().await? {
                println!("{}\t{}", user.username, user.display_name);
            }
        }
        "rooms" => {
            for room in accounts.admin_rooms().await? {
                println!(
                    "{}\t{:?}\t{}\t{} 人",
                    room.room, room.kind, room.name, room.member_count
                );
            }
        }
        "ban" | "unban" => {
            let username = opt(args, "--user").ok_or("需要 --user 账号")?;
            let banned = action == "ban";
            if !accounts.set_banned(&username, banned).await? {
                return Err(format!("账号不存在: {username}").into());
            }
            println!("{}: {username}", if banned { "已封禁" } else { "已解封" });
        }
        "clear" => {
            let room: RoomId = opt(args, "--room")
                .ok_or("需要 --room 房间标识")?
                .parse()
                .map_err(|e| format!("房间标识无效: {e}"))?;
            let messages = chat_server::SqliteStore::open(&dir.join("messages.db")).await?;
            let count = chat_core::MessageStore::clear(&messages, room).await?;
            messages.close().await;
            println!("已清空 {room} 的 {count} 条消息");
        }
        "health" => {
            let users = accounts.admin_users().await?.len();
            let rooms = accounts.admin_rooms().await?.len();
            println!("数据目录: {}", dir.display());
            println!("账号: {users}，房间: {rooms}");
            println!("accounts.db / messages.db 可读");
        }
        _ => return Err(format!("未知管理命令: {action}").into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn announcement(fingerprint: &str) -> RoomAnnouncement {
        RoomAnnouncement {
            room: RoomId::new(),
            name: "测试房间".into(),
            version: PROTOCOL_VERSION,
            fingerprint: fingerprint.into(),
            addrs: vec!["127.0.0.1:45678".parse().unwrap()],
        }
    }

    #[test]
    fn discovered_room_uses_its_fingerprint_unless_explicitly_pinned() {
        let (_, _, fingerprint) = discovered_target(announcement("advertised"), None).unwrap();
        assert_eq!(fingerprint, "advertised");

        let (_, _, fingerprint) =
            discovered_target(announcement("advertised"), Some("pinned".into())).unwrap();
        assert_eq!(fingerprint, "pinned");
    }

    #[test]
    fn discovery_without_fingerprint_is_rejected() {
        assert!(discovered_target(announcement(""), None).is_err());
    }

    #[test]
    fn server_address_without_fingerprint_targets_a_public_ca() {
        let identity = identity_for("chat.example.com", None, None).unwrap();
        assert_eq!(
            identity,
            ServerIdentity::PublicCa(PublicCa {
                server_name: "chat.example.com".into(),
                roots_pem: None,
            })
        );
    }

    #[test]
    fn explicit_fingerprint_keeps_the_trust_on_pin_mode() {
        let fingerprint = "a".repeat(64);
        let identity = identity_for("chat.example.com", Some(fingerprint.clone()), None).unwrap();
        assert_eq!(identity, ServerIdentity::Fingerprint(fingerprint));
        assert!(identity_for("chat.example.com", Some("short".into()), None).is_err());
    }

    #[test]
    fn ca_file_is_read_for_private_public_servers() {
        let dir = std::env::temp_dir().join(format!("libchat-ca-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ca.pem");
        std::fs::write(&path, b"CA").unwrap();

        match identity_for("chat.example.com", None, path.to_str()).unwrap() {
            ServerIdentity::PublicCa(ca) => {
                assert_eq!(ca.roots_pem.as_deref(), Some(&b"CA"[..]));
                assert_eq!(ca.server_name, "chat.example.com");
            }
            other => panic!("期望受信 CA 身份: {other:?}"),
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
