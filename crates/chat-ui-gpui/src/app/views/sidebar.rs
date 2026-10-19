use super::*;

impl ChatApp {
    pub(crate) fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        // 渲染期间要用 cx 注册点击回调，所以先把要用的状态取出来，避免同时借用 self
        let items = self.rooms.clone();
        let empty = items.is_empty();
        let local = self.local_room;
        // 「新建房间」连库一起删的是本机房间：本机有房间（不用的在侧栏挂着，在用的正托管着）
        // 就得提示，否则正用着的时候反而是没提示的时候
        let local_exists = local.is_some() || self.hosted.is_some();
        let current = self.room.as_ref().map(|view| view.id);
        // 「回到我的房间」那一行：本机有房间、且界面不是正待在这个房间里时才摆
        let local_row = local.is_some() && current != self.local_room_id;
        let joined = self.room.as_ref().map(|view| view.joined).unwrap_or(false);

        let rows: Vec<AnyElement> = items
            .into_iter()
            .enumerate()
            .map(|(index, room)| {
                let selected = current == Some(room.id);
                room_row(index, room, selected, joined, cx)
            })
            .collect();

        // 侧栏固定 240px、不参与伸缩；只有主体滚动，底部连接区始终留在窗口内。
        // 房间身份和信任指纹都是长文本，以前放在可滚动的整列里，窗口不够高时会把
        // 「直接连接 / 新建房间」挤到视口之外。
        div()
            .id("sidebar")
            .test_id("sidebar")
            .w(px(240.))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE))
            .border_r_1()
            .border_color(rgb(theme::BORDER))
            .child(
                div()
                    .id("sidebar-body")
                    .test_id("sidebar-body")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .py_3()
                    .when_some(
                        self.pending_trust.clone(),
                        |this, (room, addr, fingerprint)| {
                            this.child(section_label("首次信任：向房主核对"))
                                .child(hint(&format!("房间: {room}")))
                                .child(hint(&format!("地址: {addr}")))
                                .child(hint(&format!(
                                    "SHA-256:\n{}",
                                    fingerprint
                                        .as_bytes()
                                        .chunks(16)
                                        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                )))
                                .child(padded(button(
                                    "已核对并信任",
                                    false,
                                    true,
                                    cx.listener(Self::on_confirm_trust),
                                )))
                                .child(padded(button(
                                    "取消",
                                    false,
                                    true,
                                    cx.listener(Self::on_cancel_trust),
                                )))
                        },
                    )
                    .child(section_label("局域网房间"))
                    .child(
                        div()
                            .id("rooms")
                            .flex()
                            .flex_col()
                            .children(rows)
                            .when(empty, |this| this.child(hint("未发现房间，可手动连接"))),
                    )
                    // 本机那个房间不在局域网里——它要点了才会被广播出去。
                    // 已经待在这个房间里（可能是本机另一个实例开着、我们接过去）就别再摆入口了
                    .when(local_row, |this| {
                        this.child(section_label("本机房间"))
                            .child(local_room_row(local.unwrap_or(0), cx))
                    })
                    .when_some(self.hosted.clone(), |this, hosted| {
                        this.child(section_label("我的房间身份"))
                            .child(hint(&hosted))
                    }),
            )
            .child(
                div()
                    .id("sidebar-footer")
                    .test_id("sidebar-footer")
                    .flex_none()
                    .py_2()
                    .border_t_1()
                    .border_color(rgb(theme::BORDER))
                    .child(section_label("直接连接"))
                    .child(padded(self.addr_input.clone()))
                    .child(padded(self.fingerprint_input.clone()))
                    .child(padded(button(
                        "连接",
                        false,
                        true,
                        cx.listener(Self::on_connect_click),
                    )))
                    .child(section_label("昵称"))
                    .child(padded(self.nickname.clone()))
                    .child(padded(button(
                        "新建房间",
                        true,
                        true,
                        cx.listener(Self::on_new_room),
                    )))
                    .when(local_exists, |this| {
                        this.child(hint("新建会清空本机房间的历史"))
                    }),
            )
            .into_any_element()
    }
}

/// 侧栏里「回到本机房间」的一行：点一下就把它重新开张，历史都还在。
fn local_room_row(messages: u64, cx: &mut Context<ChatApp>) -> AnyElement {
    let subtitle = if messages == 0 {
        "暂无历史".to_string()
    } else {
        format!("历史 {messages} 条")
    };

    div()
        .id("local-room")
        .flex()
        .flex_row()
        .items_center()
        .pl_3()
        .pr_5()
        .py_2()
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::HOVER)))
        .child(
            div()
                .w(px(2.))
                .h(px(14.))
                .mr_3()
                .rounded_md()
                .bg(rgb(theme::ACCENT)),
        )
        .child(
            div()
                .flex_1()
                .text_sm()
                .text_color(rgb(theme::TEXT))
                .child("我的房间"),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::FAINT))
                .child(subtitle),
        )
        .on_mouse_up(MouseButton::Left, cx.listener(ChatApp::on_host))
        .into_any_element()
}

/// 房间列表里的一行。
///
/// 选中态用「左侧短竖条 + 低饱和强调底」表示，已加入的再加一个实心点。
fn room_row(
    index: usize,
    room: DiscoveredRoom,
    selected: bool,
    joined: bool,
    cx: &mut Context<ChatApp>,
) -> AnyElement {
    // 选中与否决定了四处配色，集中在这里算一次
    let (name_color, count_color, marker) = if selected {
        (theme::ACCENT, theme::MUTED, if joined { "● " } else { "" })
    } else {
        (theme::TEXT, theme::FAINT, "")
    };
    let addr = room.addr.clone();
    let fingerprint = room.fingerprint.clone();
    let room_id = room.id;

    div()
        .id(("room", index))
        .flex()
        .flex_row()
        .items_center()
        .pl_3()
        .pr_5()
        .py_2()
        .cursor_pointer()
        .when(selected, |this| this.bg(rgb(theme::ACCENT_SOFT)))
        .hover(|style| style.bg(rgb(theme::HOVER)))
        .child(
            // 选中时显示强调色竖条；未选中留出同样宽度，避免文字左右跳动
            div()
                .w(px(2.))
                .h(px(14.))
                .mr_3()
                .rounded_md()
                .when(selected, |this| this.bg(rgb(theme::ACCENT))),
        )
        .child(
            div()
                .flex_1()
                .text_sm()
                .text_color(rgb(name_color))
                .child(room.name.clone()),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(count_color))
                .child(format!("{marker}{} 人", room.online)),
        )
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseUpEvent, _window, cx| {
                this.on_room_click(room_id, addr.clone(), fingerprint.clone(), cx);
            }),
        )
        .into_any_element()
}
