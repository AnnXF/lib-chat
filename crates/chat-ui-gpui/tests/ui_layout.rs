//! 无头 UI 布局回归：默认窗口下各区域必须完整落在窗口内。
//!
//! 需要 `--features ui-test`（启用 gpui 测试平台与元素边界查询）。

#![cfg(feature = "ui-test")]

use chat_core::{Attachment, AttachmentId, RoomId, UserId};
use chat_ui_gpui::app::ChatApp;
use chat_ui_gpui::app::NavTab;
use chat_ui_gpui::service::{PublicHosting, UiEvent};
use gpui::{Bounds, Entity, Pixels, TestAppContext, VisualTestContext, px, size};

const WINDOW_WIDTH: f32 = 1024.;
const WINDOW_HEIGHT: f32 = 700.;
const NAV_WIDTH: f32 = 56.;
const SIDEBAR_WIDTH: f32 = 256.;

fn open_window(cx: &mut TestAppContext) -> (Entity<ChatApp>, &mut VisualTestContext) {
    open_window_sized(cx, WINDOW_WIDTH, WINDOW_HEIGHT)
}

fn open_window_sized(
    cx: &mut TestAppContext,
    width: f32,
    height: f32,
) -> (Entity<ChatApp>, &mut VisualTestContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| ChatApp::for_test(cx));
    cx.simulate_resize(size(px(width), px(height)));
    cx.run_until_parked();
    (view.clone(), cx)
}

fn apply(view: &Entity<ChatApp>, cx: &mut VisualTestContext, event: UiEvent) {
    cx.update(|_window, cx| {
        view.update(cx, |app, cx| {
            app.debug_apply(event);
            cx.notify();
        });
    });
    cx.run_until_parked();
}

fn bounds(cx: &mut VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
    cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("节点「{selector}」没有渲染"))
}

fn bottom(bounds: &Bounds<Pixels>) -> f32 {
    f32::from(bounds.origin.y) + f32::from(bounds.size.height)
}

fn right(bounds: &Bounds<Pixels>) -> f32 {
    f32::from(bounds.origin.x) + f32::from(bounds.size.width)
}

#[track_caller]
fn assert_inside(bounds: &Bounds<Pixels>, selector: &str) {
    assert!(
        f32::from(bounds.origin.x) >= -0.5 && f32::from(bounds.origin.y) >= -0.5,
        "{selector} 起点在窗口外: {:?}",
        bounds.origin
    );
    assert!(
        right(bounds) <= WINDOW_WIDTH + 0.5,
        "{selector} 右边超出窗口: {} > {WINDOW_WIDTH}",
        right(bounds)
    );
    assert!(
        bottom(bounds) <= WINDOW_HEIGHT + 0.5,
        "{selector} 底部超出窗口: {} > {WINDOW_HEIGHT}",
        bottom(bounds)
    );
}

