use super::*;

impl ChatApp {
    pub(crate) fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        let joined = self.room.as_ref().map(|room| room.joined).unwrap_or(false);

        let title = match &self.room {
            Some(room) => room.name.clone(),
            None => "未连接".to_string(),
        };
        let subtitle = match &self.room {
            Some(room) => format!("{}    在线 {} 人", room.addr, room.online),
            None => self
                .hosted
                .clone()
                .unwrap_or_else(|| "从左侧选择房间，或创建自己的房间".to_string()),
        };

        let action = if joined {
            button("退出房间", false, false, cx.listener(Self::on_leave))
        } else {
            button("加入房间", true, false, cx.listener(Self::on_join))
        };

        let lines: Vec<AnyElement> = self.lines.iter().map(render_line).collect();

        div()
            .flex_1()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_5()
                    .py_3()
                    .border_b_1()
                    .border_color(rgb(theme::BORDER))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(div().text_base().text_color(rgb(theme::TEXT)).child(title))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme::MUTED))
                                    .child(subtitle),
                            ),
                    )
                    .child(action),
            )
            .child(
                div()
                    .id("messages")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_5()
                    .py_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .children(lines),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_5()
                    .py_3()
                    .border_t_1()
                    .border_color(rgb(theme::BORDER))
                    .child(div().flex_1().child(self.message.clone()))
                    .child(button("发送", true, false, cx.listener(Self::on_send))),
            )
            .into_any_element()
    }
}
