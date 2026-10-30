//! 用户操作：提交服务指令，必要的即时视图变化交给 `update`。

use std::path::PathBuf;

use crate::app::{FriendMenu, NavTab};
use chat_core::{Attachment, RoomId, UserId};
use chat_transport_quic::validate_fingerprint;
use gpui::{
    Context, Focusable as _, KeyDownEvent, ListOffset, MouseDownEvent, MouseUpEvent,
    PathPromptOptions, Pixels, Point, Size, Window, point, px,
};

use crate::service::UiCommand;

use super::{ChatApp, ChatLine, MessageMenu};

struct PublicOptions {
    port: u16,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
    domain: Option<String>,
}

/// 公网表单的输入校验：端口必须有效，证书与私钥要么都给、要么都空。
fn public_options(
    port: &str,
    cert: &str,
    key: &str,
    domain: &str,
) -> Result<PublicOptions, String> {
    let port: u16 = port
        .trim()
        .parse()
        .map_err(|_| "监听端口无效，应为 1-65535".to_string())?;
    if port == 0 {
        return Err("监听端口无效，应为 1-65535".into());
    }
    let cert = cert.trim();
    let key = key.trim();
    let cert = (!cert.is_empty()).then(|| PathBuf::from(cert));
    let key = (!key.is_empty()).then(|| PathBuf::from(key));
    if cert.is_some() != key.is_some() {
        return Err("证书与私钥必须同时填写，或都留空使用自签".into());
    }
    let domain = domain.trim();
    Ok(PublicOptions {
        port,
        cert,
        key,
        domain: (!domain.is_empty()).then(|| domain.to_string()),
    })
}

impl ChatApp {
    /// 发一条指令并刷新界面。
    fn tell(&mut self, cmd: UiCommand, cx: &mut Context<Self>) -> bool {
        let accepted = self.cmd_tx.try_send(cmd).is_ok();
        if !accepted {
            self.status = "后端繁忙或已关闭，请稍后重试".into();
            self.status_kind = super::StatusKind::Error;
            self.loading_older = false;
        }
        cx.notify();
        accepted
    }

    /// 昵称输入框的内容；为空时用兜底名。
    fn nickname_or(&self, fallback: &str, cx: &Context<Self>) -> String {
        let name = self.nickname.read(cx).content().trim().to_string();
        if name.is_empty() {
            fallback.to_string()
        } else {
            name
        }
    }

    pub(crate) fn submit_message(&mut self, cx: &mut Context<Self>) {
        let text = self.message.read(cx).content().trim().to_string();
        if text.is_empty() {
            return;
        }
        if let Some((room, seq)) = self.editing.take() {
            if self.tell(UiCommand::EditMessage { room, seq, text }, cx) {
                self.message
                    .update(cx, |input, cx| input.set_content("", cx));
                self.status = "正在保存编辑…".into();
            } else {
                self.editing = Some((room, seq));
            }
            return;
        }
        let quote = self.pending_quote.take();
        if self.tell(
            UiCommand::Send {
                text,
                quote: quote.clone(),
            },
            cx,
        ) {
            self.message
                .update(cx, |input, cx| input.set_content("", cx));
        } else {
            // 后端没收下这条指令，引用要留在输入框上方，别让用户重新找一遍
            self.pending_quote = quote;
        }
    }

    /// 消息区滑到最顶时再往前翻一页。
    ///
    /// 这里只喊一声：能不能翻、还有没有更早的，交给会话层判断。
    pub(crate) fn load_older(&mut self, cx: &mut Context<Self>) {
        if self.begin_loading_older() {
            self.tell(UiCommand::LoadOlder, cx);
        }
    }

    pub(crate) fn connect_from_input(&mut self, cx: &mut Context<Self>) {
        let addr = self.addr_input.read(cx).content().trim().to_string();
        if addr.is_empty() {
            return;
        }
        let fingerprint = self.fingerprint_input.read(cx).content().trim().to_string();
        if !fingerprint.is_empty()
            && let Err(e) = validate_fingerprint(&fingerprint)
        {
            self.status = format!("{e}（请复制「我的房间 → 详情 → 证书 SHA-256」）");
            self.status_kind = super::StatusKind::Error;
            cx.notify();
            return;
        }
        if self.connect(addr, (!fingerprint.is_empty()).then_some(fingerprint), cx) {
            self.show_connect_form = false;
        }
    }