/// 默认页面：侧栏、聊天区、输入条全部完整可见。
#[test]
fn default_layout_fills_the_window() {
    let mut app_cx = TestAppContext::single();
    let (_view, cx) = open_window(&mut app_cx);

    let root = bounds(cx, "app-root");
    assert_inside(&root, "app-root");
    assert!((f32::from(root.size.width) - WINDOW_WIDTH).abs() <= 1.);
    assert!((f32::from(root.size.height) - WINDOW_HEIGHT).abs() <= 1.);

    let sidebar = bounds(cx, "sidebar");
    assert_inside(&sidebar, "sidebar");
    assert!((f32::from(sidebar.size.width) - SIDEBAR_WIDTH).abs() <= 1.);

    let sidebar_footer = bounds(cx, "sidebar-footer");
    assert_inside(&sidebar_footer, "sidebar-footer");
    assert!(f32::from(sidebar_footer.size.height) > 0.);
    assert!(
        (bottom(&sidebar_footer) - bottom(&sidebar)).abs() <= 1.,
        "底部连接区应贴住侧栏底边"
    );

    let main = bounds(cx, "chat-main");
    assert_inside(&main, "chat-main");
    assert!((f32::from(main.origin.x) - (NAV_WIDTH + SIDEBAR_WIDTH)).abs() <= 1.);
    assert!((f32::from(main.size.width) - (WINDOW_WIDTH - NAV_WIDTH - SIDEBAR_WIDTH)).abs() <= 1.);

    let composer = bounds(cx, "composer");
    assert_inside(&composer, "composer");
    assert!(f32::from(composer.size.height) > 0.);
    assert!(
        (bottom(&composer) - bottom(&main)).abs() <= 1.,
        "输入条应贴住聊天区底边"
    );

    // 结构：工具条在上，输入区在下并留出足够高度
    let tools = bounds(cx, "composer-tools");
    assert_inside(&tools, "composer-tools");
    assert!(
        f32::from(tools.origin.y) - f32::from(composer.origin.y) <= 16.,
        "工具条应贴在输入条顶部"
    );

    let input = bounds(cx, "composer-input");
    assert_inside(&input, "composer-input");
    assert!(
        f32::from(input.size.height) >= 96.,
        "输入区高度不足: {}",
        f32::from(input.size.height)
    );
    assert!(
        f32::from(input.origin.y) >= bottom(&tools) - 0.5,
        "输入区应位于工具条下方"
    );
}

/// 房主身份、信任指纹等长文本不能把侧栏撑宽，也不能把底部按钮挤出窗口。
#[test]
fn long_identity_text_keeps_the_layout_inside_the_window() {
    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window(&mut app_cx);
    let room = RoomId::new();

    apply(
        &view,
        cx,
        UiEvent::Hosting {
            room,
            name: "一个名字特别长的局域网聊天房间".into(),
            addrs: vec![
                "192.168.100.200:52000".into(),
                "10.0.0.2:52000".into(),
                "172.16.31.4:52000".into(),
            ],
            fingerprint: "a1b2c3d4".repeat(8),
            public: None,
        },
    );
    apply(
        &view,
        cx,
        UiEvent::TrustRequired {
            room,
            addr: "192.168.100.200:52000".into(),
            fingerprint: "d4c3b2a1".repeat(8),
        },
    );
    apply(
        &view,
        cx,
        UiEvent::LocalRoom {
            room,
            messages: 123_456,
        },
    );

    let sidebar = bounds(cx, "sidebar");
    assert!((f32::from(sidebar.size.width) - SIDEBAR_WIDTH).abs() <= 1.);
    assert_inside(&sidebar, "sidebar");

    // 默认只留摘要：完整标识与 64 位指纹折行会把侧栏堆满
    let collapsed = bounds(cx, "hosted-room");
    assert!(
        f32::from(collapsed.size.height) < 80.,
        "房间身份摘要过高: {}",
        f32::from(collapsed.size.height)
    );
    assert!(
        cx.debug_bounds("identity-details").is_none(),
        "完整身份默认应收起"
    );

    // 交互层由 actions 单测覆盖；这里只验证展开后的布局
    cx.update(|_window, cx| {
        view.update(cx, |app, cx| app.debug_toggle_identity(cx));
    });
    cx.run_until_parked();

    assert!(
        cx.debug_bounds("identity-details").is_some(),
        "点「详情」后应展开完整身份"
    );
    // 展开后的内容在侧栏主体里滚动，允许超出视口；但不能挤走底部操作区
    let expanded = bounds(cx, "hosted-room");
    assert!(
        f32::from(expanded.size.height) > f32::from(collapsed.size.height),
        "展开后的身份卡片应更高"
    );
    let body = bounds(cx, "sidebar-body");
    assert_inside(&body, "sidebar-body");

    let footer = bounds(cx, "sidebar-footer");
    assert_inside(&footer, "sidebar-footer");
    assert!(f32::from(footer.size.height) > 0.);

    let main = bounds(cx, "chat-main");
    assert_inside(&main, "chat-main");
    assert!((f32::from(main.size.width) - (WINDOW_WIDTH - NAV_WIDTH - SIDEBAR_WIDTH)).abs() <= 1.);

    let composer = bounds(cx, "composer");
    assert_inside(&composer, "composer");
}

