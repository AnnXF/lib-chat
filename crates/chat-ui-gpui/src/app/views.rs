//! 渲染件。
//!
//! 视觉上走极简路线：只用少量灰阶加一个强调色，靠留白和层次区分区域，
//! 分隔一律用 1px 发丝线，不用卡片和重底色。
//!
//! 三个区域各自成文件，共用的小件（小节标题、内边距、按钮、消息行）留在本模块。
//! 子模块统一 `use super::*`，所以这里导入的就是全区域可用的那套绘图件。

mod chat;
mod header;
mod sidebar;

use gpui::{
    AnyElement, App, Context, Div, MouseButton, MouseUpEvent, Window, div, list, prelude::*, px,
    rgb,
};

use crate::app::theme;

use chat_core::Attachment;

use super::{ChatApp, ChatLine, DiscoveredRoom, HostedRoom};

/// 给关键布局节点挂上固定标识，供无头 UI 测试查询边界。
///
/// 只在测试构建里真正记录，正式构建零成本。
pub(crate) trait TestIdExt: Sized {
    fn test_id(self, id: &'static str) -> Self;
}

#[cfg(any(test, feature = "ui-test"))]
impl TestIdExt for Div {
    fn test_id(self, id: &'static str) -> Self {
        self.debug_selector(move || id.to_string())
    }
}

#[cfg(not(any(test, feature = "ui-test")))]
impl TestIdExt for Div {
    fn test_id(self, _id: &'static str) -> Self {
        self
    }
}

#[cfg(any(test, feature = "ui-test"))]
impl TestIdExt for gpui::Stateful<Div> {
    fn test_id(self, id: &'static str) -> Self {
        self.debug_selector(move || id.to_string())
    }
}

#[cfg(not(any(test, feature = "ui-test")))]
impl TestIdExt for gpui::Stateful<Div> {
    fn test_id(self, _id: &'static str) -> Self {
        self
    }
}

/// 侧栏的小节标题。
fn section_label(text: &str) -> AnyElement {
    div()
        .px_5()
        .pt_3()
        .pb_1()
        .text_xs()
        .text_color(rgb(theme::FAINT))
        .child(text.to_string())
        .into_any_element()
}

/// 侧栏内容的统一内边距。
fn padded(child: impl IntoElement) -> AnyElement {
    div().px_5().pb_2().child(child).into_any_element()
}

/// 侧栏提示文案。
fn hint(text: &str) -> AnyElement {
    div()
        .px_5()
        .py_2()
        .text_xs()
        .text_color(rgb(theme::FAINT))
        .child(text.to_string())
        .into_any_element()
}

/// 一个按钮：`primary` 用强调色，否则是次要样式。
fn button(
    text: &str,
    primary: bool,
    full_width: bool,
    on_click: impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let (fg, bg, hover_bg) = if primary {
        (theme::ACCENT, theme::ACCENT_SOFT, 0x22345c)
    } else {
        (theme::MUTED, theme::SURFACE, theme::HOVER)
    };

    div()
        .flex()
        .flex_row()
        .justify_center()
        .items_center()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(rgb(bg))
        .text_sm()
        .text_color(rgb(fg))
        .cursor_pointer()
        .hover(move |style| style.bg(rgb(hover_bg)))
        .on_mouse_up(MouseButton::Left, on_click)
        .when(full_width, |this| this.w_full())
        .child(text.to_string())
        .into_any_element()
}

/// 文件消息的保存动作。
pub(crate) type SaveHandler = Box<dyn Fn(&MouseUpEvent, &mut Window, &mut App) + 'static>;

/// 字节数转成便于阅读的大小。
pub(crate) fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.;
    const MB: f64 = KB * 1024.;
    const GB: f64 = MB * 1024.;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes / KB)
    } else {
        format!("{bytes:.0} B")
    }
}

