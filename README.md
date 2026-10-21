# lib-chat

基于 QUIC 的单房间聊天，同一套服务端代码支持两种部署：

- **局域网版本**：免配置发现，自签证书 + 指纹固定，界面内一键建房。
- **公网版本**：独立服务端进程，域名 + 受信 CA 证书，客户端手动连接或从地址簿自动探测。

当前处于开发阶段，协议、存储与界面都还可能做破坏性调整。

## 功能一览

- 文字消息：服务端分配全局递增序号，客户端去重、补齐断线期间的消息。
- 在线人数：大厅摘要、加入/退出广播、断线重连后恢复身份。
- 文件消息：≤64 MiB，独立 QUIC 流分块传输，按 SHA-256 内容寻址去重，点击另存。
- 历史消息：加入时取最近一页，之后可往前翻。
- 局域网发现：UDP 广播 + 本机回环，自动列出房间与人数。
- 公网地址簿：连过的服务器自动保存，启动后周期探测并出现在侧栏。
- 公网托管：界面里填端口与证书 PEM 即可启动服务端，也支持自签；可选域名自动接入。
- 两种身份校验：自签指纹固定；受信 CA 证书链 + 主机名校验。

## 环境要求

- macOS（界面基于 GPUI，目前只在 macOS 上验证过）。
- Rust 1.85 或更新（界面 crate 使用 edition 2024）。
- 构建：

```bash
cargo build --workspace
```

## 局域网版本

### 界面启动与验证

```bash
# 终端 1：第一个实例
cargo run -p chat-ui-gpui
```

1. 点侧栏「新建房间」，界面会自动接入并成为房主。
2. 再开一个实例（同一台机器也可以）：

```bash
# 终端 2：第二个实例
cargo run -p chat-ui-gpui
```

3. 新实例左侧「局域网房间」会自动出现刚建的房间和在线人数，点一下连接，再点「加入房间」。
4. 两个窗口互发消息，观察在线人数与消息实时同步。

若网络环境阻断发现，点侧栏底部「手动连接」展开面板，填 `127.0.0.1:端口`，指纹填房主界面上展示的 SHA-256。

### 命令行验证

```bash
# 房主
cargo run -p chat-cli -- host --name 局域网聊天室

# 另一个终端：自动发现
cargo run -p chat-cli -- join

# 或不依赖发现，手动指定地址与指纹
cargo run -p chat-cli -- join --addr 10.0.0.5:45678 --fingerprint <hex>
```

交互命令：`join` 加入、`leave` 退出、`quit` 退出程序，其余输入按文本消息发送。

自动发现到新房间时，第一次连接会打印房间标识与完整指纹并要求输入 `yes` 核对；之后按房间记住。手动 `--addr` + `--fingerprint` 被认为已经带外核对过，不再询问。

## 公网版本

> 下文出现的 `chat.example.com` 是**占位域名**，没有注册、无法解析。本地验证请换成 `127.0.0.1:端口`；公网部署请换成你自己的、已把 A/AAAA 记录指向服务器的域名。

### 快速本地验证（自签证书）

服务端不给 `--cert/--key` 时自动生成自签证书，并打印客户端需要固定的指纹：

```bash
# 终端 1：服务端
cargo run -p chat-cli -- serve \
  --listen 127.0.0.1:45123 \
  --name 本地公网房间 \
  --data-dir /tmp/libchat-live
```

输出示例：

```
房间: c700c006-8ef0-4b8d-8ceb-705eaf31ac50（本地公网房间）
监听: 127.0.0.1:45123
证书 SHA-256: d74689ab...
```

```bash
# 终端 2：客户端 A
cargo run -p chat-cli -- join \
  --server 127.0.0.1:45123 \
  --fingerprint d74689ab... \
  --name Alice

# 终端 3：客户端 B
cargo run -p chat-cli -- join \
  --server 127.0.0.1:45123 \
  --fingerprint d74689ab... \
  --name Bob
```

两端输入 `join` 后互发消息；退出后再启动服务端（同一个 `--data-dir`），房间标识、证书指纹与历史消息都会保留。

### 正式部署（受信 CA）

服务端使用域名证书（如 Let's Encrypt 的 `fullchain.pem` 与 `privkey.pem`）：

```bash
cargo run -p chat-cli -- serve \
  --listen 0.0.0.0:4433 \
  --name 公网聊天室 \
  --cert /etc/letsencrypt/live/chat.example.com/fullchain.pem \
  --key /etc/letsencrypt/live/chat.example.com/privkey.pem \
  --data-dir /var/lib/libchat
```

客户端不需要指纹，证书链与主机名由受信根校验：

```bash
cargo run -p chat-cli -- join --server chat.example.com:4433 --name Alice
```

私有 CA（自建 PKI）用 `--ca` 指定根证书；界面客户端用环境变量：

```bash
cargo run -p chat-cli -- join --server chat.internal.example:4433 --ca ./ca.pem
LIBCHAT_CA_FILE=./ca.pem cargo run -p chat-ui-gpui
```

### 界面内启动公网服务

侧栏「我的房间」标题右侧的「公网服务」会展开一个表单，不需要命令行：