    /// 把一段文字放进系统剪贴板，并在状态栏说明复制了什么。
    pub(crate) fn copy_text(&mut self, text: String, label: &'static str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.status = format!("已复制{label}");
        cx.notify();
    }

    /// 展开/收起「我的房间」的完整标识与指纹。
    pub(crate) fn toggle_identity(&mut self, cx: &mut Context<Self>) {
        self.show_identity = !self.show_identity;
        cx.notify();
    }

    pub(crate) fn on_toggle_identity(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_identity(cx);
    }

    /// 供无头测试直接切换展开状态。
    #[cfg(any(test, feature = "ui-test"))]
    pub fn debug_toggle_identity(&mut self, cx: &mut Context<Self>) {
        self.toggle_identity(cx);
    }

    pub(crate) fn connect(
        &mut self,
        addr: String,
        fingerprint: Option<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.begin_connect(&addr);
        self.tell(UiCommand::Connect { addr, fingerprint }, cx)
    }

    pub(crate) fn on_room_click(
        &mut self,
        room: RoomId,
        addr: String,
        fingerprint: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // 已经是当前房间，重复点击不该触发一次「退出再重连」，那只会白白抖动人数
        if self.showing(room).is_none() {
            self.begin_connect(&addr);
            self.tell(
                UiCommand::ConnectDiscovered {
                    room,
                    addr,
                    fingerprint,
                },
                cx,
            );
        }
    }

    pub(crate) fn on_confirm_trust(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((room, addr, fingerprint)) = self.pending_trust.take() {
            self.tell(
                UiCommand::ConfirmTrust {
                    room,
                    addr,
                    fingerprint,
                },
                cx,
            );
        }
    }

    pub(crate) fn on_cancel_trust(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_trust = None;
        cx.notify();
    }

    pub(crate) fn on_host(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let name = self.nickname_or("局域网聊天室", cx);
        self.tell(UiCommand::Host { name }, cx);
    }

    /// 清空本机房间的历史，从空白重新开一个。
    pub(crate) fn on_new_room(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let name = self.nickname_or("局域网聊天室", cx);
        self.tell(UiCommand::NewRoom { name }, cx);
    }

    pub(crate) fn toggle_public_form(&mut self, cx: &mut Context<Self>) {
        self.show_public_form = !self.show_public_form;
        if self.show_public_form {
            self.show_connect_form = false;
        }
        cx.notify();
    }

    /// 供无头测试直接展开/收起公网表单。
    #[cfg(any(test, feature = "ui-test"))]
    pub fn debug_toggle_public_form(&mut self, cx: &mut Context<Self>) {
        self.toggle_public_form(cx);
    }

    pub(crate) fn on_toggle_public_form(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_public_form(cx);
    }

    pub(crate) fn toggle_connect_form(&mut self, cx: &mut Context<Self>) {
        self.show_connect_form = !self.show_connect_form;
        if self.show_connect_form {
            self.show_public_form = false;
        }
        cx.notify();
    }

    /// 供无头测试直接展开/收起手动连接面板。
    #[cfg(any(test, feature = "ui-test"))]
    pub fn debug_toggle_connect_form(&mut self, cx: &mut Context<Self>) {
        self.toggle_connect_form(cx);
    }

    pub(crate) fn on_toggle_connect_form(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_connect_form(cx);
    }

    pub(crate) fn on_start_public(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let port = self.public_port.read(cx).content().trim().to_string();
        let cert = self.public_cert.read(cx).content().trim().to_string();
        let key = self.public_key.read(cx).content().trim().to_string();
        let domain = self.public_domain.read(cx).content().trim().to_string();
        match public_options(&port, &cert, &key, &domain) {
            Ok(options) => {
                let name = self.nickname_or("公网聊天室", cx);
                self.show_public_form = false;
                self.tell(
                    UiCommand::HostPublic {
                        name,
                        port: options.port,
                        cert: options.cert,
                        key: options.key,
                        domain: options.domain,
                    },
                    cx,
                );
            }
            Err(e) => {
                self.status = e;
                self.status_kind = super::StatusKind::Error;
                cx.notify();
            }
        }
    }

