# lib-chat 技术方案（客户端-服务端架构）

> 目标：Rust + GPUI 聊天工具。**采用 C/S 架构，放弃 P2P**；局域网与公网必须同时支持，且为同一套协议、同一份服务端代码的两种部署形态。支持单聊/群聊（含万人群），核心是消息推拉与扇出方案；支持图片/视频/表情包等富消息。

---

## 0. 决策与结论速览

**核心决策：放弃 P2P，统一 C/S。**

理由：P2P 在离线可达、历史回放、新成员加入、消息全序、多端同步、治理（踢人/禁言/审计）上都要额外造轮子，而这些恰恰是聊天产品的刚需。一旦放弃 P2P：

- 局域网与公网**合并为同一架构**，只差部署方式与连接地址，无需维护两套逻辑。
- 消息顺序由服务端分配单调序号即可获得**全序**，彻底不再需要 DAG / HLC / 反熵。
- 离线消息、历史、多端同步全部由服务端天然解决。

| 议题 | 结论 |
|---|---|
| 架构 | 纯 C/S，服务端权威，客户端只做缓存与渲染 |
| 局域网 | 服务端本机/内网自托管，mDNS 自动发现，免配置、可完全离线 |
| 公网 | 同一服务端部署到云端，域名 + TLS，客户端填地址即可 |
| 万人群 | 服务端扇出 + **每客户端更新合并（coalescing）**，把推送频率与消息频率解耦 |
| 富媒体 | 服务端对象存储 + 客户端分块/断点续传 + 两级推送（缩略图/原图） |
| 加密 | 传输层 TLS 必需；端到端加密作为可选层（服务端只存密文） |

**代价**：必须有服务端进程（局域网场景由某个客户端以「托管模式」内置启动，用户无感知）。

---

## 1. 总体架构

### 1.1 分层与依赖方向（面向接口）

```
chat-proto     协议 DTO：客户端 <-> 服务端，与具体传输解耦（serde + 版本协商）
chat-core      领域模型 + 端口 trait（存储/推送/鉴权/媒体），无传输细节
   ▲
   ├── store-postgres   服务端存储（sea-query + sqlx）
   ├── store-sqlite     客户端本地缓存（sea-query + sqlx）
   ├── media-fs / media-s3
   └── bus-nats / bus-inproc   节点间消息总线（单机用 inproc）
   ▲
chat-server    服务端：连接网关、会话、扇出、同步、媒体
chat-client    客户端 SDK：连接管理、重连、本地缓存、事件流
   ▲
chat-ui-gpui   视图层，仅消费 chat-client 的 ViewModel + 事件流
   ▲
bin/server     服务端可执行（可单机内嵌，也可集群部署）
bin/client     客户端可执行
```

> 面向接口编程的价值从「P2P ↔ 中继切换」转变为「**单机内嵌 ↔ 集群部署**」与「**存储/总线/媒体后端可替换**」。

### 1.2 核心端口（trait 草图）

```rust
// 服务端：持久化
#[async_trait::async_trait]
pub trait MessageStore: Send + Sync {
    async fn append(&self, conv: &ConvId, msg: &NewMessage) -> Result<Seq>;   // 分配会话内单调 seq
    async fn since(&self, conv: &ConvId, cursor: Seq, limit: usize) -> Result<Vec<StoredMessage>>;
    async fn latest_seq(&self, conv: &ConvId) -> Result<Seq>;
}

// 服务端：把会话更新投递给某个在线订阅者（单机=直接写连接；集群=发总线）
#[async_trait::async_trait]
pub trait Fanout: Send + Sync {
    async fn publish(&self, conv: &ConvId, from: Seq, to: Seq) -> Result<()>;
    async fn subscribe(&self, conv: &ConvId, sink: Box<dyn UpdateSink>);
}

// 客户端：与服务端对话的唯一出口（LAN / WAN 只是 endpoint 不同）
#[async_trait::async_trait]
pub trait ServerApi: Send + Sync {
    async fn authenticate(&self, cred: Credential) -> Result<Session>;
    async fn sync(&self, cursors: Vec<ConvCursor>) -> Result<Vec<UpdateRange>>;
    async fn send(&self, conv: &ConvId, draft: Draft) -> Result<Seq>;
    async fn recv(&self) -> Result<ServerEvent>;   // 长连接事件流
}

// 客户端：媒体
#[async_trait::async_trait]
pub trait BlobClient: Send + Sync {
    async fn upload(&self, data: Bytes, mime: &str) -> Result<BlobId>;
    async fn download(&self, id: &BlobId, range: Option<Range>, resume: Option<Offset>) -> Result<Bytes>;
}
```

