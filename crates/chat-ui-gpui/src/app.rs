//! lib-chat 桌面界面：左侧发现局域网房间，右侧看人数、加入、聊天。
//!
//! 按职责拆成子模块，`main.rs` 只负责开窗：
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`model`] | 视图数据 |
//! | [`update`] | 服务事件与交互反馈 → 房间视图状态 |
//! | [`actions`] | 用户操作 → 服务指令 |
//! | [`views`] | 渲染 |
//! | [`widgets`] | 可复用控件 |
//! | [`theme`] | 配色 |
//!
//! 房间视图由 [`update`] 维护；输入控件由 [`actions`] 处理，渲染只读。

mod actions;
mod model;
pub mod theme;
mod update;
mod views;
pub mod widgets;

pub use model::{ChatLine, DiscoveredRoom, RoomView};

use std::collections::HashSet;

use chat_core::{RoomId, Seq, UserId};
use gpui::{Context, Entity, ListAlignment, ListState, Render, Window, div, prelude::*, px, rgb};

use crate::service::{self, UiCommand};

use self::widgets::text_input::{TextInput, TextInputSubmitted};

pub struct ChatApp {
    cmd_tx: flume::Sender<UiCommand>,
    nickname: Entity<TextInput>,
    addr_input: Entity<TextInput>,
    fingerprint_input: Entity<TextInput>,
    message: Entity<TextInput>,
    pending_trust: Option<(RoomId, String, String)>,
    rooms: Vec<DiscoveredRoom>,
    room: Option<RoomView>,
    lines: Vec<ChatLine>,
    seen_seqs: HashSet<Seq>,
    /// 消息列表的虚拟化状态；项数必须与 `lines` 始终一致，否则渲染会越界。
    list: ListState,
    /// 服务端那边还有更早的消息可以往前翻。
    has_older: bool,
    /// 有一页更早的历史还在路上；避免滚动回调反复发同一个请求。
    loading_older: bool,
    status: String,
    hosted: Option<String>,
    /// 本机已有一个可恢复的房间，存了多少条消息；`None` 表示还没有，或已经在用。
    local_room: Option<u64>,
    /// 本机房间的房间标识：拿它判断界面是不是正待在这个房间里，免得再摆一个入口。
    local_room_id: Option<RoomId>,
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
        let fingerprint_input = cx.new(|cx| TextInput::new("房主提供的完整 SHA-256 指纹", cx));
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

        // 滚到最顶就再往前翻一页；能不能翻、还有没有更早的，交给会话层判断
        let list = ListState::new(0, ListAlignment::Bottom, px(200.));
        let this = cx.entity().downgrade();
        list.set_scroll_handler(move |event, _window, cx| {
            if event.visible_range.start > 0 {
                return;
            }
            let _ = this.update(cx, |app, cx| app.load_older(cx));
        });

        Self {
            cmd_tx,
            nickname,
            addr_input,
            fingerprint_input,
            message,
            pending_trust: None,
            rooms: Vec::new(),
            room: None,
            lines: Vec::new(),
            seen_seqs: HashSet::new(),
            list,
            has_older: true,
            loading_older: false,
            status: "正在搜索局域网房间…".into(),
            hosted: None,
            local_room: None,
            local_room_id: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 消息列表的项数由 `ListState` 自己记着，而它只认「旧区间被新项替换」这一种说法。
    /// 这里把 [`ChatApp`] 各处用到的那几种写法钉住，免得哪天写成越界的区间。
    #[test]
    fn list_state_item_count_follows_splice() {
        let list = ListState::new(0, ListAlignment::Bottom, px(0.));
        assert_eq!(list.item_count(), 0);

        // 追加一条：空区间放在末尾
        list.splice(0..0, 1);
        assert_eq!(list.item_count(), 1);

        // 再追加两条
        list.splice(1..1, 2);
        assert_eq!(list.item_count(), 3);

        // 前插五条：空区间放在最前
        list.splice(0..0, 5);
        assert_eq!(list.item_count(), 8);

        // 清空
        list.reset(0);
        assert_eq!(list.item_count(), 0);
    }
}
