//! 后端事件 → 界面状态。
//!
//! 所有状态变更都收敛在这里，渲染函数只读不写。消息列表只能经 [`ChatApp`] 上那几个
//! `*_lines` 方法改动——它们同时维护虚拟列表的项数，绕过它们就会越界。

use chat_proto::UserId;
use gpui::ListOffset;

use crate::service::UiEvent;

use super::{ChatApp, ChatLine, DiscoveredRoom, RoomView};

impl ChatApp {
    pub(crate) fn apply(&mut self, event: UiEvent) {
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
                // 指纹是十六进制，取前 8 位足够辨认；不足 8 位就整段显示
                let short = fingerprint.get(..8).unwrap_or(&fingerprint);
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
                self.clear_lines();
                // 新房间，能不能往上翻要等它的第一页历史说了算
                self.has_older = true;
                self.my_id = None;
            }
            UiEvent::Joined { room, you, online } => {
                // 重连后服务端会重新分配身份并再发一次 Joined，措辞要区分开，
                // 否则用户会以为自己是重新进了一次房间
                let recovered = self.showing(room).is_some_and(|view| view.joined);
                if let Some(view) = self.showing_mut(room) {
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
                if let Some(view) = self.showing_mut(room) {
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
                if self.showing(room).is_some() {
                    let line = chat_line(sender_id, sender, text, self.my_id);
                    self.push_line(line);
                }
            }
            UiEvent::History {
                room,
                messages,
                has_more,
            } => {
                if self.showing(room).is_none() {
                    return;
                }
                self.has_older = has_more;
                self.loading_older = false;
                let my_id = self.my_id;
                let lines = messages
                    .into_iter()
                    .map(|m| chat_line(m.sender_id, m.sender, m.text, my_id))
                    .collect();
                // 比当前会话更早，插到最前才是正确的时间顺序
                self.prepend_lines(lines);
            }
            UiEvent::Missed { room, messages } => {
                if self.showing(room).is_none() {
                    return;
                }
                let my_id = self.my_id;
                let lines = messages
                    .into_iter()
                    .map(|m| chat_line(m.sender_id, m.sender, m.text, my_id))
                    .collect();
                self.push_lines(lines);
            }
            UiEvent::Presence { room, online } => {
                if let Some(view) = self.showing_mut(room) {
                    view.online = online;
                }
            }
            UiEvent::RoomGone { room } => {
                self.rooms.retain(|known| known.id != room);
                self.status = format!("发现 {} 个局域网房间", self.rooms.len());
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

    /// 追加一条消息。
    pub(crate) fn push_line(&mut self, line: ChatLine) {
        let at = self.lines.len();
        self.lines.push(line);
        self.list.splice(at..at, 1);
    }

    /// 追加一批消息（重连补齐的漏收消息）。
    pub(crate) fn push_lines(&mut self, lines: Vec<ChatLine>) {
        if lines.is_empty() {
            return;
        }
        let at = self.lines.len();
        let added = lines.len();
        self.lines.extend(lines);
        self.list.splice(at..at, added);
    }

    /// 在列表最前插入一批消息（加入时的历史、往上翻的更早历史）。
    pub(crate) fn prepend_lines(&mut self, lines: Vec<ChatLine>) {
        if lines.is_empty() {
            return;
        }
        // 前插会把已有内容整体下移，先记下此刻看到的首项，插入后按新下标摆回去
        let anchor = self.list.logical_scroll_top();
        let added = lines.len();
        self.lines.splice(0..0, lines);
        self.list.splice(0..0, added);
        self.list.scroll_to(ListOffset {
            item_ix: anchor.item_ix + added,
            offset_in_item: anchor.offset_in_item,
        });
    }

    /// 清空消息列表（切换房间）。
    pub(crate) fn clear_lines(&mut self) {
        self.lines.clear();
        self.list.reset(0);
    }

    pub(crate) fn push_system(&mut self, text: String) {
        self.push_line(ChatLine {
            sender: "系统".into(),
            text,
            mine: false,
            system: true,
        });
    }
}

/// 把一条聊天消息转成界面行，顺带判断是不是自己发的。
fn chat_line(sender_id: UserId, sender: String, text: String, mine: Option<UserId>) -> ChatLine {
    ChatLine {
        sender,
        text,
        mine: mine == Some(sender_id),
        system: false,
    }
}
