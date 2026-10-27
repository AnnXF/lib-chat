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

pub use model::{ChatLine, DiscoveredRoom, HostedRoom, NavTab, RoomSource, RoomView, StatusKind};

use std::collections::HashSet;

use chat_core::{RoomId, Seq, UserId};
use gpui::{
    Context, Entity, ListAlignment, ListState, Pixels, Point, Render, Window, div, prelude::*, px,
    rgb,
};

use crate::service::{self, UiCommand};

use self::views::TestIdExt;
use self::widgets::text_input::{TextInput, TextInputSubmitted};

pub struct ChatApp {
    cmd_tx: flume::Sender<UiCommand>,
    nickname: Entity<TextInput>,
    addr_input: Entity<TextInput>,
    fingerprint_input: Entity<TextInput>,
    message: Entity<TextInput>,
    /// 登录表单。
    login_addr: Entity<TextInput>,
    login_user: Entity<TextInput>,
    login_password: Entity<TextInput>,
    login_fingerprint: Entity<TextInput>,
    /// 好友与建群输入。
    friend_input: Entity<TextInput>,
    group_input: Entity<TextInput>,
    /// 手动连接面板是否展开。
    show_connect_form: bool,
    /// 公网托管面板是否展开。
    show_public_form: bool,
    public_port: Entity<TextInput>,
    public_cert: Entity<TextInput>,
    public_key: Entity<TextInput>,
    public_domain: Entity<TextInput>,
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
    status_kind: StatusKind,
    hosted: Option<HostedRoom>,
    /// 是否展开「我的房间」的完整标识与指纹；默认收起，避免侧栏堆满长文本。
    show_identity: bool,
    /// 本机已有一个可恢复的房间，存了多少条消息；`None` 表示还没有，或已经在用。
    local_room: Option<u64>,
    /// 本机房间的房间标识：拿它判断界面是不是正待在这个房间里，免得再摆一个入口。
    local_room_id: Option<RoomId>,
    /// 我在当前房间的身份；用于判断消息是不是自己发的。
    my_id: Option<UserId>,
    /// 消息右键菜单；`None` 表示没有打开。
    message_menu: Option<MessageMenu>,
    friend_menu: Option<FriendMenu>,
    /// 点好友后等待服务端返回单聊房间，再自动打开。
    pending_direct: Option<String>,
    pending_open_room: Option<RoomId>,
    /// 输入框上方待发送的引用；发送成功或点 × 清除。
    pending_quote: Option<chat_core::Quote>,
    /// 已登录账号；`None` 时界面显示登录页。
    account: Option<chat_core::UserSummary>,
    /// 我的会话列表（群聊 + 单聊）。
    conversations: Vec<chat_core::RoomSummary>,
    friends: Vec<chat_core::UserSummary>,
    /// 微信式左侧导航：聊天 / 通讯录 / 房间。
    nav: NavTab,
    /// 会话与好友搜索。
    search: Entity<TextInput>,
}

/// 消息右键菜单的目标与位置。
#[derive(Clone)]
pub(crate) struct MessageMenu {
    pub(crate) position: Point<Pixels>,
    pub(crate) line: ChatLine,
}

