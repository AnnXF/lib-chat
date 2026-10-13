use super::*;

impl ChatApp {
    pub(crate) fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        // 渲染期间要用 cx 注册点击回调，所以先把房间列表取出来，避免同时借用 self.rooms
        let items = self.rooms.clone();
        let empty = items.is_empty();
        let current = self.room.as_ref().map(|view| view.id);
        let joined = self.room.as_ref().map(|view| view.joined).unwrap_or(false);

        let rows = items.into_iter().enumerate().map(|(index, room)| {
            let selected = current == Some(room.id);
            room_row(index, room, selected, joined, cx)
        });

        div()
            .w(px(240.))
            .flex()
            .flex_col()
            .py_3()
            .bg(rgb(theme::SURFACE))
            .border_r_1()
            .border_color(rgb(theme::BORDER))
            .child(section_label("局域网房间"))
            .child(
                div()
                    .id("rooms")
                    .flex()
                    .flex_col()
                    .max_h(px(200.))
                    .overflow_y_scroll()
                    .children(rows)
                    .when(empty, |this| this.child(hint("未发现房间，可手动连接"))),
            )
            .child(div().flex_1())
            .child(section_label("直接连接"))
            .child(padded(self.addr_input.clone()))
            .child(padded(button(
                "连接",
                false,
                true,
                cx.listener(Self::on_connect_click),
            )))
            .child(section_label("昵称"))
            .child(padded(self.nickname.clone()))
            .child(padded(button(
                "创建房间",
                true,
                true,
                cx.listener(Self::on_host),
            )))
            .into_any_element()
    }
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
