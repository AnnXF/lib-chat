//! lib-chat 桌面界面：左侧发现局域网房间，右侧看人数、加入、聊天。
//!
//! 视觉上走极简路线：只用少量灰阶加一个强调色，靠留白和层次区分区域，
//! 分隔一律用 1px 发丝线，不用卡片和重底色。

use chat_proto::{RoomId, UserId};
use chat_ui_gpui::backend::{self, UiCommand, UiEvent};
use chat_ui_gpui::diag;
use chat_ui_gpui::text_input::{self, TextInput, TextInputSubmitted};
use chat_ui_gpui::theme;
use gpui::{
    AnyElement, App, Application, Bounds, Context, Entity, MouseButton, MouseUpEvent, Render,
    Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb, rgba, size,
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
            } => {
                self.room = Some(RoomView {
                    id: room,
                    name,
                    addr,
                    online,
                    joined: false,
                });
                self.lines.clear();
                self.my_id = None;
            }
            UiEvent::Joined { room, you, online } => {
                // 重连后服务端会重新分配身份并再发一次 Joined，措辞要区分开，
                // 否则用户会以为自己是重新进了一次房间
                let recovered = self
                    .room
                    .as_ref()
                    .is_some_and(|view| view.id == room && view.joined);
                if let Some(view) = &mut self.room
                    && view.id == room
                {
                    view.joined = true;
                    view.online = online;
                }
                self.my_id = Some(you);
                self.push_system(if recovered {
                    format!("连接已恢复，当前在线 {online} 人")
                } else {
                    format!("已加入，当前在线 {online} 人")
                });
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
            UiEvent::Presence { room, online } => {
                if let Some(view) = &mut self.room
                    && view.id == room
                {
                    view.online = online;
                }
            }
            UiEvent::Reconnecting(reason) => {
                // 只是暂时断开：保留房间与聊天记录，重连成功后会自动恢复身份
                self.status = format!("{reason}，正在重连…");
            }
            UiEvent::Disconnected => {
                self.room = None;
                self.my_id = None;
                self.status = "连接已断开，可重新加入".into();
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

    fn on_room_click(
        &mut self,
        room: RoomId,
        addr: String,
        fingerprint: String,
        cx: &mut Context<Self>,
    ) {
        // 已经是当前房间，重复点击不应触发一次「退出再重连」，那只会白白抖动人数
        if self.room.as_ref().is_some_and(|view| view.id == room) {
            return;
        }
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

// ---------------------------------------------------------------- 视觉件

/// 侧栏的小节标题。
fn section_label(text: &str) -> AnyElement {
    div()
        .px_5()
        .pt_3()
        .pb_1()
        .text_xs()
        .text_color(rgb(theme::FAINT))
        .child(text.to_string())
        .into_any_element()
}

/// 侧栏内容的统一内边距。
fn padded(child: impl IntoElement) -> AnyElement {
    div().px_5().pb_2().child(child).into_any_element()
}

fn button(
    text: &str,
    primary: bool,
    full_width: bool,
    on_click: impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let (fg, bg, hover_bg) = if primary {
        (theme::ACCENT, theme::ACCENT_SOFT, 0x22345c)
    } else {
        (theme::MUTED, theme::SURFACE, theme::HOVER)
    };

    let mut element = div()
        .flex()
        .flex_row()
        .justify_center()
        .items_center()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(rgb(bg))
        .text_sm()
        .text_color(rgb(fg))
        .cursor_pointer()
        .hover(move |style| style.bg(rgb(hover_bg)))
        .on_mouse_up(MouseButton::Left, on_click)
        .child(text.to_string());

    if full_width {
        element = element.w_full();
    }
    element.into_any_element()
}

fn render_line(line: &ChatLine) -> AnyElement {
    if line.system {
        return div()
            .w_full()
            .flex()
            .flex_row()
            .justify_center()
            .text_xs()
            .text_color(rgb(theme::FAINT))
            .child(line.text.clone())
            .into_any_element();
    }

    // 自己发的带一点强调色底并靠右，别人的只留文字，视觉噪声更低
    let body = if line.mine {
        div()
            .max_w(px(520.))
            .px_3()
            .py_2()
            .rounded_md()
            .bg(rgb(theme::ACCENT_SOFT))
            .text_sm()
            .text_color(rgb(theme::TEXT))
            .child(line.text.clone())
    } else {
        div()
            .max_w(px(520.))
            .text_sm()
            .text_color(rgb(theme::TEXT))
            .child(line.text.clone())
    };

    let (align_end, label, label_color) = if line.mine {
        (true, "我".to_string(), theme::FAINT)
    } else {
        (false, line.sender.clone(), theme::MUTED)
    };

    let label_row = if align_end {
        div().flex().flex_row().justify_end()
    } else {
        div().flex().flex_row().justify_start()
    };
    let body_row = if align_end {
        div().flex().flex_row().justify_end()
    } else {
        div().flex().flex_row().justify_start()
    };

    div()
        .w_full()
        .flex()
        .flex_col()
        .child(
            label_row
                .mb_1()
                .child(div().text_xs().text_color(rgb(label_color)).child(label)),
        )
        .child(body_row.child(body))
        .into_any_element()
}

impl ChatApp {
    fn render_header(&self) -> AnyElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(44.))
            .px_5()
            .border_b_1()
            .border_color(rgb(theme::BORDER))
            .child(
                div()
                    .text_base()
                    .text_color(rgb(theme::TEXT))
                    .child("lib-chat"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(self.status.clone()),
            )
            .into_any_element()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let items: Vec<DiscoveredRoom> = self.rooms.clone();
        let current = self.room.as_ref().map(|view| view.id);
        let joined = self.room.as_ref().map(|view| view.joined).unwrap_or(false);

        let rows: Vec<AnyElement> = items
            .into_iter()
            .enumerate()
            .map(|(index, room)| {
                let addr = room.addr.clone();
                let fingerprint = room.fingerprint.clone();
                let room_id = room.id;
                let selected = current == Some(room_id);
                // 选中态：左侧短竖条 + 低饱和强调底；已加入再加一个实心点
                let name_color = if selected { theme::ACCENT } else { theme::TEXT };

                let base = div()
                    .id(("room", index))
                    .flex()
                    .flex_row()
                    .items_center()
                    .pl_3()
                    .pr_5()
                    .py_2()
                    .cursor_pointer();
                let base = if selected {
                    base.bg(rgb(theme::ACCENT_SOFT))
                } else {
                    base
                };

                base.hover(|style| style.bg(rgb(theme::HOVER)))
                    .child(
                        div()
                            .w(px(2.))
                            .h(px(14.))
                            .mr_3()
                            .rounded_md()
                            .bg(if selected {
                                rgb(theme::ACCENT)
                            } else {
                                rgba(0x00000000)
                            }),
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
                            .text_color(if selected {
                                rgb(theme::MUTED)
                            } else {
                                rgb(theme::FAINT)
                            })
                            .child(format!(
                                "{}{} 人",
                                if selected && joined { "● " } else { "" },
                                room.online
                            )),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseUpEvent, _window, cx| {
                            this.on_room_click(room_id, addr.clone(), fingerprint.clone(), cx);
                        }),
                    )
                    .into_any_element()
            })
            .collect();

        let hint = if rows.is_empty() {
            Some(
                div()
                    .px_5()
                    .py_2()
                    .text_xs()
                    .text_color(rgb(theme::FAINT))
                    .child("未发现房间，可手动连接")
                    .into_any_element(),
            )
        } else {
            None
        };

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
                    .children(hint),
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
            .child(padded(button("创建房间", true, true, cx.listener(Self::on_host))))
            .into_any_element()
    }

    fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        let joined = self.room.as_ref().map(|room| room.joined).unwrap_or(false);

        let title = match &self.room {
            Some(room) => room.name.clone(),
            None => "未连接".to_string(),
        };
        let subtitle = match &self.room {
            Some(room) => format!("{}    在线 {} 人", room.addr, room.online),
            None => self
                .hosted
                .clone()
                .unwrap_or_else(|| "从左侧选择房间，或创建自己的房间".to_string()),
        };

        let action = if joined {
            button("退出房间", false, false, cx.listener(Self::on_leave))
        } else {
            button("加入房间", true, false, cx.listener(Self::on_join))
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
                    .px_5()
                    .py_3()
                    .border_b_1()
                    .border_color(rgb(theme::BORDER))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_base()
                                    .text_color(rgb(theme::TEXT))
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme::MUTED))
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
                    .px_5()
                    .py_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .children(lines),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_5()
                    .py_3()
                    .border_t_1()
                    .border_color(rgb(theme::BORDER))
                    .child(div().flex_1().child(self.message.clone()))
                    .child(button("发送", true, false, cx.listener(Self::on_send))),
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
            .bg(rgb(theme::BG))
            .text_color(rgb(theme::TEXT))
            .text_sm()
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
    diag::install_panic_hook();
    diag::log_line(&format!(
        "[{}] 界面启动 pid={}",
        diag::now_stamp(),
        std::process::id()
    ));

    Application::new().run(|cx: &mut App| {
        // 输入框的退格、方向键、复制粘贴依赖这些绑定，必须在开窗前注册
        text_input::bind_keys(cx);

        let bounds = Bounds::centered(None, size(px(1000.), px(680.)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(ChatApp::new),
        );
        if let Err(e) = opened {
            diag::log_line(&format!("[{}] 打开窗口失败: {e}", diag::now_stamp()));
            return;
        }
        cx.activate(true);
    });

    // 能走到这里说明是正常退出；崩溃与信号杀死都到不了这一行
    diag::log_line(&format!("[{}] 界面退出", diag::now_stamp()));
}