/// 通讯录好友的右键菜单：发消息、复制用户名。
#[derive(Clone)]
pub(crate) struct FriendMenu {
    pub(crate) user: chat_core::UserSummary,
    pub(crate) position: Point<Pixels>,
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
        Self::with_channels(cmd_tx, events, cx)
    }

    /// 不启动后端、只构造界面。供无头 UI 测试验证布局与状态渲染。
    #[cfg(any(test, feature = "ui-test"))]
    pub fn for_test(cx: &mut Context<Self>) -> Self {
        let (cmd_tx, _cmd_rx) = flume::bounded(1);
        let (_event_tx, events) = flume::bounded(1);
        let mut app = Self::with_channels(cmd_tx, events, cx);
        // 布局测试直接看聊天界面；登录页有单独的用例
        app.account = Some(chat_core::UserSummary {
            id: UserId::new(),
            username: "tester".into(),
            display_name: "测试用户".into(),
        });
        // 旧布局测试跑「房间」页；聊天/通讯录页另有专项用例
        app.nav = NavTab::Rooms;
        app
    }

    /// 切换左侧导航页，供无头测试验证三栏布局。
    #[cfg(any(test, feature = "ui-test"))]
    pub fn debug_nav(&mut self, nav: NavTab, cx: &mut Context<Self>) {
        self.nav = nav;
        cx.notify();
    }

    /// 直接投递一条服务事件，等价于后端事件泵收到它。
    #[cfg(any(test, feature = "ui-test"))]
    pub fn debug_apply(&mut self, event: service::UiEvent) {
        self.apply(event);
    }

    /// 当前输入框内容，供无头测试断言引用写入结果。
    #[cfg(any(test, feature = "ui-test"))]
    pub fn debug_message_input(&self, cx: &gpui::App) -> String {
        self.message.read(cx).content().to_string()
    }

    /// 模拟退出登录，供登录页布局测试使用。
    #[cfg(any(test, feature = "ui-test"))]
    pub fn debug_logout(&mut self, cx: &mut Context<Self>) {
        self.account = None;
        cx.notify();
    }

    fn with_channels(
        cmd_tx: flume::Sender<UiCommand>,
        events: flume::Receiver<service::UiEvent>,
        cx: &mut Context<Self>,
    ) -> Self {
        let nickname = cx.new(|cx| {
            let mut input = TextInput::new("你的昵称", cx);
            input.set_content("我", cx);
            input
        });
        let addr_input = cx.new(|cx| TextInput::new("如 127.0.0.1:4433 或 你的域名:4433", cx));
        let fingerprint_input =
            cx.new(|cx| TextInput::new("自签房间的 64 位 SHA-256；公网留空", cx));
        let public_port = cx.new(|cx| {
            let mut input = TextInput::new("监听端口，如 4433", cx);
            input.set_content("4433", cx);
            input
        });
        let public_cert = cx.new(|cx| TextInput::new("证书链 PEM 路径（留空自签）", cx));
        let public_key = cx.new(|cx| TextInput::new("私钥 PEM 路径（留空自签）", cx));
        let public_domain = cx.new(|cx| TextInput::new("域名，如 chat.example.com（可选）", cx));
        let login_addr = cx.new(|cx| {
            let mut input = TextInput::new("服务端地址，如 127.0.0.1:4433", cx);
            input.set_content("127.0.0.1:4433", cx);
            input
        });
        let login_user = cx.new(|cx| TextInput::new("用户名（3-32 位字母数字）", cx));
        let login_password = cx.new(|cx| TextInput::new("密码（至少 8 位）", cx));
        let login_fingerprint = cx.new(|cx| TextInput::new("自签服务端指纹；受信 CA 留空", cx));
        let friend_input = cx.new(|cx| TextInput::new("好友用户名", cx));
        let group_input = cx.new(|cx| TextInput::new("群名称（拉入全部好友）", cx));
        let search = cx.new(|cx| TextInput::new("搜索会话或好友", cx));
        let message = cx.new(|cx| {
            let mut input = TextInput::new("输入消息，Enter 发送", cx);
            // 外观由输入区容器负责，控件本身不再带底色与内边距
            input.set_bare(cx);
            input
        });

        cx.subscribe(&message, |this, _, _: &TextInputSubmitted, cx| {
            this.submit_message(cx);
        })
        .detach();

        cx.subscribe(&addr_input, |this, _, _: &TextInputSubmitted, cx| {
            this.connect_from_input(cx);
        })
        .detach();

        cx.subscribe(&friend_input, |this, _, _: &TextInputSubmitted, cx| {
            this.submit_friend(cx);
        })
        .detach();

        // 后端事件泵：把网络事件搬进界面状态
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv_async().await {
                let alive = this
                    .update(cx, |state, cx| {
                        state.apply(event);
                        if let Some(room) = state.pending_open_room.take() {
                            state.open_conversation(room, cx);
                        }
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
            show_connect_form: false,
            show_public_form: false,
            public_port,
            public_cert,
            public_key,
            public_domain,
            login_addr,
            login_user,
            login_password,
            login_fingerprint,
            friend_input,
            group_input,
            pending_trust: None,
            rooms: Vec::new(),
            room: None,
            lines: Vec::new(),
            seen_seqs: HashSet::new(),
            list,
            has_older: true,
            loading_older: false,
            status: "正在搜索局域网房间…".into(),
            status_kind: StatusKind::Info,
            hosted: None,
            show_identity: false,
            local_room: None,
            local_room_id: None,
            my_id: None,
            message_menu: None,
            friend_menu: None,
            pending_direct: None,
            pending_open_room: None,
            pending_quote: None,
            account: None,
            conversations: Vec::new(),
            friends: Vec::new(),
            nav: NavTab::Chats,
            search,
        }
    }
}

impl Render for ChatApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self.render_header();
        let sidebar = self.render_sidebar(cx);
        let main = self.render_main(cx);
        let menu = self.message_menu.clone();
        let friend_menu = self.friend_menu.clone();

        div()
            .id("app-root")
            .test_id("app-root")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::BG))
            .text_color(rgb(theme::TEXT))
            .text_sm()
            // 全局快捷键：Esc 收起浮层，Cmd/Ctrl+K 手动连接，Cmd/Ctrl+Shift+P 公网服务
            .on_key_down(cx.listener(Self::on_key_down))
            .child(header)
            .child(if self.account.is_none() {
                self.render_login(cx).into_any_element()
            } else {
                div()
                    .id("app-body")
                    .test_id("app-body")
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    .child(self.render_nav(cx))
                    .child(sidebar)
                    .child(main)
                    .into_any_element()
            })
            .when_some(menu, |this, menu| {
                this.child(self.render_message_menu(menu, cx))
            })
            .when_some(friend_menu, |this, menu| {
                this.child(self.render_friend_menu(menu, cx))
            })
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
