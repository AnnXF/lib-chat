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

use gpui::{AnyElement, App, Context, MouseButton, MouseUpEvent, Window, div, prelude::*, px, rgb};

use crate::app::theme;

use super::{ChatApp, ChatLine, DiscoveredRoom};

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

/// 一条消息。
///
/// 自己发的靠右并带强调底色，别人的靠左只留文字，视觉噪声更低。
fn render_line(line: &ChatLine) -> AnyElement {
    if line.system {
        return div()
            .w_full()
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

    let body = div()
        .max_w(px(520.))
        .text_sm()
        .text_color(rgb(theme::TEXT))
        .child(line.text.clone())
        .when(line.mine, |this| {
            this.px_3().py_2().rounded_md().bg(rgb(theme::ACCENT_SOFT))
        });

    div()
        .w_full()
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
