//! 用户操作 → 服务指令。
//!
//! 这里只发指令、不动界面数据：真正的状态变化由服务的 [`UiEvent`] 回来驱动，
//! 保证「界面看到的状态」只有 [`super::update`] 一个写入方。
//!
//! [`UiEvent`]: crate::service::UiEvent

use chat_proto::RoomId;
use gpui::{Context, MouseUpEvent, Window};

use crate::service::UiCommand;

use super::ChatApp;

impl ChatApp {
    /// 发一条指令并刷新界面。
    fn tell(&self, cmd: UiCommand, cx: &mut Context<Self>) {
        let _ = self.cmd_tx.send(cmd);
        cx.notify();
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
        let text = self.message.update(cx, |input, cx| input.take(cx));
        let text = text.trim().to_string();
        if !text.is_empty() {
            self.tell(UiCommand::Send { text }, cx);
        }
    }

    /// 消息区滑到最顶时再往前翻一页。
    ///
    /// 这里只喊一声：能不能翻、还有没有更早的，交给会话层判断。
    pub(crate) fn load_older(&mut self, cx: &mut Context<Self>) {
        if !self.has_older || self.loading_older {
            return;
        }
        self.loading_older = true;
        self.tell(UiCommand::LoadOlder, cx);
    }

    pub(crate) fn connect_from_input(&mut self, cx: &mut Context<Self>) {
        let addr = self.addr_input.read(cx).content().trim().to_string();
        if !addr.is_empty() {
            self.connect(addr, None, cx);
        }
    }

    pub(crate) fn connect(
        &mut self,
        addr: String,
        fingerprint: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // 换房间是一次明确的离开，旧房间的视图与记录先清干净
        self.room = None;
        self.clear_lines();
        self.has_older = true;
        self.loading_older = false;
        self.status = format!("正在连接 {addr} …");
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
            self.connect(addr, Some(fingerprint), cx);
        }
    }

    pub(crate) fn on_host(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let name = self.nickname_or("局域网聊天室", cx);
        self.tell(UiCommand::Host { name }, cx);
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

    pub(crate) fn on_connect_click(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connect_from_input(cx);
    }
}
