use super::*;

impl ChatApp {
    pub(crate) fn render_header(&self) -> AnyElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(44.))
            .px_5()
            .border_b_1()
            .border_color(rgb(theme::BORDER))
            .child(
                div()
                    .text_base()
                    .text_color(rgb(theme::TEXT))
                    .child("lib-chat"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(self.status.clone()),
            )
            .into_any_element()
    }
}