---

## 2. 部署形态（局域网 / 公网统一）

同一份 `bin/server`，两种部署：

| 维度 | 局域网模式 | 公网模式 |
|---|---|---|
| 服务端位置 | 内网某台机器 / 客户端内嵌「托管模式」 | 云主机 / 容器集群 |
| 发现方式 | **mDNS 广播** `_libchat._tcp.local`，客户端自动发现并连接 | 手动填域名 / 内置默认地址 |
| 传输 | QUIC 或 WSS（内网可信，可 TLS 自签 + TOFU） | WSS/HTTPS（受信 CA 证书） |
| 外网依赖 | **完全不需要** | 需要 |
| 存储 | 单机 SQLite/Postgres + 本地文件 | Postgres + 对象存储 |
| 总线 | 进程内 | NATS/Redis 等 |
| 媒体 | 本地磁盘，内网直传 | 对象存储 + CDN |

**统一要点**：
- 协议、消息模型、同步算法、客户端代码**完全一致**，差异封装在「发现 + 传输地址 + 存储/总线后端」三个适配器里。
- 客户端支持配置多个 endpoint 并自动择优/切换：在家连内网服务端，出门连公网服务端。
- 局域网「托管模式」：任一客户端可一键把服务端跑在本机，其余客户端 mDNS 自动接入，体验接近 P2P，但内核仍是 C/S。

---

## 2.5 V1 范围与交互流程（首个可用版本）

**目标**：局域网内单房间群聊，零账号、零配置、可离线。

**交互流程**：

```
打开 App
   ↓ 自动 mDNS 发现局域网房间（无任何配置）
显示「房间名 · 在线 N 人」        ← 未加入，看不到消息
   ↓ 手动点「加入」
进入群聊，开始收发消息             ← 不点一直不进
   ↓ 手动点「退出」
离开房间，人数 -1                  ← 不点退出就一直在群里
```

**状态机**：

```
未发现 ──发现──► 已发现(未加入) ──加入──► 已加入 ──退出──► 已发现(未加入)
```

**三个关键设计点**：

1. **Lobby 协议（人数从哪来）**：服务端对「未加入」的查询者只返回房间摘要，不含任何消息；加入后才订阅消息流。

   ```rust
   struct LobbyInfo {
       room_id: RoomId,
       name: String,
       online_count: u32,   // 当前在线人数（瞬态）
       member_count: u32,   // 总成员数（持久）
   }
   ```

2. **成员与在线必须分离**：成员持久（只在加入/退出时变），在线瞬态（断线即离线）。界面默认展示**在线人数**。

   | 概念 | 何时变 | 存储 |
   |---|---|---|
   | 成员 member | 仅在加入/退出 | 落库，持久 |
   | 在线 online | 连接建立/断开 | 内存，瞬态 |

3. **房主**：V1 采用「第一个启动者自动成为房主」的内嵌托管；房主退出即房间解散（V1 简单语义，不做迁移/选举）。

**V1 明确不做**：账号体系、多端同步、单聊、万人群、公网、端到端加密、富媒体。

---

## 3. 服务端设计

### 3.1 连接与网关

- 每客户端一条**长连接**（QUIC 多流 或 WebSocket），多会话复用同一条连接。
- 网关职责：鉴权、心跳、连接状态维护、把 `Fanout` 投来的更新写回连接。
- 集群形态：`gateway`（无状态，持有连接）+ `logic`（会话/存储）+ 总线，任一 gateway 可服务任一客户端，更新经总线广播到持有该用户连接的 gateway。

### 3.2 消息模型与顺序

```rust
struct NewMessage {
    client_msg_id: Uuid,   // 客户端生成，用于幂等去重
    sender: UserId,
    kind: MessageKind,     // Text / Image / Video / Sticker / System
    body: MessageBody,
    client_ts: Timestamp,
}

// 服务端落库后
struct StoredMessage {
    conv: ConvId,
    seq: i64,              // 会话内单调递增，由服务端分配 —— 全序
    msg_id: Uuid,
    sender: UserId,
    kind: MessageKind,
    body: MessageBody,     // 媒体只存 BlobId 引用
    server_ts: Timestamp,
}
```

