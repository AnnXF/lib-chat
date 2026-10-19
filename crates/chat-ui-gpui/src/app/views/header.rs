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
            .h(px(44.))
            .px_5()
            .border_b_1()
            .border_color(rgb(theme::BORDER))
            .child(
                div()
                    .flex_none()
                    .text_base()
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
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(self.status.clone()),
            )
            .into_any_element()
    }
}
