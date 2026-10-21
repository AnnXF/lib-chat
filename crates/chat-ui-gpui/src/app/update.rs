//! 房间与消息视图状态：应用服务事件和交互即时反馈。
//!
//! 消息列表的改动同时维护虚拟列表的项数。

use chat_core::{RoomId, Seq, UserId};
use gpui::ListOffset;

use crate::service::UiEvent;

use chat_core::Attachment;

use super::{ChatApp, ChatLine, DiscoveredRoom, HostedRoom, RoomSource, RoomView, StatusKind};

impl ChatApp {
    pub(crate) fn begin_loading_older(&mut self) -> bool {
        if !self.has_older
            || self.loading_older
            || !self.room.as_ref().is_some_and(|view| view.joined)
            || self.lines.iter().all(|line| line.seq.is_none())
        {
            return false;
        }
        self.loading_older = true;
        true
    }

    pub(crate) fn begin_connect(&mut self, addr: &str) {
        self.status = format!("正在连接 {addr} …");
        self.status_kind = StatusKind::Info;
    }

    pub(crate) fn apply(&mut self, event: UiEvent) {
        // 先说语气，再改文案：错误红、重连橙、其余灰
        self.status_kind = match &event {
            UiEvent::Error(_) => StatusKind::Error,
            UiEvent::Reconnecting(_) => StatusKind::Warn,
            _ => StatusKind::Info,
        };
        match event {
            UiEvent::Status(text) => self.status = text,
            UiEvent::TrustRequired {
                room,
                addr,
                fingerprint,
            } => {
                self.status = "首次连接前，请向房主核对完整房间标识和证书指纹".into();
                self.pending_trust = Some((room, addr, fingerprint));
            }
            UiEvent::Error(text) => {
                self.status = format!("⚠ {text}");
                self.loading_older = false;
            }
            UiEvent::RoomFound {
                room,
                name,
                addr,
                fingerprint,
                online,
            } => self.observe_room(
                room,
                name,
                addr,
                Some(fingerprint),
                online,
                RoomSource::Local,
            ),
            UiEvent::RemoteRoom {
                room,
                name,
                addr,
                online,
            } => self.observe_room(room, name, addr, None, online, RoomSource::Remote),
            UiEvent::Hosting {
                room,
                name,
                addrs,
                fingerprint,
                public,
            } => {
                self.status = match &public {
                    Some(public) => match &public.domain {
                        Some(domain) => format!("公网服务已启动：{domain}:{}", public.port),
                        None => format!(
                            "公网服务已启动：监听 0.0.0.0:{}，客户端填公网域名:{}",
                            public.port, public.port
                        ),
                    },
                    None => "房间已启动，等待其他客户端连接".into(),
                };
                self.hosted = Some(HostedRoom {
                    name,
                    room,
                    addrs,
                    fingerprint,
                    public,
                });
                // 换了房间就收起到摘要，别把上一间的详情状态带过来
                self.show_identity = false;
                // 已经在用这个房间了，侧栏那个「回到我的房间」入口就该收起来
                self.local_room = None;
                self.local_room_id = None;
            }
            UiEvent::LocalRoom { room, messages } => {
                self.local_room = Some(messages);
                self.local_room_id = Some(room);
            }
            UiEvent::Lobby {
                room,
                name,
                addr,
                online,
            } => {
                self.pending_trust = None;
                let same_room = self.showing(room).is_some();
                let joined = same_room && self.room.as_ref().is_some_and(|view| view.joined);
                self.room = Some(RoomView {
                    id: room,
                    name,
                    addr,
                    online,
                    joined,
                });
                self.sync_online(room, online);
                if !same_room {
                    self.clear_lines();
                    self.has_older = true;
                    self.loading_older = false;
                    self.my_id = None;
                }
                self.status = if joined {
                    "已重新连接房间"
                } else {
                    "已连接，可加入房间"
                }
                .into();
            }
            UiEvent::Joined { room, you, online } => {
                if self.showing(room).is_none() {
                    return;
                }
                // 重连后服务端会重新分配身份并再发一次 Joined，措辞要区分开，
                // 否则用户会以为自己是重新进了一次房间
                let recovered = self.showing(room).is_some_and(|view| view.joined);
                if let Some(view) = self.showing_mut(room) {
                    view.joined = true;
                    view.online = online;
                }
                self.sync_online(room, online);
                self.my_id = Some(you);
                self.status = if recovered {
                    format!("连接已恢复，当前在线 {online} 人")
                } else {
                    format!("已加入，当前在线 {online} 人")
                };
                self.push_system(self.status.clone());
            }
            UiEvent::Left { room } => {
                let Some(view) = self.showing_mut(room) else {
                    return;
                };
                view.joined = false;
                self.my_id = None;
                self.status = "已退出房间".into();
                self.push_system("已退出房间".into());
            }
            UiEvent::Message {
                room,
                seq,
                sender_id,
                sender,
                text,
                attachment,
                quote,
            } => {
                if self.showing(room).is_some() {
                    let line =
                        chat_line(seq, sender_id, sender, text, attachment, quote, self.my_id);
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
                    .map(|m| {
                        chat_line(
                            m.seq,
                            m.sender_id,
                            m.sender,
                            m.text,
                            m.attachment,
                            m.quote,
                            my_id,
                        )
                    })
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
                    .map(|m| {
                        chat_line(
                            m.seq,
                            m.sender_id,
                            m.sender,
                            m.text,
                            m.attachment,
                            m.quote,
                            my_id,
                        )
                    })
                    .collect();
                self.push_lines(lines);
            }
            UiEvent::HistoryCancelled { room } => {
                if self.showing(room).is_some() {
                    self.loading_older = false;
                }
            }
            UiEvent::Presence { room, online } => {
                if let Some(view) = self.showing_mut(room) {
                    view.online = online;
                }
                self.sync_online(room, online);
            }
            UiEvent::RoomGone { room } => {
                let before = self.rooms.len();
                self.rooms.retain(|known| known.id != room);
                if self.rooms.len() != before {
                    self.report_room_count();
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

    /// 发现或刷新一个房间：同一房间只保留一行。
    ///
    /// 自己就在这个房间里时，人数以房间推送为准：探测是几秒一轮的，
    /// 早于推送发出的那次探测结果回来，只会把数字倒回去。
    fn observe_room(
        &mut self,
        room: RoomId,
        name: String,
        addr: String,
        fingerprint: Option<String>,
        online: u32,
        source: RoomSource,
    ) {
        let online = match self
            .room
            .as_ref()
            .filter(|view| view.id == room && view.joined)
        {
            Some(view) => view.online,
            None => online,
        };
        match self.rooms.iter_mut().find(|known| known.id == room) {
            Some(known) => {
                known.name = name;
                known.addr = addr;
                // 公网条目没有指纹，不要把已有的自签指纹冲掉
                if fingerprint.is_some() {
                    known.fingerprint = fingerprint;
                }
                known.online = online;
                known.source = source;
            }
            None => {
                self.rooms.push(DiscoveredRoom {
                    id: room,
                    name,
                    addr,
                    fingerprint,
                    online,
                    source,
                });
                self.rooms.sort_by(|a, b| a.name.cmp(&b.name));
                // 只在确实多了一个房间时说话。复查是在刷新同一个房间，
                // 每次都刷状态栏的话，连接失败之类的提示两秒就没了。
                self.report_room_count();
            }
        }
    }

    /// 把「发现了几个房间」写进状态栏。
    ///
    /// 只在房间数真的变了时调用：复查是每几秒一轮的，逢轮就写会把错误提示盖掉。
    fn report_room_count(&mut self) {
        self.status = format!("发现 {} 个房间", self.rooms.len());
    }

    /// 把实时人数同步到侧栏那一行。
    ///
    /// 侧栏平时只能靠几秒一轮的探测更新人数，正在聊的这个房间明明有实时推送，
    /// 不同步的话就会出现「房间里写着 2 人、侧栏还停在 1 人」。
    fn sync_online(&mut self, room: RoomId, online: u32) {
        if let Some(known) = self.rooms.iter_mut().find(|known| known.id == room) {
            known.online = online;
        }
    }

    fn insert_line(&mut self, line: ChatLine, history: bool) -> Option<usize> {
        let at = if let Some(seq) = line.seq {
            if !self.seen_seqs.insert(seq) {
                return None;
            }
            match (
                self.lines.iter().find_map(|line| line.seq),
                self.lines.iter().rposition(|line| line.seq.is_some()),
            ) {
                (Some(first), _) if seq < first => 0,
                (_, Some(last)) if seq > self.lines[last].seq.unwrap() => {
                    if history {
                        last + 1
                    } else {
                        self.lines.len()
                    }
                }
                (Some(_), _) => self
                    .lines
                    .iter()
                    .position(|line| line.seq.is_some_and(|old| old > seq))
                    .unwrap(),
                _ if history => 0,
                _ => self.lines.len(),
            }
        } else {
            self.lines.len()
        };
        self.lines.insert(at, line);
        self.list.splice(at..at, 1);
        Some(at)
    }

    pub(crate) fn push_line(&mut self, line: ChatLine) {
        self.insert_line(line, false);
    }

    pub(crate) fn push_lines(&mut self, lines: Vec<ChatLine>) {
        for line in lines {
            self.insert_line(line, false);
        }
    }

    pub(crate) fn prepend_lines(&mut self, lines: Vec<ChatLine>) {
        if lines.is_empty() {
            return;
        }
        let anchor = self.list.logical_scroll_top();
        let mut item_ix = anchor.item_ix;
        for line in lines {
            if let Some(at) = self.insert_line(line, true)
                && at <= item_ix
            {
                item_ix += 1;
            }
        }
        if item_ix != anchor.item_ix {
            self.list.scroll_to(ListOffset {
                item_ix,
                offset_in_item: anchor.offset_in_item,
            });
        }
    }

    pub(crate) fn clear_lines(&mut self) {
        self.lines.clear();
        self.seen_seqs.clear();
        self.list.reset(0);
    }

    pub(crate) fn push_system(&mut self, text: String) {
        self.push_line(ChatLine {
            seq: None,
            sender: "系统".into(),
            text,
            attachment: None,
            quote: None,
            mine: false,
            system: true,
        });
    }
}

fn chat_line(
    seq: Seq,
    sender_id: UserId,
    sender: String,
    text: String,
    attachment: Option<Attachment>,
    quote: Option<chat_core::Quote>,
    mine: Option<UserId>,
) -> ChatLine {
    ChatLine {
        seq: Some(seq),
        sender,
        text,
        attachment,
        quote,
        mine: mine == Some(sender_id),
        system: false,
    }
}
