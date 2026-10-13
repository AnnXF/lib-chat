//! lib-chat 桌面界面：左侧发现局域网房间，右侧看人数、加入、聊天。
//!
//! 按职责拆成子模块，`main.rs` 只负责开窗：
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`model`] | 视图数据 |
//! | [`update`] | 服务事件 → 界面状态 |
//! | [`actions`] | 用户操作 → 服务指令 |
//! | [`views`] | 渲染 |
//! | [`widgets`] | 可复用控件 |
//! | [`theme`] | 配色 |
//!
//! 状态只有一个写入方（[`update`]），渲染只读，交互只发指令。

mod actions;
mod model;
pub mod theme;
mod update;
mod views;
pub mod widgets;

pub use model::{ChatLine, DiscoveredRoom, RoomView};

use chat_proto::{RoomId, UserId};
use gpui::{Context, Entity, Render, Window, div, prelude::*, rgb};

use crate::service::{self, UiCommand};

use self::widgets::text_input::{TextInput, TextInputSubmitted};

pub struct ChatApp {
    cmd_tx: flume::Sender<UiCommand>,
    nickname: Entity<TextInput>,
    addr_input: Entity<TextInput>,
    message: Entity<TextInput>,
    rooms: Vec<DiscoveredRoom>,
    room: Option<RoomView>,
    lines: Vec<ChatLine>,
    status: String,
    hosted: Option<String>,
    /// 我在当前房间的身份；用于判断消息是不是自己发的。
    my_id: Option<UserId>,
}
impl ChatApp {
    /// 事件所属的房间，正是界面此刻展示的那个吗？
    ///
    /// 切换房间后旧连接可能还有在途事件，不属于当前房间的一律丢弃。
    fn showing(&self, room: RoomId) -> Option<&RoomView> {
        self.room.as_ref().filter(|view| view.id == room)
    }

    /// 同上，需要改状态时用。
    fn showing_mut(&mut self, room: RoomId) -> Option<&mut RoomView> {
        self.room.as_mut().filter(|view| view.id == room)
    }

    pub fn new(cx: &mut Context<Self>) -> Self {
        let (cmd_tx, events) = service::spawn();

        let nickname = cx.new(|cx| {
            let mut input = TextInput::new("你的昵称", cx);
            input.set_content("我", cx);
            input
        });
        let addr_input = cx.new(|cx| TextInput::new("如 192.168.1.5:52000", cx));
        let message = cx.new(|cx| TextInput::new("输入消息，回车发送", cx));

        cx.subscribe(&message, |this, _, _: &TextInputSubmitted, cx| {
            this.submit_message(cx);
        })
        .detach();

        cx.subscribe(&addr_input, |this, _, _: &TextInputSubmitted, cx| {
            this.connect_from_input(cx);
        })
        .detach();

        // 后端事件泵：把网络事件搬进界面状态
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv_async().await {
                let alive = this
                    .update(cx, |state, cx| {
                        state.apply(event);
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();

        Self {
            cmd_tx,
            nickname,
            addr_input,
            message,
            rooms: Vec::new(),
            room: None,
            lines: Vec::new(),
            status: "正在搜索局域网房间…".into(),
            hosted: None,
            my_id: None,
        }
    }
}

impl Render for ChatApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self.render_header();
        let sidebar = self.render_sidebar(cx);
        let main = self.render_main(cx);

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::BG))
            .text_color(rgb(theme::TEXT))
            .text_sm()
            .child(header)
            .child(div().flex().flex_row().flex_1().child(sidebar).child(main))
    }
}