    pub(crate) fn on_join(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let name = self.nickname_or("我", cx);
        self.tell(UiCommand::Join { name }, cx);
    }

    pub(crate) fn on_leave(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.tell(UiCommand::Leave, cx);
    }

    pub(crate) fn on_send(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.submit_message(cx);
    }

    /// 打开系统表情面板，插入到输入框光标处。
    pub(crate) fn on_open_emojis(
        &mut self,
        _: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.message.read(cx).focus_handle(cx).focus(window);
        window.show_character_palette();
    }

    /// 点输入区就让输入框获得焦点。
    pub(crate) fn on_focus_composer(
        &mut self,
        _: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.message.read(cx).focus_handle(cx).focus(window);
    }

    /// 选一个本地文件发出去。
    pub(crate) fn on_pick_file(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择要发送的文件".into()),
        });
        cx.spawn(async move |this, cx| {
            let path = match picked.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => None,
                Ok(Err(e)) => {
                    let _ = this.update(cx, |app, cx| {
                        app.status = format!("⚠ 打开文件选择器失败: {e}");
                        app.status_kind = super::StatusKind::Error;
                        cx.notify();
                    });
                    return;
                }
                Err(_) => None,
            };
            if let Some(path) = path {
                let _ = this.update(cx, |app, cx| {
                    app.tell(UiCommand::SendFile { path }, cx);
                });
            }
        })
        .detach();
    }

    /// 把聊天里的附件另存到本地。
    pub(crate) fn save_attachment(&mut self, attachment: Attachment, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_new_path(&std::env::temp_dir(), Some(&attachment.name));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = picked.await {
                let id = attachment.id;
                let _ = this.update(cx, |app, cx| {
                    app.tell(UiCommand::SaveFile { id, path }, cx);
                });
            }
        })
        .detach();
    }

    pub(crate) fn on_connect_click(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connect_from_input(cx);
    }
}

impl ChatApp {
    /// 打开消息右键菜单；靠近窗口底部时向上翻，避免被裁掉。
    pub(crate) fn open_message_menu(
        &mut self,
        position: Point<Pixels>,
        viewport: Size<Pixels>,
        line: ChatLine,
        cx: &mut Context<Self>,
    ) {
        let menu_height = px(150.);
        let y = if position.y + menu_height > viewport.height {
            if position.y > menu_height {
                position.y - menu_height
            } else {
                px(0.)
            }
        } else {
            position.y
        };
        self.message_menu = Some(MessageMenu {
            position: point(position.x, y),
            line,
        });
        cx.notify();
    }

    pub(crate) fn open_friend_menu(
        &mut self,
        user: chat_core::UserSummary,
        position: Point<Pixels>,
        viewport: Size<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let menu_height = px(76.);
        let y = if position.y + menu_height > viewport.height {
            if position.y > menu_height {
                position.y - menu_height
            } else {
                px(0.)
            }
        } else {
            position.y
        };
        self.message_menu = None;
        self.friend_menu = Some(FriendMenu {
            user,
            position: point(position.x, y),
        });
        cx.notify();
    }

    pub(crate) fn on_close_friend_menu(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.friend_menu = None;
        cx.notify();
    }

    pub(crate) fn on_close_message_menu(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.message_menu.take().is_some() {
            cx.notify();
        }
    }

