//! 暗色主题：冷蓝中性色，靠 3~6% 的微明度差分层，不用阴影。
//!
//! 层级由深到浅：画布 → 侧栏 → 输入框/气泡 → 悬停态，边框只做发丝分隔。
//! 文字三级：正文、次要、最弱；强调色只用于可交互元素与自己发的消息。

/// 聊天区底色（最深，视觉焦点在消息上）。
pub const BG: u32 = 0x0b0f14;
/// 侧栏与顶栏等抬升面板。
pub const SURFACE: u32 = 0x111721;
/// 输入框、文件卡片、他人消息气泡。
pub const FIELD: u32 = 0x1a222e;
/// 悬停态与选中行。
pub const HOVER: u32 = 0x222c3a;
/// 发丝分隔线。
pub const BORDER: u32 = 0x1e2733;
/// 需要一点存在感的边框（输入框悬停）。
pub const BORDER_STRONG: u32 = 0x2f3b4d;
/// 正文。
pub const TEXT: u32 = 0xf5f7fa;
/// 次要信息（时间、人数、说明）。
pub const MUTED: u32 = 0xb4bdcb;
/// 最弱信息（小节标题、快捷键提示）。
pub const FAINT: u32 = 0x7a8496;
/// 强调色：自己发的消息、主按钮、可点击文字。
pub const ACCENT: u32 = 0x3b82f6;
/// 强调色悬停。
pub const ACCENT_HOVER: u32 = 0x5b9bff;
/// 强调色的低饱和底（标签、选中行）。
pub const ACCENT_SOFT: u32 = 0x1b2c4d;
/// 强调底上的文字。
pub const ON_ACCENT: u32 = 0xffffff;
/// 警示色。
pub const DANGER: u32 = 0xf87171;
/// 提醒色：正在重连等「需要知道但不算失败」的状态。
pub const WARN: u32 = 0xfbbf24;