/// 长房间名、长地址与长昵称不能把标题栏或输入条挤出窗口。
#[test]
fn long_room_name_keeps_the_composer_visible() {
    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window(&mut app_cx);
    let room = RoomId::new();
    let long_name = "很长的房间名称".repeat(20);
    let long_addr = "192.168.100.200:52000";

    apply(
        &view,
        cx,
        UiEvent::RoomFound {
            room,
            name: long_name.clone(),
            addr: long_addr.into(),
            fingerprint: "ef".repeat(32),
            online: 42,
        },
    );
    apply(
        &view,
        cx,
        UiEvent::Lobby {
            room,
            name: long_name,
            addr: long_addr.into(),
            online: 42,
        },
    );
    apply(
        &view,
        cx,
        UiEvent::Joined {
            room,
            you: UserId::new(),
            online: 42,
        },
    );

    let header = bounds(cx, "chat-header");
    assert_inside(&header, "chat-header");

    let main = bounds(cx, "chat-main");
    assert!((f32::from(main.size.width) - (WINDOW_WIDTH - NAV_WIDTH - SIDEBAR_WIDTH)).abs() <= 1.);
    assert_inside(&main, "chat-main");

    let composer = bounds(cx, "composer");
    assert_inside(&composer, "composer");
    assert!(f32::from(composer.size.height) > 0.);
}

/// 窗口变矮时，侧栏主体收缩滚动，底部操作区与输入条仍必须可达。
#[test]
fn short_window_keeps_controls_reachable() {
    const HEIGHT: f32 = 420.;

    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window_sized(&mut app_cx, WINDOW_WIDTH, HEIGHT);
    let room = RoomId::new();
    apply(
        &view,
        cx,
        UiEvent::Hosting {
            room,
            name: "矮窗口房间".into(),
            addrs: vec!["192.168.100.200:52000".into()],
            fingerprint: "a1b2c3d4".repeat(8),
            public: None,
        },
    );
    apply(
        &view,
        cx,
        UiEvent::TrustRequired {
            room,
            addr: "192.168.100.200:52000".into(),
            fingerprint: "d4c3b2a1".repeat(8),
        },
    );

    let footer = bounds(cx, "sidebar-footer");
    assert!(
        bottom(&footer) <= HEIGHT + 0.5,
        "底部操作区被挤出窗口: {} > {HEIGHT}",
        bottom(&footer)
    );
    assert!(f32::from(footer.size.height) > 0.);

    let body = bounds(cx, "sidebar-body");
    let sidebar = bounds(cx, "sidebar");
    assert!(
        f32::from(body.size.height) < f32::from(sidebar.size.height),
        "内容区应与底部操作区共享侧栏高度"
    );
    assert!(f32::from(body.size.height) > 0.);

    let composer = bounds(cx, "composer");
    assert!(
        bottom(&composer) <= HEIGHT + 0.5,
        "输入条被挤出窗口: {} > {HEIGHT}",
        bottom(&composer)
    );
}

/// 顶栏状态是长错误信息时也不能撑宽窗口或挤压主体。
#[test]
fn long_status_text_cannot_widen_the_window() {
    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window(&mut app_cx);

    apply(
        &view,
        cx,
        UiEvent::Error(
            "连接失败：证书指纹不匹配，请向房主重新核对房间标识与完整 SHA-256 指纹".repeat(3),
        ),
    );

    let root = bounds(cx, "app-root");
    assert!(
        (f32::from(root.size.width) - WINDOW_WIDTH).abs() <= 1.,
        "顶栏长文本把窗口撑宽了: {}",
        f32::from(root.size.width)
    );

    let header = bounds(cx, "app-header");
    assert_inside(&header, "app-header");
    assert!((f32::from(header.size.height) - 48.).abs() <= 1.);

    let main = bounds(cx, "chat-main");
    assert!((f32::from(main.size.width) - (WINDOW_WIDTH - NAV_WIDTH - SIDEBAR_WIDTH)).abs() <= 1.);
}

