use super::*;

impl ChatApp {
    pub(crate) fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        let title = match &self.room {
            Some(room) => room.name.clone(),
            None => "未连接".to_string(),
        };
        let subtitle = match &self.room {
            Some(room) => format!("{}    在线 {} 人", room.addr, room.online),
            None => self.hosted.as_ref().map_or_else(
                || "从左侧选择房间，或创建自己的房间".to_string(),
                |hosted| format!("本机已创建「{}」，加入后可聊天", hosted.name),
            ),
        };

        // 没连上房间时不摆「加入」：那会儿点下去什么也不会发生，只会让人以为界面卡住了
        let action = match self.room.as_ref().map(|room| room.joined) {
            None => div().into_any_element(),
            Some(true) => button("退出房间", false, false, cx.listener(Self::on_leave)),
            Some(false) => button("加入房间", true, false, cx.listener(Self::on_join)),
        };

        // 只渲染可见范围那几条，消息再多每帧成本也不变
        let this = cx.entity();
        let messages = list(self.list.clone(), move |ix, _window, cx| {
            let lines = &this.read(cx).lines;
            render_line(&lines[ix])
        })
        .flex_1();

        div()
            .id("chat-main")
            .test_id("chat-main")
            .flex_1()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .child(
                div()
                    .id("chat-header")
                    .test_id("chat-header")
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_5()
                    .py_3()
                    .border_b_1()
                    .border_color(rgb(theme::BORDER))
                    .child(
                        // 房间标识和地址都是长文本，必须允许缩到 0 再省略，否则会把窗口撑宽
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .text_base()
                                    .text_color(rgb(theme::TEXT))
                                    .child(title),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_xs()
                                    .text_color(rgb(theme::MUTED))
                                    .child(subtitle),
                            ),
                    )
                    .child(action),
            )
            .child(messages)
            .child(
                div()
                    .id("composer")
                    .test_id("composer")
                    .flex_none()
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
