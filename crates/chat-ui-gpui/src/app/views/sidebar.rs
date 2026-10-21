use super::*;
use crate::app::NavTab;
use chat_core::RoomId;

impl ChatApp {
    /// 左侧列表：聊天 / 通讯录 / 房间。
    pub(crate) fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.nav {
            NavTab::Chats => self.render_chats_sidebar(cx),
            NavTab::Contacts => self.render_contacts_sidebar(cx),
            NavTab::Rooms => self.render_rooms_sidebar(cx),
        }
    }

    /// 微信式最左侧导航条。
    pub(crate) fn render_nav(&self, cx: &mut Context<Self>) -> AnyElement {
        let account_avatar = self
            .account
            .as_ref()
            .map(|user| avatar(user.display_name.chars().next().unwrap_or('?').to_string()));
        div()
            .id("nav")
            .test_id("nav")
            .w(px(56.))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .py_3()
            .gap_2()
            .bg(rgb(theme::BG))
            .border_r_1()
            .border_color(rgb(theme::BORDER))
            .child(nav_button(
                "聊",
                "nav-chats",
                self.nav == NavTab::Chats,
                cx.listener(Self::on_nav_chats),
            ))
            .child(nav_button(
                "友",
                "nav-contacts",
                self.nav == NavTab::Contacts,
                cx.listener(Self::on_nav_contacts),
            ))
            .child(nav_button(
                "房",
                "nav-rooms",
                self.nav == NavTab::Rooms,
                cx.listener(Self::on_nav_rooms),
            ))
            .child(div().flex_1())
            .when_some(account_avatar, |this, avatar| this.child(avatar))
            .into_any_element()
    }

    /// 聊天页：搜索 + 会话列表。
    pub(crate) fn render_chats_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let query = self.search.read(cx).content().trim().to_lowercase();
        let rooms: Vec<_> = self
            .conversations
            .iter()
            .filter(|room| query.is_empty() || room.name.to_lowercase().contains(&query))
            .cloned()
            .collect();
        let current = self.room.as_ref().map(|view| view.id);
        let rows: Vec<_> = rooms
            .into_iter()
            .enumerate()
            .map(|(index, room)| conversation_item(index, room, current, cx))
            .collect();
        let empty = rows.is_empty();

        div()
            .id("sidebar")
            .test_id("sidebar")
            .w(px(260.))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE))
            .border_r_1()
            .border_color(rgb(theme::BORDER))
            .child(list_title("聊天"))
            .child(padded(self.search.clone()))
            .child(
                div()
                    .id("conversations")
                    .test_id("conversations")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .when(empty, |this| {
                        this.child(hint("还没有会话。去通讯录找好友，或发起群聊。"))
                    })
                    .children(rows),
            )
            .child(padded(button(
                "房间与连接",
                false,
                true,
                cx.listener(Self::on_nav_rooms),
            )))
            .into_any_element()
    }

    /// 通讯录页：新的朋友、群聊、好友、发起群聊。
    pub(crate) fn render_contacts_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let query = self.search.read(cx).content().trim().to_lowercase();
        let groups: Vec<_> = self
            .conversations
            .iter()
            .filter(|room| room.kind == chat_core::RoomKind::Group)
            .filter(|room| query.is_empty() || room.name.to_lowercase().contains(&query))
            .cloned()
            .collect();
        let friends: Vec<_> = self
            .friends
            .iter()
            .filter(|user| {
                query.is_empty()
                    || user.display_name.to_lowercase().contains(&query)
                    || user.username.to_lowercase().contains(&query)
            })
            .cloned()
            .collect();
        let group_rows: Vec<_> = groups
            .into_iter()
            .enumerate()
            .map(|(index, room)| conversation_item(index, room, None, cx))
            .collect();
        let friend_rows: Vec<_> = friends
            .into_iter()
            .enumerate()
            .map(|(index, user)| friend_item(index, user, cx))
            .collect();

        div()
            .id("sidebar")
            .test_id("sidebar")
            .w(px(260.))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE))
            .border_r_1()
            .border_color(rgb(theme::BORDER))
            .child(list_title("通讯录"))
            .child(padded(self.search.clone()))
            .child(
                div()
                    .id("contacts-body")
                    .test_id("contacts-body")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .pb_3()
                    .child(section_label("新的朋友"))
                    .child(padded(self.friend_input.clone()))
                    .child(padded(button(
                        "添加好友",
                        false,
                        true,
                        cx.listener(Self::on_add_friend),
                    )))
                    .child(section_label("群聊"))
                    .child(
                        div()
                            .id("groups")
                            .test_id("groups")
                            .flex()
                            .flex_col()
                            .when(group_rows.is_empty(), |this| this.child(hint("还没有群聊")))
                            .children(group_rows),
                    )
                    .child(section_label("好友"))
                    .child(
                        div()
                            .id("friends")
                            .test_id("friends")
                            .flex()
                            .flex_col()
                            .when(friend_rows.is_empty(), |this| {
                                this.child(hint("还没有好友"))
                            })
                            .children(friend_rows),
                    )
                    .child(section_label("发起群聊"))
                    .child(padded(self.group_input.clone()))
                    .child(padded(button(
                        "拉好友建群",
                        false,
                        true,
                        cx.listener(Self::on_create_group),
                    ))),
            )
            .into_any_element()
    }

    pub(crate) fn render_rooms_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        // 渲染期间要用 cx 注册点击回调，所以先把要用的状态取出来，避免同时借用 self
        let items = self.rooms.clone();
        let local = self.local_room;
        // 「新建房间」连库一起删的是本机房间：本机有房间（不用的在侧栏挂着，在用的正托管着）
        // 就得提示，否则正用着的时候反而是没提示的时候
        let local_exists = local.is_some() || self.hosted.is_some();
        let current = self.room.as_ref().map(|view| view.id);
        // 「回到我的房间」那一行：本机有房间、且界面不是正待在这个房间里时才摆
        let local_row = local.is_some() && current != self.local_room_id;
        let joined = self.room.as_ref().map(|view| view.joined).unwrap_or(false);
        let identity_open = self.show_identity;

        let mut local_rooms = Vec::new();
        let mut remote_rooms = Vec::new();
        for (index, room) in items.into_iter().enumerate() {
            let selected = current == Some(room.id);
            let source = room.source;
            let row = room_row(index, room, selected, joined, cx);
            match source {
                RoomSource::Local => local_rooms.push(row),
                RoomSource::Remote => remote_rooms.push(row),
            }
        }
        let local_empty = local_rooms.is_empty();
        let remote_empty = remote_rooms.is_empty();

        // 侧栏固定 256px、不参与伸缩；只有主体滚动，底部操作区始终留在窗口内。
        // 连接与公网表单改成从底部按钮展开的面板，底部不再常驻两个输入框。
        div()
            .id("sidebar")
            .test_id("sidebar")
            .w(px(256.))
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
                    .pb_3()
                    .when(self.show_connect_form, |this| {
                        this.child(self.render_connect_panel(cx))
                    })
                    .when(self.show_public_form, |this| {
                        this.child(self.render_public_panel(cx))
                    })
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
                    // 会话列表（群聊 + 单聊）：登录后由 Hub 推送
                    .when(!self.conversations.is_empty(), |this| {
                        this.child(section_label("会话")).child(
                            div()
                                .id("conversations")
                                .test_id("conversations")
                                .flex()
                                .flex_col()
                                .children(self.conversations.iter().enumerate().map(
                                    |(index, room)| {
                                        let room_id = room.room;
                                        let label =
                                            format!("{} · {} 人", room.name, room.member_count);
                                        div()
                                            .id(("conversation", index))
                                            .h(px(36.))
                                            .pl_4()
                                            .pr_4()
                                            .flex()
                                            .items_center()
                                            .cursor_pointer()
                                            .hover(|style| style.bg(rgb(theme::HOVER)))
                                            .on_mouse_up(
                                                MouseButton::Left,
                                                cx.listener(move |this, _, _, cx| {
                                                    this.open_conversation(room_id, cx)
                                                }),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w(px(0.))
                                                    .truncate()
                                                    .text_sm()
                                                    .text_color(rgb(theme::TEXT))
                                                    .child(label),
                                            )
                                            .into_any_element()
                                    },
                                )),
                        )
                    })
                    .when(!self.friends.is_empty(), |this| {
                        this.child(section_label("好友")).child(
                            div()
                                .id("friends")
                                .test_id("friends")
                                .flex()
                                .flex_col()
                                .children(self.friends.iter().enumerate().map(|(index, user)| {
                                    let user_id = user.id;
                                    let label = user.display_name.clone();
                                    div()
                                        .id(("friend", index))
                                        .h(px(36.))
                                        .pl_4()
                                        .pr_4()
                                        .flex()
                                        .items_center()
                                        .cursor_pointer()
                                        .hover(|style| style.bg(rgb(theme::HOVER)))
                                        .on_mouse_up(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, _, cx| {
                                                this.on_open_direct(user_id, cx)
                                            }),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w(px(0.))
                                                .truncate()
                                                .text_sm()
                                                .text_color(rgb(theme::TEXT))
                                                .child(label),
                                        )
                                        .into_any_element()
                                })),
                        )
                    })
                    .child(section_label("添加好友"))
                    .child(padded(self.friend_input.clone()))
                    .child(padded(button(
                        "添加好友",
                        false,
                        true,
                        cx.listener(Self::on_add_friend),
                    )))
                    .child(section_label("新建群聊"))
                    .child(padded(self.group_input.clone()))
                    .child(padded(button(
                        "拉好友建群",
                        false,
                        true,
                        cx.listener(Self::on_create_group),
                    )))
                    // 「我的房间」：托管中或可恢复，放最上面
                    .child(section_header(
                        "我的房间",
                        if self.show_public_form {
                            "收起"
                        } else {
                            "公网服务"
                        },
                        cx.listener(Self::on_toggle_public_form),
                    ))
                    .when_some(self.hosted.clone(), |this, hosted| {
                        this.child(identity_card(hosted, identity_open, cx))
                    })
                    .when(local_row, |this| {
                        this.child(local_room_row(local.unwrap_or(0), cx))
                    })
                    .child(section_label("局域网房间"))
                    .child(
                        div()
                            .id("rooms")
                            .flex()
                            .flex_col()
                            .children(local_rooms)
                            .when(local_empty, |this| {
                                this.child(hint("未发现房间，点底部「手动连接」"))
                            }),
                    )
                    // 公网没有广播：这里列的是连过一次、已存进地址簿的服务器
                    .when(!remote_empty, |this| {
                        this.child(section_label("远程服务器")).child(
                            div()
                                .id("remote-rooms")
                                .test_id("remote-rooms")
                                .flex()
                                .flex_col()
                                .children(remote_rooms),
                        )
                    }),
            )
            .child(
                div()
                    .id("sidebar-footer")
                    .test_id("sidebar-footer")
                    .flex_none()
                    .pt_3()
                    .pb_3()
                    .border_t_1()
                    .border_color(rgb(theme::BORDER))
                    .child(padded(self.nickname.clone()))
                    // 两个入口并排，矮窗口下也不额外占一行高度
                    .child(padded(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(div().flex_1().child(button(
                                if self.show_connect_form {
                                    "收起连接"
                                } else {
                                    "手动连接"
                                },
                                false,
                                true,
                                cx.listener(Self::on_toggle_connect_form),
                            )))
                            .child(div().flex_1().child(button(
                                "新建房间",
                                true,
                                true,
                                cx.listener(Self::on_new_room),
                            ))),
                    ))
                    .when(local_exists, |this| {
                        this.child(hint("新建会清空本机房间的历史"))
                    }),
            )
            .into_any_element()
    }

    /// 手动连接：地址 + 可选指纹。从底部「连接」展开，提交后自动收起。
    fn render_connect_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("connect-form")
            .test_id("connect-form")
            .flex()
            .flex_col()
            .bg(rgb(theme::BG))
            .border_b_1()
            .border_color(rgb(theme::BORDER))
            .child(section_label("连接房间"))
            .child(form_field("地址", self.addr_input.clone()))
            .child(form_field(
                "证书指纹（64 位，受信证书留空）",
                self.fingerprint_input.clone(),
            ))
            .child(padded(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(div().flex_1().child(button(
                        "连接",
                        true,
                        true,
                        cx.listener(Self::on_connect_click),
                    )))
                    .child(div().flex_1().child(button(
                        "取消",
                        false,
                        true,
                        cx.listener(Self::on_toggle_connect_form),
                    ))),
            ))
            .child(hint("自签房间填 64 位 SHA-256；受信证书留空"))
            .into_any_element()
    }

    /// 公网托管：端口与证书；从「我的房间」右侧的「公网服务」展开。
    fn render_public_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("public-form")
            .test_id("public-form")
            .flex()
            .flex_col()
            .bg(rgb(theme::BG))
            .border_b_1()
            .border_color(rgb(theme::BORDER))
            .child(section_label("公网服务端"))
            .child(form_field("监听端口", self.public_port.clone()))
            .child(form_field(
                "证书链 PEM（留空自签）",
                self.public_cert.clone(),
            ))
            .child(form_field("私钥 PEM（留空自签）", self.public_key.clone()))
            .child(form_field(
                "域名（可选，用于自动加入）",
                self.public_domain.clone(),
            ))
            .child(padded(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(div().flex_1().child(button(
                        "启动公网服务",
                        true,
                        true,
                        cx.listener(Self::on_start_public),
                    )))
                    .child(div().flex_1().child(button(
                        "取消",
                        false,
                        true,
                        cx.listener(Self::on_toggle_public_form),
                    ))),
            ))
            .child(hint("证书与私钥留空时使用自签，客户端填指纹"))
            .into_any_element()
    }
}

