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
    AnyElement, App, Context, Div, MouseButton, MouseDownEvent, MouseUpEvent, Window, div, list,
    prelude::*, px, rgb, rgba,
};

use crate::app::theme;

use chat_core::Attachment;

use super::{ChatApp, ChatLine, DiscoveredRoom, HostedRoom, MessageMenu, RoomSource, StatusKind};

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

/// 侧栏的小节标题：字号压到 11px，靠上留白分组。
fn section_label(text: &str) -> AnyElement {
    div()
        .px_4()
        .pt_3()
        .pb_2()
        .text_size(px(11.))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(rgb(theme::FAINT))
        .child(text.to_string())
        .into_any_element()
}

/// 侧栏内容的统一内边距。
fn padded(child: impl IntoElement) -> AnyElement {
    div().px_4().pb_2().child(child).into_any_element()
}

/// 侧栏提示文案。
fn hint(text: &str) -> AnyElement {
    div()
        .px_4()
        .py_1()
        .text_size(px(11.))
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
    let base = div()
        .h(px(32.))
        .flex()
        .flex_row()
        .justify_center()
        .items_center()
        .px_3()
        .rounded_md()
        .text_sm()
        .font_weight(gpui::FontWeight::MEDIUM)
        .cursor_pointer()
        .on_mouse_up(MouseButton::Left, on_click)
        .when(full_width, |this| this.w_full())
        .child(text.to_string());

    if primary {
        base.bg(rgb(theme::ACCENT))
            .text_color(rgb(theme::ON_ACCENT))
            .hover(|style| style.bg(rgb(theme::ACCENT_HOVER)))
            .into_any_element()
    } else {
        base.border_1()
            .border_color(rgb(theme::BORDER))
            .text_color(rgb(theme::MUTED))
            .hover(|style| {
                style
                    .bg(rgb(theme::HOVER))
                    .border_color(rgb(theme::BORDER_STRONG))
                    .text_color(rgb(theme::TEXT))
            })
            .into_any_element()
    }
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
        .pl_3()
        .pr_3()
        .py_3()
        .rounded_xl()
        .bg(rgb(theme::FIELD))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(
            div()
                .flex_none()
                .w(px(3.))
                .h(px(28.))
                .rounded_full()
                .bg(rgb(theme::ACCENT)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .truncate()
                        .text_sm()
                        .text_color(rgb(theme::TEXT))
                        .child(attachment.name.clone()),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(theme::MUTED))
                        .child(format!("文件 · {}", human_size(attachment.size))),
                ),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(12.))
                .font_weight(gpui::FontWeight::MEDIUM)
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
        .h(px(28.))
        .flex()
        .flex_row()
        .items_center()
        .px_2()
        .rounded_md()
        .text_size(px(12.))
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
            .px_6()
            .py_2()
            .flex()
            .flex_row()
            .justify_center()
            .text_size(px(11.))
            .text_color(rgb(theme::FAINT))
            .child(line.text.clone())
            .into_any_element();
    }

    let (label, label_color) = if line.mine {
        ("我", theme::FAINT)
    } else {
        (line.sender.as_str(), theme::FAINT)
    };

    let body = match &line.attachment {
        Some(attachment) => attachment_body(attachment, save),
        None => {
            let mut content = div().flex().flex_col().gap_1();
            if let Some(quote) = &line.quote {
                content = content.child(
                    div()
                        .test_id("message-quote")
                        .border_l_2()
                        .border_color(if line.mine {
                            rgba(0xffffff88)
                        } else {
                            rgb(theme::BORDER_STRONG)
                        })
                        .pl_2()
                        .text_size(px(12.))
                        .line_height(px(18.))
                        .text_color(if line.mine {
                            rgba(0xffffffcc)
                        } else {
                            rgb(theme::MUTED)
                        })
                        .child(format!("{}：{}", quote.sender, quote.text)),
                );
            }
            content = content.child(div().child(line.text.clone()));

            let bubble = div()
                .max_w(px(560.))
                .px_4()
                .py_3()
                .rounded_xl()
                .text_size(px(15.))
                .line_height(px(24.))
                .child(content);
            if line.mine {
                bubble
                    .bg(rgb(theme::ACCENT))
                    .text_color(rgb(theme::ON_ACCENT))
                    .into_any_element()
            } else {
                bubble
                    .bg(rgb(theme::FIELD))
                    .text_color(rgb(theme::TEXT))
                    .into_any_element()
            }
        }
    };

    div()
        .w_full()
        .px_6()
        .py_1()
        .flex()
        .flex_col()
        .when_else(
            line.mine,
            |this| this.items_end(),
            |this| this.items_start(),
        )
        .child(
            div()
                .px_1()
                .mb_1()
                .text_size(px(11.))
                .text_color(rgb(label_color))
                .child(label.to_string()),
        )
        .child(body)
        .into_any_element()
}

impl ChatApp {
    /// 消息右键菜单：复制、引用，文件消息多一项另存。
    pub(crate) fn render_message_menu(
        &self,
        menu: MessageMenu,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let line = menu.line.clone();
        let text = line.text.clone();
        let attachment = line.attachment.clone();
        let line_for_quote = line.clone();

        div()
            .id("message-menu-overlay")
            .absolute()
            .inset_0()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_close_message_menu))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_close_message_menu))
            .child(
                div()
                    .id("message-menu")
                    .test_id("message-menu")
                    .absolute()
                    .left(menu.position.x)
                    .top(menu.position.y)
                    .w(px(168.))
                    .flex()
                    .flex_col()
                    .py_1()
                    .rounded_lg()
                    .bg(rgb(theme::FIELD))
                    .border_1()
                    .border_color(rgb(theme::BORDER_STRONG))
                    .on_mouse_down(
                        MouseButton::Left,
                        |_: &MouseDownEvent, _: &mut Window, cx: &mut App| cx.stop_propagation(),
                    )
                    .child(menu_item(
                        "复制",
                        cx.listener(move |this, _, _, cx| {
                            this.copy_text(text.clone(), "消息", cx);
                            this.message_menu = None;
                            cx.notify();
                        }),
                    ))
                    .child(menu_item(
                        "引用",
                        cx.listener(move |this, _, window, cx| {
                            this.quote_message(line_for_quote.clone(), window, cx);
                        }),
                    ))
                    .when_some(attachment, |this, attachment| {
                        this.child(menu_item(
                            "保存附件",
                            cx.listener(move |this, _, _, cx| {
                                this.save_attachment(attachment.clone(), cx);
                                this.message_menu = None;
                                cx.notify();
                            }),
                        ))
                    }),
            )
            .into_any_element()
    }
}

/// 右键菜单里的一行。
fn menu_item(
    label: &str,
    on_click: impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .h(px(32.))
        .flex()
        .items_center()
        .px_3()
        .text_size(px(12.))
        .text_color(rgb(theme::TEXT))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::HOVER)))
        .on_mouse_up(MouseButton::Left, on_click)
        .child(label.to_string())
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
                    quote: None,
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
