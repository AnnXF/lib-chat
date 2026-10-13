//! lib-chat 桌面端。
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`app`] | 界面：状态、交互、渲染、控件 |
//! | [`service`] | 聊天服务：连接、托管、房间发现 |
//!
//! 二进制入口在 `bin/libchat-ui/`，进程级诊断归它自己。

pub mod app;
pub mod service;
