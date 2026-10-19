//! 用户操作：提交服务指令，必要的即时视图变化交给 `update`。

use chat_core::{Attachment, RoomId};
use gpui::{Context, Focusable as _, MouseUpEvent, PathPromptOptions, Window};

use crate::service::UiCommand;

use super::ChatApp;

impl ChatApp {
    /// 发一条指令并刷新界面。
    fn tell(&mut self, cmd: UiCommand, cx: &mut Context<Self>) -> bool {
        let accepted = self.cmd_tx.try_send(cmd).is_ok();
        if !accepted {
            self.status = "后端繁忙或已关闭，请稍后重试".into();
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
        if !text.is_empty() && self.tell(UiCommand::Send { text }, cx) {
            self.message
                .update(cx, |input, cx| input.set_content("", cx));
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
        if !addr.is_empty() {
            let fingerprint = self.fingerprint_input.read(cx).content().trim().to_string();
            self.connect(addr, (!fingerprint.is_empty()).then_some(fingerprint), cx);
        }
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
    ) {
        self.begin_connect(&addr);
        self.tell(UiCommand::Connect { addr, fingerprint }, cx);
    }

    pub(crate) fn on_room_click(
        &mut self,
        room: RoomId,
        addr: String,
        fingerprint: String,
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