/// 表单字段：小标题在上，输入框在下。
fn form_field(label: &str, input: impl IntoElement) -> AnyElement {
    div()
        .px_4()
        .pb_2()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(10.))
                .text_color(rgb(theme::FAINT))
                .child(label.to_string()),
        )
        .child(input)
        .into_any_element()
}

/// 带右侧入口的小节标题（例如「我的房间」+「公网服务」）。
fn section_header(
    label: &str,
    action: &str,
    on_click: impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .px_4()
        .pt_3()
        .pb_2()
        .child(
            div()
                .text_size(px(11.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(rgb(theme::FAINT))
                .child(label.to_string()),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(theme::ACCENT))
                .cursor_pointer()
                .hover(|style| style.text_color(rgb(theme::ACCENT_HOVER)))
                .on_mouse_up(MouseButton::Left, on_click)
                .child(action.to_string()),
        )
        .into_any_element()
}

/// 本机房间摘要：名字 + 房间短标识 + 一个可用地址，完整信息按需展开。
///
/// 完整标识与 64 位指纹常驻侧栏会把「直接连接 / 新建房间」挤下去，也压过了房间列表的视觉重心。
fn identity_card(hosted: HostedRoom, expanded: bool, cx: &mut Context<ChatApp>) -> AnyElement {
    let summary = if hosted.is_public() {
        format!("公网 · {}", hosted.summary_addr())
    } else {
        format!("{} · {}", hosted.short_room(), hosted.summary_addr())
    };
    let toggle = div()
        .id("identity-toggle")
        .test_id("identity-toggle")
        .flex_none()
        .px_2()
        .py_1()
        .rounded_md()
        .text_xs()
        .text_color(rgb(theme::ACCENT))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::HOVER)))
        .on_mouse_up(MouseButton::Left, cx.listener(ChatApp::on_toggle_identity))
        .child(if expanded { "收起" } else { "详情" });

    let fingerprint = hosted.fingerprint.clone();
    let join_addr = hosted.join_addr();
    let public = hosted.public.clone();

    let mut card = div()
        .id("hosted-room")
        .test_id("hosted-room")
        .px_5()
        .pb_2()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .text_sm()
                        .text_color(rgb(theme::TEXT))
                        .child(hosted.name.clone()),
                )
                .child(copy_text_button(
                    "copy-fingerprint",
                    fingerprint.clone(),
                    "证书指纹",
                    cx,
                ))
                .child(toggle),
        )
        .child(
            div()
                .truncate()
                .text_xs()
                .text_color(rgb(theme::FAINT))
                .child(summary),
        );

    if expanded {
        card = card.child(
            div()
                .id("identity-details")
                .test_id("identity-details")
                .flex()
                .flex_col()
                .child(identity_detail(
                    "房间（分享用，不是连接指纹）",
                    hosted.room.to_string(),
                    Some((hosted.room.to_string(), "房间标识", "copy-room")),
                    cx,
                ))
                .child(identity_detail(
                    if hosted.is_public() {
                        "监听"
                    } else {
                        "地址"
                    },
                    hosted.addrs.join("\n"),
                    hosted
                        .addrs
                        .first()
                        .cloned()
                        .map(|addr| (addr, "监听地址", "copy-listen")),
                    cx,
                ))
                .when_some(public, |this, public| {
                    this.child(identity_detail(
                        "接入地址（客户端填这个）",
                        join_addr.clone(),
                        Some((join_addr.clone(), "接入地址", "copy-join-addr")),
                        cx,
                    ))
                    .child(identity_detail(
                        "证书",
                        if public.trusted_ca {
                            "受信 CA 证书，客户端无需指纹"
                        } else {
                            "自签，客户端需填下面的 SHA-256"
                        }
                        .to_string(),
                        None,
                        cx,
                    ))
                })
                .child(identity_detail(
                    "证书 SHA-256（客户端指纹）",
                    hosted.fingerprint_rows(),
                    Some((fingerprint.clone(), "证书指纹", "copy-fingerprint-detail")),
                    cx,
                )),
        );
    }

    card.into_any_element()
}