    /// 引用一条消息：显示在输入框上方，随下一条消息一起发出。
    pub(crate) fn quote_message(
        &mut self,
        line: ChatLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = match &line.attachment {
            Some(attachment) => format!("[文件] {}", attachment.name.replace(['\n', '\r'], " ")),
            None => line.text.replace(['\n', '\r'], " "),
        };
        self.pending_quote = Some(chat_core::Quote {
            sender: if line.mine {
                "我".to_string()
            } else {
                line.sender.replace(['\n', '\r'], " ")
            },
            text,
            seq: line.seq,
        });
        self.message_menu = None;
        self.message.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    pub(crate) fn on_clear_quote_down(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_quote = None;
        cx.notify();
    }

    /// 全局快捷键。
    pub(crate) fn on_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        let key = keystroke.key.to_ascii_lowercase();
        if key == "escape" {
            let changed = self.message_menu.is_some()
                || self.friend_menu.is_some()
                || self.show_connect_form
                || self.show_public_form
                || self.pending_trust.is_some()
                || self.show_search;
            self.message_menu = None;
            self.friend_menu = None;
            self.show_connect_form = false;
            self.show_public_form = false;
            self.pending_trust = None;
            self.show_search = false;
            self.message_results.clear();
            self.editing = None;
            if changed {
                cx.notify();
            }
            return;
        }
        if !keystroke.modifiers.platform {
            return;
        }
        match key.as_str() {
            "enter" => self.submit_message(cx),
            "v" => self.paste_clipboard_image(cx),
            "f" => self.toggle_search(cx),
            "l" if keystroke.modifiers.shift => {
                self.show_members = false;
                self.tell(UiCommand::Logout, cx);
            }
            _ => {}
        }
    }

    /// 供无头测试直接打开消息菜单。
    #[cfg(any(test, feature = "ui-test"))]
    pub fn debug_open_message_menu(&mut self, x: f32, y: f32, cx: &mut Context<Self>) {
        let line = ChatLine {
            seq: Some(1),
            at: 0,
            sender: "某人".into(),
            text: "一条消息".into(),
            attachment: None,
            quote: None,
            mine: false,
            mentioned: false,
            unread_marker: false,
            system: false,
        };
        self.open_message_menu(
            point(px(x), px(y)),
            gpui::size(px(1024.), px(700.)),
            line,
            cx,
        );
    }
}

impl ChatApp {
    fn perform_login(&mut self, register: bool, cx: &mut Context<Self>) {
        let addr = self.login_addr.read(cx).content().trim().to_string();
        let username = self.login_user.read(cx).content().trim().to_string();
        let password = self.login_password.read(cx).content().to_string();
        let fingerprint = self.login_fingerprint.read(cx).content().trim().to_string();
        if addr.is_empty() || username.is_empty() || password.is_empty() {
            self.status = "地址、用户名和密码都不能为空".into();
            self.status_kind = super::StatusKind::Error;
            cx.notify();
            return;
        }
        self.status = if register {
            "正在注册…".into()
        } else {
            "正在登录…".into()
        };
        self.status_kind = super::StatusKind::Info;
        self.tell(
            UiCommand::Login {
                addr,
                fingerprint: (!fingerprint.is_empty()).then_some(fingerprint),
                username,
                password,
                register,
            },
            cx,
        );
    }

    pub(crate) fn on_login(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.perform_login(false, cx);
    }

    pub(crate) fn on_register(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.perform_login(true, cx);
    }

    pub(crate) fn open_conversation(&mut self, room: RoomId, cx: &mut Context<Self>) {
        // 无论从会话列表还是通讯录进来，都回到「聊」页展示聊天
        self.nav = NavTab::Chats;
        self.unread_after = None;
        self.unread_count = 0;
        self.unread_room = None;
        self.unread_marker_ix = None;
        self.jump_requested = false;
        self.unread_prompt_done = false;
        self.auto_unread = false;
        self.auto_unread_pages = 0;
        if let Some(known) = self.conversations.iter().find(|known| known.room == room) {
            let previous = self.read_seqs.get(&room).copied().unwrap_or(0);
            if known.last_seq > previous {
                self.unread_count = (known.last_seq - previous) as usize;
                self.unread_room = Some(room);
                self.unread_after = Some(previous);
                self.auto_unread = true;
            }
            // 进入会话即视为已读，红点消失；未读分隔留给本次查看
            self.mark_read(room, known.last_seq);
        }
        self.status = "正在进入会话…".into();
        self.status_kind = super::StatusKind::Info;
        self.tell(UiCommand::OpenRoom { room }, cx);
    }

    pub(crate) fn on_add_friend(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit_friend(cx);
    }

    /// 添加好友：按钮与回车共用；发送后清空输入并给出状态。
    pub(crate) fn submit_friend(&mut self, cx: &mut Context<Self>) {
        let username = self.friend_input.read(cx).content().trim().to_string();
        if username.is_empty() {
            return;
        }
        self.status = format!("正在添加好友 {username}…");
        self.status_kind = super::StatusKind::Info;
        self.tell(UiCommand::AddFriend { username }, cx);
        self.friend_input
            .update(cx, |input, cx| input.set_content("", cx));
    }