| 字段 | 说明 |
|---|---|
| 监听端口 | 默认 `4433`；用 443 需要相应权限 |
| 证书链 PEM | 留空则自签；填 `fullchain.pem` 则走受信 CA |
| 私钥 PEM | 与证书同时填写或同时留空 |
| 域名 | 可选。填了会在启动后以客户端身份自动接入，并作为加入地址显示 |

点「启动公网服务」后：

- 侧栏「我的房间」显示端口与证书类型，展开详情能看到接入方式与完整 SHA-256。
- 详情里的「接入地址」「证书 SHA-256」「监听地址」右侧都有「复制」按钮，直接粘贴到「手动连接」面板即可；「房间」是分享用的房间标识，不是连接指纹。
- **受信证书**：其他客户端填 `域名:端口`，指纹留空。
- **自签**：其他客户端填 `IP:端口`，指纹填界面里的 SHA-256。
- 填了域名但 DNS 未指向本机时，服务仍会启动，只是本机自动接入会报错；这时用另一台机器或「手动连接」接入。
- 数据落在 `~/.lib-chat/rooms/room-public.db`、`room-public.files/` 与 `room-public.identity`，重启沿用同一个房间与证书身份。

### 界面连接公网

1. 点侧栏底部「手动连接」展开面板，地址填 `chat.example.com:4433`（本机自测填 `127.0.0.1:端口`）。
2. 自签服务端填详情里「证书 SHA-256」的 64 位指纹（不要在「房间」那一行复制）；受信 CA 留空。
3. 连上后地址写入 `~/.lib-chat/rooms/servers.json`，下次启动会在侧栏「远程服务器」自动出现（房间名与人数每 5 秒刷新）。
4. 私有 CA 通过启动前的 `LIBCHAT_CA_FILE` 指定根证书 PEM。

### 服务端参数

| 参数 | 默认值 | 说明 |
|---|---|---|
| `--listen` | `0.0.0.0:4433` | QUIC 监听地址 |
| `--name` | `公网聊天室` | 房间名 |
| `--cert` / `--key` | 无 | 证书链与私钥 PEM；不给则自签 |
| `--data-dir` | `~/.lib-chat/rooms` | 消息库、附件与自签身份目录 |
| `--announce` | 无 | 额外做局域网广播的端口（一般不需要） |

### 数据与身份

| 路径 | 内容 |
|---|---|
| `<data-dir>/room.db` | 公网服务端的房间标识与消息 |
| `<data-dir>/room.files/` | 附件内容，按 SHA-256 命名 |
| `<data-dir>/room.identity` | 自签证书与私钥，重启沿用 |
| `~/.lib-chat/rooms/room-52100.db` | 界面托管的局域网房间（按广播端口区分） |
| `~/.lib-chat/rooms/room-52100.files/` | 局域网房间附件 |
| `~/.lib-chat/rooms/room-public.db` | 界面托管的公网房间 |
| `~/.lib-chat/rooms/room-public.files/` | 公网房间附件 |
| `~/.lib-chat/rooms/room-public.identity` | 公网自签证书与私钥 |
| `~/.lib-chat/rooms/known-rooms.json` | 固定过的房间指纹 |
| `~/.lib-chat/rooms/servers.json` | 公网地址簿 |
| `~/.lib-chat/libchat-ui.log` | 界面诊断日志 |

## 界面快捷键与右键菜单

| 快捷键 | 行为 |
|---|---|
| `⌘K` / `Ctrl+K` | 打开或收起「手动连接」面板 |
| `⌘⇧P` / `Ctrl+Shift+P` | 打开或收起「公网服务」面板 |
| `⌘Enter` / `Ctrl+Enter` | 发送输入框内容 |
| `Esc` | 关闭右键菜单、连接面板、公网面板或首次信任提示 |

在消息上点右键可以：

- **复制**：把消息正文放进剪贴板
- **引用**：输入框上方出现引用条（发送者 + 摘要），随下一条消息一起发出；点引用条右侧的 `×` 取消
- **保存附件**：仅文件消息

引用是结构化数据：发送后显示在消息气泡顶部（左侧竖线 + `发送者：摘要`），并随消息一起落库、进历史。

## 技术实现

### 分层

```text
界面（chat-ui-gpui） / 命令行（chat-cli）
        │
        ▼
chat-ui-gpui::service   后端线程：连接会话、托管房间、局域网发现、公网地址簿
        │
        ├─ chat-core              协议 DTO、Connection / MessageStore 端口、Client 门面
        ├─ chat-server            单房间：成员、全局序号、扇出、SQLite/内存存储
        ├─ chat-transport-quic    QUIC 握手、请求/事件流、附件流、两种身份校验
        └─ chat-discovery         UDP 广播公告与回环应答
```

上下层只依赖 `chat-core` 的 trait：换传输或换存储不影响房间逻辑与界面。

### 协议