/// 详情里的一枚「复制」按钮：把原文放进系统剪贴板，并在状态栏说明复制了什么。
fn copy_text_button(
    id: &'static str,
    text: String,
    label: &'static str,
    cx: &mut Context<ChatApp>,
) -> AnyElement {
    div()
        .id(id)
        .test_id(id)
        .flex_none()
        .px_2()
        .py_1()
        .rounded_md()
        .text_size(px(11.))
        .text_color(rgb(theme::ACCENT))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::HOVER)))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| this.copy_text(text.clone(), label, cx)),
        )
        .child("复制")
        .into_any_element()
}

/// 展开态的一行：小标题在上，值在下，右侧可选一枚复制按钮。
fn identity_detail(
    label: &str,
    value: String,
    copy: Option<(String, &'static str, &'static str)>,
    cx: &mut Context<ChatApp>,
) -> AnyElement {
    div()
        .mt_1()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::FAINT))
                .child(label.to_string()),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_xs()
                        .text_color(rgb(theme::MUTED))
                        .child(value),
                )
                .when_some(copy, |this, (text, note, id)| {
                    this.child(copy_text_button(id, text, note, cx))
                }),
        )
        .into_any_element()
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
        .h(px(36.))
        .pl_4()
        .pr_4()
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::HOVER)))
        .child(
            div()
                .w(px(2.))
                .h(px(16.))
                .mr_3()
                .rounded_full()
                .bg(rgb(theme::ACCENT)),
        )
        .child(
            div()
                .flex_1()
                .text_sm()
                .text_color(rgb(theme::TEXT))
                .child("继续上次的房间"),
        )
        .child(
            div()
                .text_size(px(11.))
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
    let (name_color, count_color) = if selected {
        (theme::TEXT, theme::MUTED)
    } else {
        (theme::TEXT, theme::FAINT)
    };
    let addr = room.addr.clone();
    let fingerprint = room.fingerprint.clone();
    let room_id = room.id;

    div()
        .id(("room", index))
        .flex()
        .flex_row()
        .items_center()
        .h(px(36.))
        .pl_4()
        .pr_4()
        .cursor_pointer()
        .when(selected, |this| this.bg(rgb(theme::HOVER)))
        .hover(|style| style.bg(rgb(theme::HOVER)))
        .child(
            // 选中时显示强调色竖条；未选中留出同样宽度，避免文字左右跳动
            div()
                .w(px(2.))
                .h(px(16.))
                .mr_3()
                .rounded_full()
                .when(selected, |this| this.bg(rgb(theme::ACCENT))),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_sm()
                .text_color(rgb(name_color))
                .child(room.name.clone()),
        )
        // 已加入用实心点表示，比文字更安静
        .when(joined, |this| {
            this.child(
                div()
                    .flex_none()
                    .w(px(6.))
                    .h(px(6.))
                    .mr_2()
                    .rounded_full()
                    .bg(rgb(theme::ACCENT)),
            )
        })
        .child(
            div()
                .flex_none()
                .text_size(px(11.))
                .text_color(rgb(count_color))
                .child(format!("{} 人", room.online)),
        )
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseUpEvent, _window, cx| {
                this.on_room_click(room_id, addr.clone(), fingerprint.clone(), cx);
            }),
        )
        .into_any_element()
}