- **顺序 = `seq`**。不需要因果时钟、不需要 DAG。这是放弃 P2P 后最大的简化收益。
- **幂等**：`(conv, sender, client_msg_id)` 唯一约束，重发不重复。
- 已读/正在输入走独立瞬态通道，不落库。

### 3.3 推拉方案与扇出（核心）

服务端对每个在线客户端维护**每会话的待推送区间**，可合并、可降级：

1. **落库**：消息入库并拿到 `seq`。
2. **扇出**：`Fanout::publish(conv, from_seq, to_seq)`。
3. **每客户端合并**：网关为每个客户端维护 `pending: Map<ConvId, (min_seq, max_seq)>`，后台每 **100ms 或达到阈值**才 flush 一次。
4. **发送**：flush 时发「区间摘要」，客户端据 `seq` 增量拉取；或对活跃会话直接内联消息体（见下）。

关键点：**第 3 步把「推送频率」与「消息频率」解耦**。万人群即使 167 msg/s，对单个客户端也最多每 100ms 一次更新通知（≤10 次/s），扇出成本与消息速率无关。

两种投递策略，服务端按会话规模/客户端状态动态选择：

| 策略 | 适用 | 行为 |
|---|---|---|
| **内联推送** | 单聊、≤ 数百小群 | flush 时直接携带消息体，客户端零额外请求，延迟最低 |
| **提示 + 拉取** | 大群、慢客户端、移动网络 | flush 只带 `(conv, from_seq, to_seq)`，客户端调 `sync` 拉取内容 |

**万人群实测目标**：单条消息对网关产生 O(在线数) 次写，但单客户端推送频率有上界（10 Hz）；服务端带宽 ≈ `推送频率 × 客户端数 × 摘要大小`，与消息速率解耦。

### 3.4 离线与多端同步

- 客户端维护每会话 `cursor = 已确认最大 seq`。
- 上线/重连：`sync(cursors)` → 服务端按会话返回 `UpdateRange`（可能跨多段，或直接给 `from..=latest`）。
- 客户端可继续分页拉取，避免一次拉取巨大区间。
- 多端：同一 user 的多个 device 各自持有 cursor，服务端分别扇出（含自己发的消息回显到其他端）。

### 3.5 媒体（图片 / 视频 / 表情包）

- **两级推送**：消息体只带元数据（BlobId、尺寸、时长）+ **缩略图 BlobId**；缩略图随消息内联推，原图/视频按需拉。
- **上传**：客户端分块上传，服务端内容寻址（`BlobId = BLAKE3`）去重；支持断点续传。
- **下载**：Range 请求，视频边下边播；局域网下直接走内网服务端，速度接近本地。
- **表情包**：Unicode emoji 走字体；贴纸包为一组 blob 打包按需拉取。
- 存储后端抽象为 `media-fs`（局域网）/ `media-s3`（公网）。

### 3.6 存储

- 服务端 Postgres，客户端 SQLite，统一用 `sea-query` + `sqlx`，SQL 一律通过 `Query::select()` 构造，禁止硬编码字符串。
- 服务端表：
  - `users(user_id, display_name, created_at)`
  - `devices(device_id, user_id, public_key, last_seen)`
  - `conversations(conv_id, kind, title, created_at)`
  - `members(conv_id, user_id, role, joined_seq)`
  - `messages(conv_id, seq, msg_id, sender_id, kind, body, blob_id, server_ts)` — PK `(conv_id, seq)`
  - `blobs(blob_id, size, mime, storage_key, state)`
  - `receipts(conv_id, user_id, last_read_seq)`
- 索引：`messages(conv_id, seq)` 支撑增量拉取；`messages(conv_id, sender_id, client_msg_id)` 唯一幂等。
- 客户端缓存同构裁剪版，只保留本地需要的会话与区间。

---

## 4. 客户端设计

- **连接管理**：多 endpoint（内网/公网）择优与故障切换；断线指数退避重连；重连后自动 `sync` 补齐。
- **本地缓存**：SQLite 存消息与媒体索引，离线可读历史。
- **事件流**：`chat-client` 向上暴露 `Stream<ClientEvent>`（新消息/更新摘要/连接状态），UI 只订阅它。
- **发送**：乐观插入（`client_msg_id` 占位）→ 服务端返回 `seq` 后替换/重排 → 幂等保证不重复。
- **UI**：GPUI `List`/`UniformList` 虚拟化渲染消息与成员列表；分页向上加载历史。

