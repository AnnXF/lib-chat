//! lib-chat 桌面界面：左侧发现局域网房间，右侧看人数、加入、聊天。

use chat_ui_gpui::backend::{self, UiCommand, UiEvent};
use chat_ui_gpui::text_input::{TextInput, TextInputSubmitted};
use chat_proto::{RoomId, UserId};
use gpui::{
    AnyElement, App, Application, Bounds, Context, Entity, MouseButton, MouseUpEvent, Render,
    Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};

#[derive(Clone)]
struct DiscoveredRoom {
    id: RoomId,
    name: String,
    addr: String,
    fingerprint: String,
    online: u32,
}

struct RoomView {
    id: RoomId,
    name: String,
    addr: String,
    online: u32,
    members: u32,
    joined: bool,
}

struct ChatLine {
    sender: String,
    text: String,
    /// 是否由自己发出，决定气泡靠哪一边。
    mine: bool,
    system: bool,
}

struct ChatApp {
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
    fn new(cx: &mut Context<Self>) -> Self {
        let (cmd_tx, events) = backend::spawn();

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

    // ------------------------------------------------------------ 状态

    fn apply(&mut self, event: UiEvent) {
        match event {
            UiEvent::Status(text) => self.status = text,
            UiEvent::Error(text) => self.status = format!("⚠ {text}"),
            UiEvent::RoomFound {
                room,
                name,
                addr,
                fingerprint,
                online,
                ..
            } => {
                match self.rooms.iter_mut().find(|known| known.id == room) {
                    Some(known) => {
                        known.name = name;
                        known.addr = addr;
                        known.fingerprint = fingerprint;
                        known.online = online;
                    }
                    None => {
                        self.rooms.push(DiscoveredRoom {
                            id: room,
                            name,
                            addr,
                            fingerprint,
                            online,
                        });
                        self.rooms.sort_by(|a, b| a.name.cmp(&b.name));
                    }
                }
                self.status = format!("发现 {} 个局域网房间", self.rooms.len());
            }
            UiEvent::Hosting {
                name,
                addrs,
                fingerprint,
            } => {
                let short = &fingerprint[..8.min(fingerprint.len())];
                self.hosted = Some(format!(
                    "已创建「{name}」，地址 {}（指纹 {short}…）",
                    addrs.join("  /  ")
                ));
                self.status = "房间已启动，等待其他客户端连接".into();
            }
            UiEvent::Lobby {
                room,
                name,
                addr,
                online,
                members,
            } => {
                self.room = Some(RoomView {
                    id: room,
                    name,
                    addr,
                    online,
                    members,
                    joined: false,
                });
                self.lines.clear();
                self.my_id = None;
            }
            UiEvent::Joined { room, you, members } => {
                if let Some(view) = &mut self.room
                    && view.id == room
                {
                    view.joined = true;
                    view.members = members as u32;
                }
                self.my_id = Some(you);
                self.push_system(format!("已加入，当前成员 {members} 人"));
            }
            UiEvent::Left { room } => {
                if let Some(view) = &mut self.room
                    && view.id == room
                {
                    view.joined = false;
                }
                self.my_id = None;
                self.push_system("已退出房间".into());
            }
            UiEvent::Message {
                room,
                sender_id,
                sender,
                text,
            } => {
                // 切换房间后旧连接可能还有在途消息，不属于当前房间的直接丢弃
                if self.room.as_ref().is_some_and(|view| view.id == room) {
                    let mine = self.my_id == Some(sender_id);
                    self.lines.push(ChatLine {
                        sender,
                        text,
                        mine,
                        system: false,
                    });
                }
            }
            UiEvent::Presence {
                room,
                online,
                members,
            } => {
                if let Some(view) = &mut self.room
                    && view.id == room
                {
                    view.online = online;
                    view.members = members;
                }
            }
            UiEvent::Disconnected => {
                self.room = None;
                self.my_id = None;
                self.status = "连接已断开".into();
            }
        }
    }

    fn push_system(&mut self, text: String) {
        self.lines.push(ChatLine {
            sender: "系统".into(),
            text,
            mine: false,
            system: true,
        });
    }

    // ------------------------------------------------------------ 交互

    fn submit_message(&mut self, cx: &mut Context<Self>) {
        let text = self.message.update(cx, |input, cx| input.take(cx));
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let _ = self.cmd_tx.send(UiCommand::Send { text });
        cx.notify();
    }

    fn connect_from_input(&mut self, cx: &mut Context<Self>) {
        let addr = self.addr_input.read(cx).content().trim().to_string();
        if addr.is_empty() {
            return;
        }
        self.connect(addr, None, cx);
    }

    fn connect(&mut self, addr: String, fingerprint: Option<String>, cx: &mut Context<Self>) {
        self.room = None;
        self.lines.clear();
        self.status = format!("正在连接 {addr} …");
        let _ = self.cmd_tx.send(UiCommand::Connect { addr, fingerprint });
        cx.notify();
    }

    fn on_room_click(&mut self, addr: String, fingerprint: String, cx: &mut Context<Self>) {
        self.connect(addr, Some(fingerprint), cx);
    }

    fn on_host(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let name = self.nickname.read(cx).content().trim().to_string();
        let name = if name.is_empty() {
            "局域网聊天室".to_string()
        } else {
            name
        };
        let _ = self.cmd_tx.send(UiCommand::Host { name });
        cx.notify();
    }

    fn on_join(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let name = self.nickname.read(cx).content().trim().to_string();
        let name = if name.is_empty() {
            "我".to_string()
        } else {
            name
        };
        let _ = self.cmd_tx.send(UiCommand::Join { name });
        cx.notify();
    }

    fn on_leave(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let _ = self.cmd_tx.send(UiCommand::Leave);
        cx.notify();
    }

    fn on_send(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.submit_message(cx);
    }

    fn on_connect_click(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.connect_from_input(cx);
    }
}

// ---------------------------------------------------------------- 渲染

fn label(text: &str) -> AnyElement {
    div()
        .text_sm()
        .text_color(rgb(0x6d7f96))
        .child(text.to_string())
        .into_any_element()
}

fn button(
    text: &str,
    primary: bool,
    on_click: impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let (base, hover) = if primary {
        (rgb(0x2f6feb), rgb(0x3d81ff))
    } else {
        (rgb(0x26303f), rgb(0x334054))
    };
    div()
        .px_3()
        .py_1()
        .rounded_md()
        .bg(base)
        .text_sm()
        .text_color(rgb(0xf2f6fc))
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .on_mouse_up(MouseButton::Left, on_click)
        .child(text.to_string())
        .into_any_element()
}

fn render_line(line: &ChatLine) -> AnyElement {
    if line.system {
        return div()
            .w_full()
            .flex()
            .flex_row()
            .justify_center()
            .text_sm()
            .text_color(rgb(0x6d7f96))
            .child(line.text.clone())
            .into_any_element();
    }

    // 自己发的靠右、蓝底；别人发的靠左、灰底，避免多窗口时张冠李戴
    let (side, sender_label, sender_color, bubble_bg) = if line.mine {
        (
            true,
            "我".to_string(),
            rgb(0x9fc2ff),
            rgb(0x2f6feb),
        )
    } else {
        (
            false,
            line.sender.clone(),
            rgb(0x6fb1ff),
            rgb(0x232d3d),
        )
    };

    let bubble = div()
        .max_w(px(460.))
        .px_3()
        .py_2()
        .rounded_md()
        .bg(bubble_bg)
        .child(line.text.clone());

    let label = div()
        .text_sm()
        .text_color(sender_color)
        .child(sender_label);

    let label_row = if side {
        div().flex().flex_row().justify_end().child(label)
    } else {
        div().flex().flex_row().justify_start().child(label)
    };
    let bubble_row = if side {
        div().flex().flex_row().justify_end().child(bubble)
    } else {
        div().flex().flex_row().justify_start().child(bubble)
    };
    let column = div().flex().flex_col().child(label_row).child(bubble_row);

    if side {
        div()
            .w_full()
            .flex()
            .flex_row()
            .justify_end()
            .child(column)
            .into_any_element()
    } else {
        div()
            .w_full()
            .flex()
            .flex_row()
            .justify_start()
            .child(column)
            .into_any_element()
    }
}

impl ChatApp {
    fn render_header(&self) -> AnyElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_4()
            .py_2()
            .bg(rgb(0x161d29))
            .border_b_1()
            .border_color(rgb(0x232c3a))
            .child(div().text_lg().child("lib-chat · 局域网聊天"))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0x8fa3bf))
                    .child(self.status.clone()),
            )
            .into_any_element()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let items: Vec<DiscoveredRoom> = self.rooms.clone();
        let rooms: Vec<AnyElement> = items
            .into_iter()
            .enumerate()
            .map(|(index, room)| {
                let addr = room.addr.clone();
                let fingerprint = room.fingerprint.clone();
                div()
                    .id(("room", index))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .mb_1()
                    .rounded_md()
                    .bg(rgb(0x1a2130))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(0x243044)))
                    .child(div().text_sm().child(room.name.clone()))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x6fb1ff))
                            .child(format!("在线 {}", room.online)),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseUpEvent, _window, cx| {
                            this.on_room_click(addr.clone(), fingerprint.clone(), cx);
                        }),
                    )
                    .into_any_element()
            })
            .collect();

        let empty_hint = if rooms.is_empty() {
            Some(label("未发现房间，可手动连接或创建"))
        } else {
            None
        };

        div()
            .w(px(300.))
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .bg(rgb(0x121821))
            .border_r_1()
            .border_color(rgb(0x232c3a))
            .child(label("局域网房间"))
            .child(
                div()
                    .id("rooms")
                    .flex()
                    .flex_col()
                    .max_h(px(220.))
                    .overflow_y_scroll()
                    .children(rooms)
                    .children(empty_hint),
            )
            .child(label("手动连接"))
            .child(self.addr_input.clone())
            .child(button("连接", false, cx.listener(Self::on_connect_click)))
            .child(label("我的昵称"))
            .child(self.nickname.clone())
            .child(button("创建房间", true, cx.listener(Self::on_host)))
            .into_any_element()
    }

    fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        let joined = self.room.as_ref().map(|room| room.joined).unwrap_or(false);

        let title = match &self.room {
            Some(room) => format!("「{}」", room.name),
            None => "未连接".to_string(),
        };
        let subtitle = match &self.room {
            Some(room) => format!(
                "{}    在线 {} / 成员 {}",
                room.addr, room.online, room.members
            ),
            None => self
                .hosted
                .clone()
                .unwrap_or_else(|| "从左侧选择房间，或创建自己的房间".to_string()),
        };

        let action = if joined {
            button("退出房间", false, cx.listener(Self::on_leave))
        } else {
            button("加入房间", true, cx.listener(Self::on_join))
        };

        let lines: Vec<AnyElement> = self.lines.iter().map(render_line).collect();

        div()
            .flex_1()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .py_3()
                    .bg(rgb(0x161d29))
                    .border_b_1()
                    .border_color(rgb(0x232c3a))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(div().text_lg().child(title))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0x8fa3bf))
                                    .child(subtitle),
                            ),
                    )
                    .child(action),
            )
            .child(
                div()
                    .id("messages")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(lines),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .p_3()
                    .bg(rgb(0x121821))
                    .border_t_1()
                    .border_color(rgb(0x232c3a))
                    .child(div().flex_1().child(self.message.clone()))
                    .child(button("发送", true, cx.listener(Self::on_send))),
            )
            .into_any_element()
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
            .bg(rgb(0x0f1319))
            .text_color(rgb(0xe8edf5))
            .child(header)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .child(sidebar)
                    .child(main),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1020.), px(700.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(ChatApp::new),
        )
        .unwrap();
        cx.activate(true);
    });
}