fn list_title(text: &str) -> AnyElement {
    div()
        .px_4()
        .pt_4()
        .pb_2()
        .text_size(px(18.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(rgb(theme::TEXT))
        .child(text.to_string())
        .into_any_element()
}

fn nav_button(
    label: &'static str,
    test_id: &'static str,
    active: bool,
    on_click: impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(test_id)
        .test_id(test_id)
        .size(px(40.))
        .rounded_lg()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_size(px(13.))
        .when(active, |this| {
            this.bg(rgb(theme::ACCENT))
                .text_color(rgb(theme::ON_ACCENT))
        })
        .when(!active, |this| {
            this.text_color(rgb(theme::MUTED))
                .hover(|style| style.bg(rgb(theme::HOVER)))
        })
        .child(label)
        .on_mouse_up(MouseButton::Left, on_click)
        .into_any_element()
}

fn avatar(text: String) -> AnyElement {
    div()
        .size(px(36.))
        .flex_none()
        .rounded_full()
        .bg(rgb(theme::ACCENT))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(14.))
        .text_color(rgb(theme::ON_ACCENT))
        .child(text)
        .into_any_element()
}

fn conversation_item(
    index: usize,
    room: chat_core::RoomSummary,
    current: Option<RoomId>,
    cx: &mut Context<ChatApp>,
) -> AnyElement {
    let room_id = room.room;
    let selected = current == Some(room_id);
    let name = room.name.clone();
    let first = name.chars().next().unwrap_or('?').to_string();
    let subtitle = if room.kind == chat_core::RoomKind::Direct {
        "单聊".to_string()
    } else {
        format!("{} 人", room.member_count)
    };
    let badge = if room.last_seq > 0 {
        room.last_seq.to_string()
    } else {
        String::new()
    };
    div()
        .id(("conversation-item", index))
        .h(px(60.))
        .px_3()
        .flex()
        .items_center()
        .gap_3()
        .cursor_pointer()
        .when(selected, |this| this.bg(rgb(theme::HOVER)))
        .hover(|style| style.bg(rgb(theme::HOVER)))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| this.open_conversation(room_id, cx)),
        )
        .child(avatar(first))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .truncate()
                        .text_sm()
                        .text_color(rgb(theme::TEXT))
                        .child(name),
                )
                .child(
                    div()
                        .truncate()
                        .text_xs()
                        .text_color(rgb(theme::FAINT))
                        .child(subtitle),
                ),
        )
        .child(div().text_xs().text_color(rgb(theme::FAINT)).child(badge))
        .into_any_element()
}

fn friend_item(
    index: usize,
    user: chat_core::UserSummary,
    cx: &mut Context<ChatApp>,
) -> AnyElement {
    let user_id = user.id;
    let name = user.display_name.clone();
    let first = name.chars().next().unwrap_or('?').to_string();
    let username = user.username.clone();
    div()
        .id(("friend-item", index))
        .h(px(56.))
        .px_3()
        .flex()
        .items_center()
        .gap_3()
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::HOVER)))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| this.on_open_direct(user_id, cx)),
        )
        .child(avatar(first))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .truncate()
                        .text_sm()
                        .text_color(rgb(theme::TEXT))
                        .child(name),
                )
                .child(
                    div()
                        .truncate()
                        .text_xs()
                        .text_color(rgb(theme::FAINT))
                        .child(username),
                ),
        )
        .into_any_element()
}
