//! 无头 UI 布局回归：默认窗口下各区域必须完整落在窗口内。
//!
//! 需要 `--features ui-test`（启用 gpui 测试平台与元素边界查询）。

#![cfg(feature = "ui-test")]

use chat_core::{Attachment, AttachmentId, RoomId, UserId};
use chat_ui_gpui::app::ChatApp;
use chat_ui_gpui::service::UiEvent;
use gpui::{Bounds, Entity, Pixels, TestAppContext, VisualTestContext, px, size};

const WINDOW_WIDTH: f32 = 1024.;
const WINDOW_HEIGHT: f32 = 700.;
const SIDEBAR_WIDTH: f32 = 240.;

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
    assert!((f32::from(main.origin.x) - SIDEBAR_WIDTH).abs() <= 1.);
    assert!((f32::from(main.size.width) - (WINDOW_WIDTH - SIDEBAR_WIDTH)).abs() <= 1.);

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
    assert!((f32::from(main.size.width) - (WINDOW_WIDTH - SIDEBAR_WIDTH)).abs() <= 1.);

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
    assert!((f32::from(main.size.width) - (WINDOW_WIDTH - SIDEBAR_WIDTH)).abs() <= 1.);
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
    assert!((f32::from(header.size.height) - 44.).abs() <= 1.);

    let main = bounds(cx, "chat-main");
    assert!((f32::from(main.size.width) - (WINDOW_WIDTH - SIDEBAR_WIDTH)).abs() <= 1.);
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