/// 文件消息卡片不能把聊天气泡区撑出窗口，输入条仍要可见。
#[test]
fn file_message_renders_inside_the_window() {
    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window(&mut app_cx);
    let room = RoomId::new();

    apply(
        &view,
        cx,
        UiEvent::Lobby {
            room,
            name: "文件房间".into(),
            addr: "192.168.1.5:52000".into(),
            online: 1,
        },
    );
    apply(
        &view,
        cx,
        UiEvent::Joined {
            room,
            you: UserId::new(),
            online: 1,
        },
    );
    apply(
        &view,
        cx,
        UiEvent::Message {
            room,
            seq: 1,
            sender_id: UserId::new(),
            sender: "Bob".into(),
            text: String::new(),
            attachment: Some(Attachment {
                id: AttachmentId::new(),
                name: "一个名字很长很长很长的报告文件.pdf".into(),
                size: 3 * 1024 * 1024,
                mime: "application/pdf".into(),
                sha256: "ab".repeat(32),
            }),
            quote: None,
        },
    );

    let card = bounds(cx, "file-card");
    assert_inside(&card, "file-card");
    assert!(
        f32::from(card.size.width) <= WINDOW_WIDTH - SIDEBAR_WIDTH,
        "文件卡片宽度超出聊天区"
    );

    let composer = bounds(cx, "composer");
    assert_inside(&composer, "composer");
    assert!(
        bottom(&card) <= f32::from(composer.origin.y) + 0.5,
        "文件卡片压到了输入条"
    );
}

/// 公网地址簿：长域名与长房间名不能撑宽侧栏，也不能把底部操作区挤走。
#[test]
fn remote_server_rows_stay_inside_the_sidebar() {
    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window(&mut app_cx);
    let room = RoomId::new();

    apply(
        &view,
        cx,
        UiEvent::RemoteRoom {
            room,
            name: "一个名字特别长的公网聊天房间".repeat(3),
            addr: "a-very-long-subdomain.chat.example.com:4433".into(),
            online: 12,
        },
    );

    let rows = bounds(cx, "remote-rooms");
    assert_inside(&rows, "remote-rooms");
    assert!(
        f32::from(rows.size.width) <= SIDEBAR_WIDTH + 0.5,
        "远程服务器列表把侧栏撑宽了: {}",
        f32::from(rows.size.width)
    );

    let sidebar = bounds(cx, "sidebar");
    assert!((f32::from(sidebar.size.width) - SIDEBAR_WIDTH).abs() <= 1.);

    let footer = bounds(cx, "sidebar-footer");
    assert_inside(&footer, "sidebar-footer");
    assert!((bottom(&footer) - bottom(&sidebar)).abs() <= 1.);

    let composer = bounds(cx, "composer");
    assert_inside(&composer, "composer");
}

/// 矮窗口 + 远程服务器列表：主体滚动，底部操作区仍完整可见。
#[test]
fn short_window_with_remote_servers_keeps_the_footer_visible() {
    const HEIGHT: f32 = 420.;

    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window_sized(&mut app_cx, WINDOW_WIDTH, HEIGHT);
    for index in 0..6 {
        apply(
            &view,
            cx,
            UiEvent::RemoteRoom {
                room: RoomId::new(),
                name: format!("远程房间 {index}"),
                addr: format!("server-{index}.example.com:4433"),
                online: index,
            },
        );
    }

    let footer = bounds(cx, "sidebar-footer");
    assert!(
        bottom(&footer) <= HEIGHT + 0.5,
        "底部操作区被挤出窗口: {} > {HEIGHT}",
        bottom(&footer)
    );
    let body = bounds(cx, "sidebar-body");
    assert_inside(&body, "sidebar-body");
    assert!(f32::from(body.size.height) > 0.);
}

