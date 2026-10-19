use super::*;

impl ChatApp {
    pub(crate) fn render_header(&self) -> AnyElement {
        div()
            .id("app-header")
            .test_id("app-header")
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_4()
            .h(px(48.))
            .px_5()
            .bg(rgb(theme::SURFACE))
            .border_b_1()
            .border_color(rgb(theme::BORDER))
            .child(
                div()
                    .flex_none()
                    .text_size(px(15.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(theme::TEXT))
                    .child("lib-chat"),
            )
            .child(
                // 状态栏会出现很长的错误信息，必须允许缩到 0 再省略，不能把窗口撑宽
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_right()
                    .text_size(px(12.))
                    // 出错时用警示色，正常状态保持低对比
                    .text_color(rgb(if self.status.starts_with('⚠') {
                        theme::DANGER
                    } else {
                        theme::MUTED
                    }))
                    .child(self.status.clone()),
            )
            .into_any_element()
    }
}