> GPUI 风险与缓解不变：API 未稳定，需锁 revision；UI 与逻辑严格隔离。

---

## 5. 加密与信任

- **传输层**：局域网自签证书 + TOFU/指纹校验（或共享 PSK）；公网受信 CA 证书。必需。
- **端到端加密（可选层）**：
  - 开启后服务端只存密文，转发不解析；1:1 用双棘轮，群用 Sender Keys（中小群）/ MLS（万人群）。
  - 代价：服务端搜索、审核、服务端历史回放失效。
  - 建议：默认 TLS 可信，E2EE 作为「私密会话」选项逐步引入。

---

## 6. 横向扩容路径

- 单机（局域网/小规模）→ 内嵌 bus，单进程即可。
- 集群（公网）：
  - `gateway` 无状态水平扩容，连接按 user 一致性路由；
  - `logic` 按会话分片；
  - 总线（NATS）解耦 gateway 与 logic；
  - 存储 Postgres 分库分表 / 消息按会话归档；
  - 媒体对象存储 + CDN。
- **扩容粒度天然对齐「会话」**，因为全序只需会话内保证。

---

## 7. 里程碑

- **M0 骨架**：workspace 分层、`chat-proto`/`chat-core` trait、单机 inproc 服务端、GPUI 空窗口 + 虚拟列表。
- **M1 局域网大厅**：mDNS 广播/发现 + `LobbyInfo` 摘要（房间名/在线人数）+ 房主内嵌托管。
- **M2 加入/退出 + 文本群聊**：join/leave 状态机、成员与在线分离、消息落库与扇出、群文本消息。
- **M3 单聊与多会话**：会话列表、单聊、版本号增量同步。
- **M4 富媒体**：Blob 上传下载 + 缩略图两级推送 + 图片/表情。
- **M5 万人群**：网关合并推送 + 提示/拉取策略 + 压测 1w 在线连接。
- **M6 公网与集群**：网关/逻辑分离 + 总线 + 对象存储 + 域名 TLS。

---

## 8. 风险

| 风险 | 影响 | 缓解 / 验证 |
|---|---|---|
| GPUI API 不稳定 | 返工 | 逻辑与 UI 隔离；锁 revision |
| 万人群扇出成本 | 带宽/CPU | 每客户端合并推送，压测验证推送频率上界 |
| 长连接数上限 | 单 gateway 承载 | 压测单机连接数，规划 gateway 扩容 |
| 离线消息积压 | 存储膨胀 | cursor 分页拉取 + 归档策略 |
| 局域网自签证书 | 信任体验 | TOFU + 指纹展示 + 可选 PSK |
| E2EE 与服务端功能冲突 | 搜索/审核 | E2EE 作为可选会话类型 |

**测试**：所有端口提供内存假实现；服务端用模拟客户端做万人群压力测试；同步协议走 property-based testing 验证「任意断连重连后最终一致」。

---

## 9. V1 已定决策

| 决策点 | 结论 | 理由 |
|---|---|---|
| 人数显示 | 未加入：只显示「在线 N 人」；加入后：显示「在线 N / 共 M」 | 隐私友好，且区分瞬态/持久 |
| 未加入的可见信息 | 仅房间名 + 人数，**不暴露昵称** | 局域网隐私 |
| 房间数量 | V1 单房间；协议保留 `room_id`，多房间无需改协议 | 先简化，预留扩展 |
| 消息类型 | V1 纯文本 + Unicode emoji，不做图片/贴纸/视频 | 聚焦核心链路 |
| 服务端存储 | SQLite 单机，`sea-query` + `sqlx`；后期可迁 Postgres | V1 单机足够 |
| 传输 | QUIC（`quinn`）+ rustls 自签证书 + TOFU 指纹校验 | 自带 TLS 1.3、多路复用，免单独配 TLS |
| 端到端加密 | **不进 V1**，仅传输层加密 | 避免与搜索/审核/回放冲突 |
| 房主仲裁 | 同时启动时，`DeviceId` 最小者成为房主；mDNS 冲突检测后让位 | 避免双房主 |

---

## 10. 后续演进（V1 之后）

1. 多房间与房间列表选择。
2. 图片 / 表情包（BlobStore + 两级推送）。
3. 单聊与端到端加密。
4. 离线消息与多端同步。
5. 万人群：网关合并推送 + 提示/拉取。
6. 公网部署与集群化。

---

