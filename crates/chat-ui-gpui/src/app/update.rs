//! 后端事件 → 界面状态。
//!
//! 所有状态变更都收敛在这里，渲染函数只读不写，避免状态从多个入口被改。

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
                self.lines.clear();
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
                if let Some(view) = self.showing_mut(room) {
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

    pub(crate) fn push_system(&mut self, text: String) {
        self.lines.push(ChatLine {
            sender: "系统".into(),
            text,
            mine: false,
            system: true,
        });
    }
}