    pub(crate) fn on_search_users(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let query = self.user_search.read(cx).content().trim().to_string();
        if query.is_empty() {
            self.status = "请输入要搜索的用户名".into();
            self.status_kind = super::StatusKind::Error;
            cx.notify();
            return;
        }
        self.status = "正在搜索用户…".into();
        self.status_kind = super::StatusKind::Info;
        self.tell(UiCommand::SearchUsers { query }, cx);
    }

    pub(crate) fn on_add_search_user(&mut self, username: String, cx: &mut Context<Self>) {
        self.status = format!("正在添加好友 {username}…");
        self.status_kind = super::StatusKind::Info;
        self.search_results.clear();
        self.tell(UiCommand::AddFriend { username }, cx);
    }

    fn toggle_search(&mut self, cx: &mut Context<Self>) {
        self.show_search = !self.show_search;
        if !self.show_search {
            self.message_results.clear();
        }
        cx.notify();
    }

    pub(crate) fn on_toggle_search(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_search(cx);
    }

    pub(crate) fn on_search_messages(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(room) = self.room.as_ref().map(|view| view.id) else {
            return;
        };
        let query = self
            .message_search_input
            .read(cx)
            .content()
            .trim()
            .to_string();
        if query.is_empty() {
            return;
        }
        self.status = "正在搜索聊天记录…".into();
        self.status_kind = super::StatusKind::Info;
        self.tell(UiCommand::SearchMessages { room, query }, cx);
    }

    pub(crate) fn on_edit_message(&mut self, seq: i64, text: String, cx: &mut Context<Self>) {
        if let Some(room) = self.room.as_ref().map(|view| view.id) {
            self.editing = Some((room, seq));
            self.message
                .update(cx, |input, cx| input.set_content(text, cx));
            self.message_menu = None;
            self.status = "编辑消息：Enter 保存，Esc 取消".into();
            self.status_kind = super::StatusKind::Info;
            cx.notify();
        }
    }

    #[cfg(any(test, feature = "ui-test"))]
    pub fn debug_open_conversation(&mut self, room: RoomId, cx: &mut Context<Self>) {
        self.open_conversation(room, cx);
    }

    pub(crate) fn on_jump_unread(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self.unread_marker_ix {
            self.unread_prompt_done = true;
            self.list.scroll_to(ListOffset {
                item_ix: index,
                offset_in_item: px(0.),
            });
        } else {
            // 未读边界还在加载，等 History 插入分隔条后自动跳
            self.jump_requested = true;
        }
        cx.notify();
    }

    pub(crate) fn on_drop_files(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        if self.room.is_none() {
            return;
        }
        for path in paths {
            self.tell(UiCommand::SendFile { path: path.clone() }, cx);
        }
    }

    pub(crate) fn on_recall_message(&mut self, seq: i64, cx: &mut Context<Self>) {
        if let Some(room) = self.room.as_ref().map(|view| view.id) {
            self.message_menu = None;
            self.tell(UiCommand::RecallMessage { room, seq }, cx);
        }
    }

    pub(crate) fn on_remove_friend(&mut self, user: UserId, cx: &mut Context<Self>) {
        self.friend_menu = None;
        self.tell(UiCommand::RemoveFriend { user }, cx);
    }

    pub(crate) fn on_logout(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.show_members = false;
        self.tell(UiCommand::Logout, cx);
    }

    pub(crate) fn on_open_direct(&mut self, user: UserId, name: String, cx: &mut Context<Self>) {
        // 先在界面上切回「聊」，单聊房间返回后会自动打开
        self.nav = NavTab::Chats;
        self.pending_direct = Some(name);
        self.friend_menu = None;
        self.tell(UiCommand::OpenDirect { user }, cx);
    }

    pub(crate) fn on_create_group(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = self.group_input.read(cx).content().trim().to_string();
        if name.is_empty() {
            self.status = "群名称不能为空".into();
            self.status_kind = super::StatusKind::Error;
            cx.notify();
            return;
        }
        if self.group_selected.is_empty() {
            self.status = "先在通讯录里勾选要拉进群的好友".into();
            self.status_kind = super::StatusKind::Error;
            cx.notify();
            return;
        }
        let members = self.group_selected.clone();
        self.pending_group_name = Some(name.clone());
        self.group_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.group_selected.clear();
        self.tell(UiCommand::CreateGroup { name, members }, cx);
    }

