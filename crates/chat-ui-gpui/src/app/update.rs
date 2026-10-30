//! 房间与消息视图状态：应用服务事件和交互即时反馈。
//!
//! 消息列表的改动同时维护虚拟列表的项数。

use chat_core::{RoomId, Seq, UserId};

use crate::service::{UiCommand, UiEvent};
use gpui::ListOffset;

use chat_core::Attachment;

use super::{ChatApp, ChatLine, DiscoveredRoom, HostedRoom, RoomSource, RoomView, StatusKind};

impl ChatApp {
    pub(crate) fn mark_read(&mut self, room: RoomId, seq: Seq) {
        if seq > self.read_seqs.get(&room).copied().unwrap_or(0) {
            self.read_seqs.insert(room, seq);
            // 已读位置由服务端落库；本地只做即时更新
            let _ = self.cmd_tx.try_send(UiCommand::MarkRead { room, seq });
        }
    }

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
                self.show_members = false;
                self.members.clear();
                self.unread_after = None;
                self.unread_count = 0;
                self.unread_room = None;
                self.unread_marker_ix = None;
                self.unread_prompt_done = true;
                self.jump_requested = false;
                self.auto_unread = false;
                self.auto_unread_pages = 0;
                self.status = "已退出房间".into();
                self.push_system("已退出房间".into());
            }
            UiEvent::Message {
                room,
                seq,
                sender_id,
                sender,
                at,
                text,
                attachment,
                quote,
            } => {
                if self.showing(room).is_some() {
                    if self.window_active {
                        self.mark_read(room, seq);
                    }
                    let mentioned = self.mentions_me(&text);
                    let mut line = chat_line(
                        seq, at, sender_id, sender, text, attachment, quote, self.my_id,
                    );
                    line.mentioned = mentioned;
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
                if let Some(last) = messages.last() {
                    self.mark_read(room, last.seq);
                }
                self.has_older = has_more;
                self.loading_older = false;
                let my_id = self.my_id;
                let lines: Vec<ChatLine> = messages
                    .into_iter()
                    .map(|m| {
                        let mentioned = self.mentions_me(&m.text);
                        let mut line = chat_line(
                            m.seq,
                            m.at,
                            m.sender_id,
                            m.sender,
                            m.text,
                            m.attachment,
                            m.quote,
                            my_id,
                        );
                        line.mentioned = mentioned;
                        line
                    })
                    .collect();
                let mut marker_index = None;
                if let Some(cutoff) = self.unread_after
                    && self.unread_marker_ix.is_none()
                {
                    let boundary = lines
                        .iter()
                        .position(|line| line.seq.is_some_and(|seq| seq > cutoff));
                    if let Some(position) = boundary {
                        marker_index = Some(position);
                        self.auto_unread = false;
                    } else if self.auto_unread && self.has_older && self.auto_unread_pages < 20 {
                        // 当前页全在未读区间内：继续向前翻，直到找到已读边界
                        self.auto_unread_pages += 1;
                        self.loading_older = true;
                        if self.cmd_tx.try_send(UiCommand::LoadOlder).is_err() {
                            self.loading_older = false;
                        }
                    } else {
                        // 已经到最老的一页，第一条就是最早未读
                        marker_index = Some(0);
                        self.auto_unread = false;
                    }
                }
                // 比当前会话更早，插到最前才是正确的时间顺序
                self.prepend_lines(lines);
                // 分隔条不能走 insert_line：seq 为 None 会被追加到末尾，导致跳错位置
                if let Some(index) = marker_index {
                    self.lines.insert(index, self.unread_marker_line());
                    self.list.splice(index..index, 1);
                    self.unread_marker_ix = Some(index);
                    if self.jump_requested {
                        self.jump_requested = false;
                        self.unread_prompt_done = true;
                        self.list.scroll_to(ListOffset {
                            item_ix: index,
                            offset_in_item: gpui::px(0.),
                        });
                    }
                }
            }
            UiEvent::Missed { room, messages } => {
                if self.showing(room).is_none() {
                    return;
                }
                let my_id = self.my_id;
                let lines = messages
                    .into_iter()
                    .map(|m| {
                        let mentioned = self.mentions_me(&m.text);
                        let mut line = chat_line(
                            m.seq,
                            m.at,
                            m.sender_id,
                            m.sender,
                            m.text,
                            m.attachment,
                            m.quote,
                            my_id,
                        );
                        line.mentioned = mentioned;
                        line
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
            UiEvent::Authenticated { user, .. } => {
                self.account = Some(user);
                self.read_seqs.clear();
                self.status = "已登录".into();
            }
            UiEvent::Conversations { rooms } => {
                self.conversations = rooms;
                // 已读位置来自服务端；本地只保留即时状态
                for room in &self.conversations {
                    let current = self.read_seqs.get(&room.room).copied().unwrap_or(0);
                    if room.read_seq > current {
                        self.read_seqs.insert(room.room, room.read_seq);
                    }
                }
                // 点好友后服务端会返回新/复用的单聊，直接进去
                if let Some(name) = self.pending_direct.take()
                    && let Some(room) = self
                        .conversations
                        .iter()
                        .find(|room| room.kind == chat_core::RoomKind::Direct && room.name == name)
                        .cloned()
                {
                    self.pending_open_room = Some(room.room);
                }
                // 建群后服务端返回新群聊，直接进去
                if let Some(name) = self.pending_group_name.take()
                    && let Some(room) = self
                        .conversations
                        .iter()
                        .find(|room| room.kind == chat_core::RoomKind::Group && room.name == name)
                        .cloned()
                {
                    self.pending_open_room = Some(room.room);
                }
                // 当前正在看的会话保持已读
                if self.window_active
                    && let Some(current_room) = self.room.as_ref().map(|view| view.id)
                    && let Some(known) = self
                        .conversations
                        .iter()
                        .find(|room| room.room == current_room)
                {
                    let seq = known.last_seq;
                    self.mark_read(current_room, seq);
                }
            }
            UiEvent::Friends { users } => self.friends = users,
            UiEvent::UserSearch { users } => self.search_results = users,
            UiEvent::MessageEdited { room, message } => {
                if self.showing(room).is_some() {
                    if let Some(line) = self
                        .lines
                        .iter_mut()
                        .find(|line| line.seq == Some(message.seq))
                    {
                        line.text = message.text.clone();
                    }
                    if self
                        .editing
                        .is_some_and(|(room_id, seq)| room_id == room && seq == message.seq)
                    {
                        self.editing = None;
                    }
                    self.status = "消息已更新".into();
                }
            }
            UiEvent::MessageRecalled { room, seq } => {
                if self.showing(room).is_some() {
                    self.lines.retain(|line| line.seq != Some(seq));
                    self.seen_seqs.remove(&seq);
                    self.list.reset(self.lines.len());
                    self.status = "消息已撤回".into();
                }
            }
            UiEvent::Mention { room, from, text } => {
                let where_ = self
                    .conversations
                    .iter()
                    .find(|known| known.room == room)
                    .map(|known| known.name.clone())
                    .unwrap_or_else(|| room.to_string());
                self.status = format!("{} 在「{where_}」提到你：{text}", from.display_name);
                self.status_kind = super::StatusKind::Warn;
                notify_desktop(
                    "lib-chat",
                    &format!("{} 在「{where_}」提到你：{text}", from.display_name),
                );
            }
            UiEvent::SearchResults { room, messages } => {
                if self.showing(room).is_some() {
                    self.message_results = messages;
                    self.show_search = true;
                }
            }
            UiEvent::FriendPresence { user, online } => {
                if online {
                    self.online_friends.insert(user);
                } else {
                    self.online_friends.remove(&user);
                }
            }
            UiEvent::LoggedOut => {
                self.account = None;
                self.conversations.clear();
                self.friends.clear();
                self.search_results.clear();
                self.online_friends.clear();
                self.read_seqs.clear();
                self.room = None;
                self.my_id = None;
                self.show_members = false;
                self.members.clear();
                self.status = "已退出登录".into();
            }
            UiEvent::RoomMembers { room, members } => {
                if self.showing(room).is_some() {
                    self.members = members;
                    self.show_members = true;
                }
            }
            UiEvent::AuthRequired => {
                self.account = None;
                self.status = "请先登录".into();
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

    fn unread_marker_line(&self) -> ChatLine {
        ChatLine {
            seq: None,
            sender: String::new(),
            at: 0,
            text: format!("{} 条未读，以下是新消息", self.unread_count),
            attachment: None,
            quote: None,
            mine: false,
            mentioned: false,
            unread_marker: true,
            system: false,
        }
    }

    fn mentions_me(&self, text: &str) -> bool {
        self.account.as_ref().is_some_and(|user| {
            text.to_lowercase()
                .contains(&format!("@{}", user.username.to_lowercase()))
        })
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
            at: 0,
            sender: "系统".into(),
            text,
            attachment: None,
            quote: None,
            mine: false,
            mentioned: false,
            unread_marker: false,
            system: true,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn chat_line(
    seq: Seq,
    at: i64,
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
        at,
        text,
        attachment,
        quote,
        mine: mine == Some(sender_id),
        mentioned: false,
        unread_marker: false,
        system: false,
    }
}

/// macOS 桌面通知；没有 osascript 的平台静默失败。
fn notify_desktop(title: &str, body: &str) {
    let escape = |text: &str| text.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        escape(body),
        escape(title)
    );
    let _ = std::process::Command::new("osascript")
        .args(["-e", &script])
        .spawn();
}