/// 公网托管表单：展开后仍在侧栏内，底部连接区不被挤走。
#[test]
fn public_form_fits_inside_the_sidebar() {
    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window(&mut app_cx);
    cx.update(|_window, cx| {
        view.update(cx, |app, cx| app.debug_toggle_public_form(cx));
    });
    cx.run_until_parked();

    let form = bounds(cx, "public-form");
    assert_inside(&form, "public-form");
    assert!(
        f32::from(form.size.width) <= SIDEBAR_WIDTH + 0.5,
        "公网表单把侧栏撑宽了: {}",
        f32::from(form.size.width)
    );

    let sidebar = bounds(cx, "sidebar");
    assert!((f32::from(sidebar.size.width) - SIDEBAR_WIDTH).abs() <= 1.);

    let footer = bounds(cx, "sidebar-footer");
    assert_inside(&footer, "sidebar-footer");
    assert!((bottom(&footer) - bottom(&sidebar)).abs() <= 1.);

    let main = bounds(cx, "chat-main");
    assert_inside(&main, "chat-main");
}

/// 矮窗口下展开公网表单：主体滚动，底部操作区仍完整可见。
#[test]
fn short_window_with_public_form_keeps_the_footer_visible() {
    const HEIGHT: f32 = 420.;

    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window_sized(&mut app_cx, WINDOW_WIDTH, HEIGHT);
    cx.update(|_window, cx| {
        view.update(cx, |app, cx| app.debug_toggle_public_form(cx));
    });
    cx.run_until_parked();

    let footer = bounds(cx, "sidebar-footer");
    assert!(
        bottom(&footer) <= HEIGHT + 0.5,
        "底部操作区被挤出窗口: {} > {HEIGHT}",
        bottom(&footer)
    );
    let body = bounds(cx, "sidebar-body");
    assert_inside(&body, "sidebar-body");
    assert!(f32::from(body.size.height) > 0.);
}

/// 公网房间详情：接入地址与证书指纹都能一键复制，按钮都在侧栏内。
#[test]
fn public_identity_offers_copy_buttons_inside_the_sidebar() {
    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window(&mut app_cx);
    let room = RoomId::new();

    apply(
        &view,
        cx,
        UiEvent::Hosting {
            room,
            name: "公网房间".into(),
            addrs: vec!["127.0.0.1:4433".into()],
            fingerprint: "ab".repeat(32),
            public: Some(PublicHosting {
                port: 4433,
                domain: Some("chat.example.com".into()),
                trusted_ca: true,
            }),
        },
    );
    cx.update(|_window, cx| {
        view.update(cx, |app, cx| app.debug_toggle_identity(cx));
    });
    cx.run_until_parked();

    for selector in [
        "copy-fingerprint",
        "copy-room",
        "copy-listen",
        "copy-join-addr",
        "copy-fingerprint-detail",
    ] {
        let button = bounds(cx, selector);
        assert_inside(&button, selector);
        assert!(f32::from(button.size.width) > 0., "{selector} 应当可点击");
    }

    let sidebar = bounds(cx, "sidebar");
    assert!((f32::from(sidebar.size.width) - SIDEBAR_WIDTH).abs() <= 1.);

    let footer = bounds(cx, "sidebar-footer");
    assert_inside(&footer, "sidebar-footer");
}

/// 手动连接面板：展开后位于侧栏主体内，底部操作区仍贴底。
#[test]
fn connect_form_fits_inside_the_sidebar() {
    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window(&mut app_cx);
    cx.update(|_window, cx| {
        view.update(cx, |app, cx| app.debug_toggle_connect_form(cx));
    });
    cx.run_until_parked();

    let form = bounds(cx, "connect-form");
    assert_inside(&form, "connect-form");
    assert!(
        f32::from(form.size.width) <= SIDEBAR_WIDTH + 0.5,
        "连接面板把侧栏撑宽了: {}",
        f32::from(form.size.width)
    );

    let footer = bounds(cx, "sidebar-footer");
    assert_inside(&footer, "sidebar-footer");
    let sidebar = bounds(cx, "sidebar");
    assert!((bottom(&footer) - bottom(&sidebar)).abs() <= 1.);

    let main = bounds(cx, "chat-main");
    assert_inside(&main, "chat-main");
}

