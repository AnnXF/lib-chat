use super::*;
use chat_core::{MemberRole, RoomKind};

impl ChatApp {
    pub(crate) fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        let title = match &self.room {
            Some(room) => room.name.clone(),
            None => "未连接".to_string(),
        };
        let is_group = self.room.as_ref().is_some_and(|room| {
            self.conversations
                .iter()
                .any(|known| known.room == room.id && known.kind == RoomKind::Group)
        });
        let subtitle = match &self.room {
            Some(room) => format!("{}    在线 {} 人", room.addr, room.online),
            None => self.hosted.as_ref().map_or_else(
                || "选择左侧房间，或新建一个".to_string(),
                |hosted| format!("「{}」已启动，用「连接」加入", hosted.name),
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
            let element = render_line(&line, save);

            // 右键打开消息菜单；坐标是窗口坐标，菜单在主区上层绝对定位
            let menu_line = line.clone();
            let entity = this.clone();
            div()
                .w_full()
                .on_mouse_up(
                    MouseButton::Right,
                    move |event: &MouseUpEvent, window: &mut Window, cx: &mut App| {
                        let viewport = window.viewport_size();
                        let position = event.position;
                        let line = menu_line.clone();
                        entity.update(cx, |app, cx| {
                            app.open_message_menu(position, viewport, line, cx);
                        });
                    },
                )
                .child(element)
                .into_any_element()
        })
        .flex_1();

        // 未进入房间时给一个安静的引导，而不是一大片空白
        let body = if self.room.is_none() {
            let hint = if self.hosted.is_some() {
                "服务已启动，点底部「手动连接」加入"
            } else {
                "在左侧选择房间，或点底部「新建房间」"
            };
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .text_size(px(15.))
                        .text_color(rgb(theme::MUTED))
                        .child("还没有进入房间"),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(theme::FAINT))
                        .child(hint),
                )
                .into_any_element()
        } else {
            messages.into_any_element()
        };

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
                    .h(px(64.))
                    .px_6()
                    .bg(rgb(theme::SURFACE))
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
                                    .text_size(px(15.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(rgb(theme::TEXT))
                                    .child(title),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.))
                                    .text_color(rgb(theme::MUTED))
                                    .child(subtitle),
                            ),
                    )
                    .when(is_group, |this| {
                        this.child(button(
                            "成员",
                            false,
                            false,
                            cx.listener(Self::on_show_members),
                        ))
                    })
                    .child(action),
            )
            .when(is_group && self.show_members, |this| {
                this.child(self.render_members_panel(cx))
            })
            .child(body)
            .child(
                div()
                    .id("composer")
                    .test_id("composer")
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .px_6()
                    .pt_3()
                    .pb_3()
                    .bg(rgb(theme::SURFACE))
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
                                            .text_size(px(11.))
                                            .text_color(rgb(theme::FAINT))
                                            .child("Enter 发送"),
                                    )
                                    .child(button("发送", true, false, cx.listener(Self::on_send))),
                            ),
                    )
                    // 引用条在输入框上方：发送后自动消失，点 × 可取消
                    .when_some(self.pending_quote.clone(), |this, quote| {
                        this.child(quote_bar(quote, cx))
                    })
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
                            .rounded_xl()
                            .bg(rgb(theme::FIELD))
                            .border_1()
                            .border_color(rgb(theme::BORDER))
                            .hover(|style| style.border_color(rgb(theme::BORDER_STRONG)))
                            // 点输入区任意位置都能聚焦，不必正好点到文字行
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_focus_composer))
                            .child(self.message.clone()),
                    ),
            )
            .into_any_element()
    }
}

/// 输入框上方的引用摘要：左侧强调竖线 + 发送者与正文 + 右侧清除。
fn quote_bar(quote: chat_core::Quote, cx: &mut Context<ChatApp>) -> AnyElement {
    div()
        .id("quote-bar")
        .test_id("quote-bar")
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .rounded_lg()
        .bg(rgb(theme::FIELD))
        .border_l_2()
        .border_color(rgb(theme::ACCENT))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_size(px(12.))
                .text_color(rgb(theme::MUTED))
                .child(format!("{}：{}", quote.sender, quote.text)),
        )
        .child(
            div()
                .id("quote-clear")
                .test_id("quote-clear")
                .flex_none()
                .w(px(20.))
                .h(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .text_size(px(12.))
                .text_color(rgb(theme::FAINT))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme::HOVER)).text_color(rgb(theme::TEXT)))
                .on_mouse_down(MouseButton::Left, cx.listener(ChatApp::on_clear_quote_down))
                .child("×"),
        )
        .into_any_element()
}

impl ChatApp {
    /// 群成员面板：成员、角色、移出/退群/解散。
    pub(crate) fn render_members_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let me = self.account.as_ref().map(|user| user.id);
        let is_owner = me.is_some_and(|id| {
            self.members
                .iter()
                .any(|member| member.user.id == id && member.role == MemberRole::Owner)
        });
        let rows: Vec<_> = self
            .members
            .iter()
            .enumerate()
            .map(|(index, member)| {
                let target = member.user.id;
                let name = member.user.display_name.clone();
                let first = name.chars().next().unwrap_or('?').to_string();
                let role = if member.role == MemberRole::Owner {
                    "群主"
                } else {
                    "成员"
                };
                let can_remove = is_owner && Some(target) != me;
                div()
                    .id(("member", index))
                    .h(px(36.))
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .size(px(24.))
                            .flex_none()
                            .rounded_full()
                            .bg(rgb(theme::ACCENT))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(11.))
                            .text_color(rgb(theme::ON_ACCENT))
                            .child(first),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_sm()
                            .child(name),
                    )
                    .child(div().text_xs().text_color(rgb(theme::MUTED)).child(role))
                    .when(can_remove, |this| {
                        this.child(button(
                            "移出",
                            false,
                            false,
                            cx.listener(move |this, _, _, cx| this.on_remove_member(target, cx)),
                        ))
                    })
                    .into_any_element()
            })
            .collect();

        div()
            .id("members-panel")
            .test_id("members-panel")
            .flex_none()
            .max_h(px(190.))
            .overflow_y_scroll()
            .bg(rgb(theme::SURFACE))
            .border_b_1()
            .border_color(rgb(theme::BORDER))
            .flex()
            .flex_col()
            .py_1()
            .children(rows)
            .when(is_owner, |this| {
                this.child(div().px_4().py_2().child(button(
                    "解散群聊",
                    false,
                    false,
                    cx.listener(Self::on_dissolve_group),
                )))
            })
            .when(!is_owner, |this| {
                this.child(div().px_4().py_2().child(button(
                    "退出群聊",
                    false,
                    false,
                    cx.listener(Self::on_leave_group),
                )))
            })
            .into_any_element()
    }
}
