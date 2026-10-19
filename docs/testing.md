# 测试规范

每次改动必须在本地跑通下方「提交前检查」。新增业务能力时，按「分层清单」补齐对应层的测试，不允许只加实现不加测试。

## 提交前检查

```bash
./scripts/verify.sh
```

等价于依次执行：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets -- --test-threads=1
cargo test -p chat-ui-gpui --features ui-test --test ui_layout
cargo test -p chat-server --test load -- --nocapture
```

- `--test-threads=1`：多套端到端用例会占用固定广播端口与数据目录，串行执行才能互不干扰。
- `--features ui-test`：启用 gpui 无头测试平台与元素边界查询；不带该特性时 `#[cfg(feature = "ui-test")]` 的用例整体跳过。
- 重量级压测不进默认流程：`cargo test -p chat-server --test load -- --ignored --nocapture`。

## 分层清单

| 层 | 位置 | 覆盖什么 | 什么时候必须加 |
|---|---|---|---|
| 单元 | `src/**` 内 `#[cfg(test)]` | 纯函数、状态转移、协议解析、路径与指纹处理 | 新增分支逻辑、边界条件、格式转换 |
| 协议/端口 | `crates/chat-core` | DTO 编解码、客户端门面、端口契约 | 协议字段或语义变化 |
| 后端集成 | `crates/chat-server/tests/e2e.rs` | 加入/退出、扇出、历史、同步、慢客户端、幂等 | 任何服务端行为变化 |
| 传输集成 | `crates/chat-transport-quic/tests/` | 真实 QUIC 握手、证书指纹、保活、断线 | 传输参数、超时、TLS 相关改动 |
| 后端压测 | `crates/chat-server/tests/load.rs` | 并发顺序、去重、无丢失、时延与吞吐 | 改动锁、队列、存储或广播路径 |
| 附件传输 | `crates/chat-server/tests/e2e.rs`、`crates/chat-transport-quic/tests/quic_e2e.rs`、`crates/chat-ui-gpui/tests/service_e2e.rs` | 成员校验、摘要/大小校验、内容去重、上传下载往返、另存到本地 | 任何附件、文件消息或存储布局改动 |
| 服务集成 | `crates/chat-ui-gpui/tests/service_e2e.rs` | 连接、重连、托管、信任、切房、历史补齐 | 改动 `service/` 任一模块 |
| UI 布局 | `crates/chat-ui-gpui/tests/ui_layout.rs` | 默认窗口与小窗口下各区域完整可见 | 改动 `app/`、`views/`、样式或文案长度 |

## 新增业务扩展时

1. **先定协议与端口**：新字段进 `chat-core` 的 DTO 或 `ClientRequest`/`ServerEvent`，并补编解码与 `Client` 门面单测。
2. **后端行为**：在 `chat-server/tests/e2e.rs` 增加一条端到端用例，名字写清「什么条件下必须发生什么」，例如 `duplicate_message_replies_to_sender_without_broadcast_or_new_seq`。
3. **资源与并发**：若新增队列、锁或后台任务，同时在 `load.rs` 补一条并发用例，断言顺序、去重与不丢失；时延阈值放宽到不会被 CI 抖动误判。
4. **传输相关**：改握手、超时、证书时补 `chat-transport-quic/tests/`，用真实 QUIC 而不是 mock。
   - 附件/文件消息改动额外覆盖：非成员被拒、摘要或大小不符被拒、同一内容不重复占盘、跨连接下载内容一致。
5. **服务层**：改 `service/`（连接、重连、托管、信任）必须在 `service_e2e.rs` 覆盖成功与失败两条路径；失败路径要断言「旧会话仍可用」。
6. **界面**：改 `app/` 或 `views/` 时，为关键布局节点保留或新增 `test_id`，并在 `ui_layout.rs` 断言其边界仍在窗口内。
7. **文档**：能力进入 `plan.md` 的进度表，行为边界（例如「进程重启后待发送不恢复」）一并写明。

## 编写约定

### 命名

- 用例名描述可观察结果，不描述实现：`reconnects_after_connection_drops_without_leaving_room` 优于 `test_reconnect`。
- 一条用例只验证一个结论；需要多个结论时拆开。

### 隔离

- 数据目录一律用临时目录，命名带用途与随机后缀，结束前关闭存储并删除文件。
- 端口按测试串行分配，不写死真实端口；需要并行时用不同 `beacon_port` 与数据目录。
- 不依赖真实网络、真实时间与真实用户目录。超时用压短的参数（如把空闲超时压到 1 秒）而不是缩短断言。

### 稳定性

- 不做精确计时断言；需要时给足余量，并打印实际数值便于排查。
- 不用 `sleep` 代替同步；等待事件用「条件 + 超时」。
- 新增用例连续跑 3 次应稳定通过，否则视为不稳定用例，先修再用。

### UI

- 只在测试构建注册选择器，避免正式包开销：`#[cfg(any(test, feature = "ui-test"))]`。
- 选择器名与用途一致（`sidebar`、`sidebar-footer`、`chat-main`、`composer`），新增区域沿用同一套命名。
- 布局断言检查：是否在窗口内、宽度是否符合设计、关键区域高度是否大于 0、底部区域是否贴底。
- 长文本（房间名、地址、指纹、昵称）是必测场景：它们是侧栏/标题栏溢出的主要来源。

### 压测

- 默认档控制在数秒内，重量档用 `#[ignore]`。
- 断言正确性（序号连续、标识唯一、历史与广播一致），吞吐与时延只打印并设宽松上限。
- 至少覆盖内存存储与 SQLite 存储各一条并发用例。

## 失败处理

- 先判断是实现问题还是用例问题；不许为了变绿放宽断言。
- 用例暴露真实缺陷时，修复实现并保留用例作为回归。
- 跨进程/跨设备的手工验证（真实局域网、窗口交互）不替代自动化测试，但也不能被自动化测试冒充；未验证项要在提交说明里写明。