/// 矮窗口 + 手动连接面板：主体滚动，底部操作区仍完整可见。
#[test]
fn short_window_with_connect_form_keeps_the_footer_visible() {
    const HEIGHT: f32 = 420.;

    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window_sized(&mut app_cx, WINDOW_WIDTH, HEIGHT);
    cx.update(|_window, cx| {
        view.update(cx, |app, cx| app.debug_toggle_connect_form(cx));
    });
    cx.run_until_parked();

    let footer = bounds(cx, "sidebar-footer");
    assert!(
        bottom(&footer) <= HEIGHT + 0.5,
        "底部操作区被挤出窗口: {} > {HEIGHT}",
        bottom(&footer)
    );
    let body = bounds(cx, "sidebar-body");
    assert_inside(&body, "sidebar-body");
    assert!(f32::from(body.size.height) > 0.);
}

/// 消息右键菜单：靠近窗口底部时向上翻，完整落在窗口内。
#[test]
fn message_menu_stays_inside_the_window() {
    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window(&mut app_cx);
    cx.update(|_window, cx| {
        view.update(cx, |app, cx| app.debug_open_message_menu(320., 620., cx));
    });
    cx.run_until_parked();

    let menu = bounds(cx, "message-menu");
    assert_inside(&menu, "message-menu");
    assert!(
        f32::from(menu.size.height) >= 64.,
        "菜单至少应容纳两个基础项: {}",
        f32::from(menu.size.height)
    );
}

/// 右键菜单在窗口上方时贴着点击位置出现，不反向跳。
#[test]
fn message_menu_opens_near_the_pointer() {
    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window(&mut app_cx);
    cx.update(|_window, cx| {
        view.update(cx, |app, cx| app.debug_open_message_menu(200., 100., cx));
    });
    cx.run_until_parked();

    let menu = bounds(cx, "message-menu");
    assert!((f32::from(menu.origin.x) - 200.).abs() <= 1.);
    assert!((f32::from(menu.origin.y) - 100.).abs() <= 1.);
}

/// 引用消息后输入框必须是单行：GPUI 的单行控件遇到换行会直接 panic。
#[test]
fn quoting_a_message_shows_a_quote_bar() {
    use gpui::{Modifiers, point};

    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window(&mut app_cx);
    cx.update(|_window, cx| {
        view.update(cx, |app, cx| app.debug_open_message_menu(200., 100., cx));
    });
    cx.run_until_parked();

    // 菜单内边距 4px、每项 32px；第二项是「引用」
    cx.simulate_click(
        point(px(260.), px(100. + 4. + 32. + 16.)),
        Modifiers::default(),
    );
    cx.run_until_parked();

    let bar = bounds(cx, "quote-bar");
    assert_inside(&bar, "quote-bar");
    let content = cx.update(|_window, cx| view.read(cx).debug_message_input(cx));
    assert!(content.is_empty(), "引用不应再往输入框里塞文本: {content}");

    let clear = bounds(cx, "quote-clear");
    let x = f32::from(clear.origin.x) + f32::from(clear.size.width) / 2.;
    let y = f32::from(clear.origin.y) + f32::from(clear.size.height) / 2.;
    cx.simulate_click(point(px(x), px(y)), Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("quote-bar").is_none(),
        "点 × 后引用条应消失"
    );
}