/// 文件消息的内容块：类型标签 + 文件名 + 大小，点一下另存。
fn attachment_body(attachment: &Attachment, save: Option<SaveHandler>) -> AnyElement {
    let card = div()
        .id("file-card")
        .test_id("file-card")
        .max_w(px(420.))
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(rgb(theme::FIELD))
        .child(
            div()
                .flex_none()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(rgb(theme::ACCENT_SOFT))
                .text_xs()
                .text_color(rgb(theme::ACCENT))
                .child("文件"),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .child(
                    div()
                        .truncate()
                        .text_sm()
                        .text_color(rgb(theme::TEXT))
                        .child(attachment.name.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::MUTED))
                        .child(human_size(attachment.size)),
                ),
        )
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(rgb(theme::ACCENT))
                .child("保存"),
        );

    match save {
        Some(save) => card
            .cursor_pointer()
            .hover(|style| style.bg(rgb(theme::HOVER)))
            .on_mouse_up(MouseButton::Left, save)
            .into_any_element(),
        None => card.into_any_element(),
    }
}

/// 输入区工具条上的功能按钮：无边框，悬停才出现底色。
fn tool_button(
    text: &str,
    on_click: impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .text_xs()
        .text_color(rgb(theme::MUTED))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::HOVER)).text_color(rgb(theme::TEXT)))
        .on_mouse_up(MouseButton::Left, on_click)
        .child(text.to_string())
        .into_any_element()
}

/// 一条消息。
///
/// 自己发的靠右并带强调底色，别人的靠左只留文字，视觉噪声更低。
fn render_line(line: &ChatLine, save: Option<SaveHandler>) -> AnyElement {
    if line.system {
        return div()
            .w_full()
            .px_5()
            .py_2()
            .flex()
            .flex_row()
            .justify_center()
            .text_xs()
            .text_color(rgb(theme::FAINT))
            .child(line.text.clone())
            .into_any_element();
    }

    let (label, label_color) = if line.mine {
        ("我", theme::FAINT)
    } else {
        (line.sender.as_str(), theme::MUTED)
    };

    let body = match &line.attachment {
        Some(attachment) => attachment_body(attachment, save),
        None => div()
            .max_w(px(520.))
            .text_sm()
            .text_color(rgb(theme::TEXT))
            .child(line.text.clone())
            .when(line.mine, |this| {
                this.px_3().py_2().rounded_md().bg(rgb(theme::ACCENT_SOFT))
            })
            .into_any_element(),
    };

    div()
        .w_full()
        .px_5()
        .py_2()
        .flex()
        .flex_col()
        .when_else(
            line.mine,
            |this| this.items_end(),
            |this| this.items_start(),
        )
        .child(
            div()
                .mb_1()
                .text_xs()
                .text_color(rgb(label_color))
                .child(label.to_string()),
        )
        .child(body)
        .into_any_element()
}

#[cfg(test)]
mod perf {
    use super::*;
    use std::time::Instant;

    /// 性能特征测量：构造单条消息元素的成本。
    ///
    /// 列表是虚拟化的，每帧只对可见的那些条调用这个函数，所以这个数乘上可见条数
    /// 就是每帧的渲染开销——与消息总量无关。
    ///
    /// 只打印不断言——它是用来复现结论的标尺，不是回归门槛（时间断言在 CI 上不稳定）。
    /// 用 `cargo test --release -- --nocapture line_element_cost` 查看。
    #[test]
    fn line_element_cost() {
        for count in [1_000usize, 10_000, 50_000] {
            let lines: Vec<ChatLine> = (0..count)
                .map(|i| ChatLine {
                    seq: Some(i as i64 + 1),
                    sender: "某人".into(),
                    text: format!("这是第 {i} 条消息，长度大约二十来个字"),
                    attachment: None,
                    mine: i % 2 == 0,
                    system: false,
                })
                .collect();

            let started = Instant::now();
            let elements: Vec<AnyElement> =
                lines.iter().map(|line| render_line(line, None)).collect();
            let elapsed = started.elapsed();
            println!(
                "{count:>7} 条 → 构造元素 {elapsed:>10.2?}（{:.2}µs/条）",
                elapsed.as_secs_f64() * 1e6 / count as f64
            );
            std::hint::black_box(elements);
        }
    }
}
