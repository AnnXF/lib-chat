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
            let (line, attachment) = {
                let app = this.read(cx);
                (app.lines[ix].clone(), app.lines[ix].attachment.clone())
            };
            let save = attachment.map(|attachment| {
                let entity = this.clone();
                Box::new(move |_: &MouseUpEvent, _: &mut Window, cx: &mut App| {
                    entity.update(cx, |app, cx| {
                        app.save_attachment(attachment.clone(), cx);
                    });
                }) as SaveHandler
            });
            render_line(&line, save)
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
                    .flex_col()
                    .gap_2()
                    .px_5()
                    .pt_3()
                    .pb_4()
                    .border_t_1()
                    .border_color(rgb(theme::BORDER))
                    // 工具条在上：左侧功能入口，右侧快捷键与发送；
                    // 新增表情/图片只是在左侧再加一个入口，不挤占输入区
                    .child(
                        div()
                            .id("composer-tools")
                            .test_id("composer-tools")
                            .w_full()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .pb_2()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .child(tool_button("表情", cx.listener(Self::on_open_emojis)))
                                    .child(tool_button("文件", cx.listener(Self::on_pick_file))),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_3()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(theme::FAINT))
                                            .child("Enter 发送"),
                                    )
                                    .child(button("发送", true, false, cx.listener(Self::on_send))),
                            ),
                    )
                    // 输入区在下：大块留白，后续多行输入、@ 提及都在这里长高
                    .child(
                        div()
                            .id("composer-input")
                            .test_id("composer-input")
                            .w_full()
                            .flex_1()
                            .min_h(px(96.))
                            .flex()
                            .items_start()
                            .px_3()
                            .py_2()
                            .rounded_lg()
                            .bg(rgb(theme::FIELD))
                            .border_1()
                            .border_color(rgb(theme::BORDER))
                            // 点输入区任意位置都能聚焦，不必正好点到文字行
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_focus_composer))
                            .child(self.message.clone()),
                    ),
            )
            .into_any_element()
    }
}