/// 带引用的消息在气泡里显示引用块，正文仍在，且不压输入条。
#[test]
fn quoted_message_renders_a_quote_block() {
    use chat_core::Quote;

    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window(&mut app_cx);
    let room = RoomId::new();

    apply(
        &view,
        cx,
        UiEvent::Lobby {
            room,
            name: "引用房间".into(),
            addr: "192.168.1.5:52000".into(),
            online: 1,
        },
    );
    apply(
        &view,
        cx,
        UiEvent::Joined {
            room,
            you: UserId::new(),
            online: 1,
        },
    );
    apply(
        &view,
        cx,
        UiEvent::Message {
            room,
            seq: 1,
            sender_id: UserId::new(),
            sender: "Bob".into(),
            text: "这是回复".into(),
            attachment: None,
            quote: Some(Quote {
                sender: "Alice".into(),
                text: "这是被引用的原文".into(),
                seq: Some(0),
            }),
        },
    );

    let block = bounds(cx, "message-quote");
    assert_inside(&block, "message-quote");
    let composer = bounds(cx, "composer");
    assert!(
        bottom(&block) <= f32::from(composer.origin.y) + 0.5,
        "引用块压到了输入条"
    );
}

/// 未登录时整页是登录表单，不渲染聊天主界面。
#[test]
fn logged_out_shows_the_login_page() {
    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window(&mut app_cx);
    cx.update(|_window, cx| {
        view.update(cx, |app, cx| app.debug_logout(cx));
    });
    cx.run_until_parked();

    let page = bounds(cx, "login-page");
    assert_inside(&page, "login-page");
    assert!(f32::from(page.size.height) > 200., "登录页应占满主体高度");
}

/// 微信式三栏：左侧导航 + 列表 + 聊天；通讯录页展示好友与群聊。
#[test]
fn wechat_nav_switches_between_chats_and_contacts() {
    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window(&mut app_cx);

    cx.update(|_window, cx| {
        view.update(cx, |app, cx| app.debug_nav(NavTab::Chats, cx));
    });
    cx.run_until_parked();
    assert_inside(&bounds(cx, "nav"), "nav");
    assert_inside(&bounds(cx, "sidebar"), "sidebar");
    assert_inside(&bounds(cx, "conversations"), "conversations");
    assert_inside(&bounds(cx, "chat-main"), "chat-main");

    cx.update(|_window, cx| {
        view.update(cx, |app, cx| app.debug_nav(NavTab::Contacts, cx));
    });
    cx.run_until_parked();
    assert_inside(&bounds(cx, "contacts-body"), "contacts-body");
    assert_inside(&bounds(cx, "groups"), "groups");
    assert_inside(&bounds(cx, "friends"), "friends");
}

/// 群成员面板在窗口内，成员与角色可见。
#[test]
fn group_members_panel_fits_inside_the_window() {
    let mut app_cx = TestAppContext::single();
    let (view, cx) = open_window(&mut app_cx);
    let room = chat_core::RoomId::new();
    let me = chat_core::UserSummary {
        id: chat_core::UserId::new(),
        username: "me".into(),
        display_name: "我".into(),
    };
    let other = chat_core::UserSummary {
        id: chat_core::UserId::new(),
        username: "bob".into(),
        display_name: "Bob".into(),
    };
    cx.update(|_window, cx| {
        view.update(cx, |app, cx| {
            app.debug_apply(UiEvent::Conversations {
                rooms: vec![chat_core::RoomSummary {
                    room,
                    kind: chat_core::RoomKind::Group,
                    name: "测试群".into(),
                    member_count: 2,
                    last_seq: 0,
                    last_text: None,
                    last_at: None,
                }],
            });
            app.debug_apply(UiEvent::Lobby {
                room,
                name: "测试群".into(),
                addr: "127.0.0.1:1".into(),
                online: 2,
            });
            app.debug_apply(UiEvent::RoomMembers {
                room,
                members: vec![
                    chat_core::RoomMember {
                        user: me,
                        role: chat_core::MemberRole::Owner,
                    },
                    chat_core::RoomMember {
                        user: other,
                        role: chat_core::MemberRole::Member,
                    },
                ],
            });
            cx.notify();
        });
    });
    cx.run_until_parked();
    let panel = bounds(cx, "members-panel");
    assert_inside(&panel, "members-panel");
    assert!(f32::from(panel.size.height) > 20.);
}