## 11. 实现进度

| 阶段 | 状态 | 说明 |
|---|---|---|
| M0 骨架 | 完成 | workspace 分层、协议、端口 trait、进程内服务端、端到端测试 |
| M1 局域网大厅 | 完成 | mDNS 广播/发现、QUIC 传输 + TOFU 指纹校验 |
| M2 加入/退出 + 文本群聊 | 完成 | 成员/在线分离、消息扇出、历史分页 |
| **桌面界面** | **完成** | GPUI 单窗口：房间列表、人数、加入/退出、聊天、创建房间 |
| M3 单聊与多会话 | 未开始 | |
| M4 富媒体 | 未开始 | |
| M5 万人群 | 未开始 | |
| M6 公网与集群 | 未开始 | |

### 已有 crate

```
crates/
├── chat-proto           协议 DTO（RoomInfo / LobbyInfo / ClientRequest / ServerEvent）
├── chat-core            端口：Connection / RequestSink / ConnectionAcceptor / MessageStore
├── chat-server          房间状态、成员与在线、扇出、内存存储
├── chat-client          带类型的客户端 SDK
├── chat-discovery       房间发现：mDNS + UDP 广播双通道
├── chat-transport-quic  QUIC 传输 + 自签证书 + TOFU
├── chat-ui-gpui         GPUI 桌面界面（lib: backend + text_input；bin: libchat-ui）
└── chat-cli             libchat host / join
```

### 用法

```bash
# 桌面界面
cargo run -p chat-ui-gpui            # 或 ./target/release/libchat-ui

# 命令行
cargo run -p chat-cli -- host --name 局域网聊天室
cargo run -p chat-cli -- join                                  # 依赖 mDNS 发现
cargo run -p chat-cli -- join --addr 10.0.0.5:45678 --fingerprint <hex>
```

**界面操作**：点「创建房间」即成为房主，界面会自动接入。其他实例（**同一台机器也可以**）打开后会在左侧「局域网房间」里自动看到该房间与在线人数，点一下即连接，再点「加入房间」进入聊天。若发现被网络环境阻断，仍可用「手动连接」填 `127.0.0.1:端口` 或局域网地址兜底。

**发现机制**：mDNS（跨机标准做法）与 UDP 广播（`52100/udp` 探测应答，覆盖同机多实例与 mDNS 失效的网络）双通道并用，去重合并后展示。

### 身份与计数规则

无账号体系下，成员与在线必须定义清楚，否则同机多窗口、重连、换房间都会把同一个人算成多个：

| 概念 | 规则 |
|---|---|
| **连接** | 按 `ConnId` 记账，同一设备允许多条连接（多窗口、探测连接）互不覆盖 |
| **成员** | 加入时**同名且当前离线**的既有成员直接复用身份，否则新建 |
| **在线** | 当前仍有连接且已加入的**去重身份数**，断开即离线 |
| **退出** | 只有显式「退出房间」才删除成员；断开连接只掉在线 |

因此：重连、换房间再回来、点自己的房间，成员数都不会虚增。

### 界面约定

- **房间列表按房间标识去重**：同一个房间会经 mDNS 与 UDP 广播重复播报，且地址可能不止一个（局域网地址 + 回环地址）。客户端按 `RoomId` 归并为一条，并优先使用非回环地址连接。
- **消息区分归属**：服务端下发的消息带发送者 `UserId`，界面用加入时返回的自身 `UserId` 比对，**自己发的靠右蓝底、别人的靠左灰底**，避免同机多窗口时张冠李戴。
- **事件按房间过滤**：所有事件都带 `RoomId`，与当前房间不符的一律丢弃，切换房间时的在途事件不会串台。

### 已知限制

- ~~同主机 mDNS 互相看不见~~：已由 UDP 广播通道解决（`_libchat/_tcp` 之外在 `52100/udp` 上做探测应答）。同机两个实例现可自动发现，跨机由 mDNS + 广播双保险。
- **未落盘**：服务端消息与成员仅存内存，进程退出即丢失。SQLite 持久化待后续。
- **单会话**：界面同一时刻只连接一个房间。切换房间会关闭旧连接；若该连接已加入，成员身份保留（对应「不点退出就一直在群里」）。自己创建的房间在切换后仍继续广播与监听，直到再次「创建房间」或关闭应用。
- **房主仲裁未实现**：`DeviceId` 最小者优先的规则尚未落地，当前由用户手动创建房间。