- QUIC + TLS 1.3，ALPN `libchat/1`。
- 每条连接一条双向流承载握手与请求/事件：客户端发 `Hello` 或 `Request`，服务端回 `Welcome` 或 `Event`，帧为长度前缀 + JSON。
- 附件上传/下载各开一条双向流，第一帧声明意图与附件元数据，之后是分块内容；消息本身只带元数据。
- 服务端为每条消息分配单调 `seq`，构成房间内全序；客户端按 `seq` 去重，断线后用 `Sync(after)` 补齐。
- 文本消息可携带 `quote` 摘要（发送者、正文、被引用消息的 `seq`）；引用随消息落库并出现在历史里，不递归展开引用链。
- 空闲与保活由传输层配置：默认 60 秒空闲超时、15 秒 PING。

### 两种身份校验

| 模式 | 校验方式 | 适用 |
|---|---|---|
| 指纹 | 比对服务端证书的 SHA-256，首次带外核对后按房间存入 `known-rooms.json` | 局域网自签 |
| 受信 CA | 校验证书链（内置公共根或 `LIBCHAT_CA_FILE`）与 SNI 主机名 | 公网域名 |

公网连接在服务层统一走 `ServerIdentity::PublicCa`；自签连接走 `ServerIdentity::Fingerprint`。重连沿用同一份身份参数。

### 局域网发现

- 服务端在 UDP 52100 端口广播公告（房间标识、名称、QUIC 端口、指纹、协议版本），同时向 `127.0.0.1:52100` 发一份供同机进程接收。
- 客户端收到公告后主动 QUIC 探测一次大厅，拿到在线人数再上侧栏。
- 同机多实例通过文件锁 `room-52100.lock` 保证只有一个服务端。

### 公网地址簿

- `Connect` 成功后把地址写入 `servers.json`（原子替换，避免半截文件）。
- 后端循环每 5 秒对地址簿里的服务器做一次「连接 → 读大厅 → 断开」，成功则推 `RemoteRoom` 事件；连续失败 3 次从侧栏移除，地址仍保留。
- 探测连接不加入房间，不影响在线人数。

### 界面内公网托管

- 与局域网托管共用同一套 `Server`、SQLite 与附件实现，只换监听方式：`0.0.0.0:端口`，不广播。
- 证书二选一：自签（首次生成后固定指纹）或用户提供的 PEM（交给 rustls 的受信根校验与主机名匹配）。
- 填了域名时用 `域名:端口` 自动接入；没填且是受信证书时不自动接入，避免 SNI 与证书名不符导致一条必然失败的重连。
- 公网房间用固定分区 `room-public.*`，与按广播端口分区的局域网房间互不干扰；证书参数有误时不会先建库，避免留下「有库无身份」的半成品。

### 断线重连

会话任务在连接断开后按 1s → 2s → 4s → 5s 退避重连，成功后用原身份重新 `Join`，再 `Sync` 补齐缺席期间的消息。界面只看到「重连中 / 已恢复」，不会被踢回大厅。

### 持久化

- 消息与房间标识存 SQLite（`sea-query` 构造语句，`sqlx` 执行）。
- 附件按内容哈希落盘，同一内容只存一份；先写临时文件再改名。
- 界面托管的房间按广播端口分文件；公网服务端按数据目录分文件。两者互不干扰。

### 并发

- 服务端用连接表 + 发布互斥保证广播顺序；每条连接一个容量 256 的事件队列。
- 事件入队失败（队列已满）即断开该连接，避免一个慢客户端拖住整个房间。
- 界面后端串行处理指令，网络操作都带超时，避免「点了没反应」。

## 测试

```bash
# 全量（串行，避免端口冲突）
cargo test --workspace --all-targets -- --test-threads=1

# 无头 UI 布局
cargo test -p chat-ui-gpui --features ui-test --test ui_layout

# 一条命令跑完格式、Clippy、全量测试、UI 布局与压测
./scripts/verify.sh
```

分层与「改哪里补什么」的约定见 [`docs/testing.md`](docs/testing.md)。公网相关用例：

| 文件 | 覆盖 |
|---|---|
| `crates/chat-transport-quic/tests/public_tls.rs` | 受信 CA 握手、不受信 CA 拒绝、主机名不匹配拒绝、域名多地址回退 |
| `crates/chat-cli/tests/public_serve.rs` | PEM 部署、自签指纹重启不变、重启后房间/历史/附件延续 |
| `crates/chat-ui-gpui/tests/public_e2e.rs` | 无指纹连接、地址簿探测、公网重连、退出人数回落、文件收发 |
| `crates/chat-ui-gpui/tests/public_hosting_e2e.rs` | 界面内公网托管：自签/受信证书、自动接入、错误反馈、重启延续 |

## 已知限制

- 公网没有中心目录服务，首次需要手动填一次地址；地址簿只做「连过就记住」。
- 公网仍为单机部署：SQLite + 本地附件目录，尚未拆分网关/逻辑、总线、对象存储与 Postgres。
- 证书主机名必须与连接地址一致；直接用 IP 连接需要证书带 IP SAN。
- 命令行 `host` 是内存存储的开发用房间；需要落盘或公网证书时用 `serve`。
- 端到端加密尚未加入，当前只有传输层加密。

更完整的设计取舍与路线图见 [`plan.md`](plan.md)。