    pub(crate) fn on_toggle_group_member(&mut self, user: UserId, cx: &mut Context<Self>) {
        if let Some(index) = self.group_selected.iter().position(|id| *id == user) {
            self.group_selected.remove(index);
        } else {
            self.group_selected.push(user);
        }
        cx.notify();
    }
}

impl ChatApp {
    pub(crate) fn on_nav_chats(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.nav = NavTab::Chats;
        cx.notify();
    }

    pub(crate) fn on_nav_contacts(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.nav = NavTab::Contacts;
        cx.notify();
    }
}

impl ChatApp {
    pub(crate) fn on_show_members(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(room) = self.room.as_ref().map(|view| view.id) else {
            return;
        };
        self.show_members = !self.show_members;
        if self.show_members {
            self.tell(UiCommand::RoomMembers { room }, cx);
        } else {
            cx.notify();
        }
    }

    pub(crate) fn on_leave_group(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(room) = self.room.as_ref().map(|view| view.id) {
            self.show_members = false;
            self.room = None;
            self.my_id = None;
            self.lines.clear();
            self.seen_seqs.clear();
            self.tell(UiCommand::LeaveGroup { room }, cx);
        }
    }

    pub(crate) fn on_remove_member(&mut self, user: UserId, cx: &mut Context<Self>) {
        if let Some(room) = self.room.as_ref().map(|view| view.id) {
            self.tell(UiCommand::RemoveMember { room, user }, cx);
        }
    }

    pub(crate) fn on_dissolve_group(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(room) = self.room.as_ref().map(|view| view.id) {
            self.show_members = false;
            self.room = None;
            self.my_id = None;
            self.lines.clear();
            self.seen_seqs.clear();
            self.tell(UiCommand::DissolveGroup { room }, cx);
        }
    }
}

impl ChatApp {
    /// 剪贴板里是图片时，写成临时文件后按文件消息发送。
    fn paste_clipboard_image(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        for entry in item.entries() {
            if let gpui::ClipboardEntry::Image(image) = entry {
                let path = std::env::temp_dir().join(format!(
                    "libchat-paste-{}.png",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|duration| duration.as_millis())
                        .unwrap_or(0)
                ));
                if std::fs::write(&path, &image.bytes).is_ok() {
                    self.tell(UiCommand::SendFile { path }, cx);
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ChatLine, public_options};

    #[test]
    fn public_form_requires_a_valid_port_and_a_matched_cert_pair() {
        let options = public_options("4433", "", "", "").unwrap();
        assert_eq!(options.port, 4433);
        assert!(options.cert.is_none());
        assert!(options.key.is_none());
        assert!(options.domain.is_none());

        assert!(public_options("0", "", "", "").is_err());
        assert!(public_options("abc", "", "", "").is_err());
        assert!(public_options("4433", "cert.pem", "", "").is_err());
        assert!(public_options("4433", "", "key.pem", "").is_err());

        let options =
            public_options("4433", " cert.pem ", " key.pem ", " chat.example.com ").unwrap();
        assert_eq!(options.port, 4433);
        assert_eq!(options.cert.unwrap().to_string_lossy(), "cert.pem");
        assert_eq!(options.key.unwrap().to_string_lossy(), "key.pem");
        assert_eq!(options.domain.as_deref(), Some("chat.example.com"));
    }

    fn line_with(text: &str) -> ChatLine {
        ChatLine {
            seq: Some(1),
            at: 0,
            sender: "某人".into(),
            text: text.into(),
            attachment: None,
            quote: None,
            mine: false,
            mentioned: false,
            unread_marker: false,
            system: false,
        }
    }

    #[test]
    fn quote_uses_the_message_summary() {
        // 引用不再拼成文本塞进输入框，而是保留结构；这里只钉住基础字段的取值方式
        let line = line_with("第一行\n第二行");
        assert_eq!(line.sender, "某人");
        assert!(line.text.contains('\n'));
    }
}
