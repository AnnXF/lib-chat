//! lib-chat 桌面界面入口。
//!
//! 界面逻辑在 [`chat_ui_gpui::app`]，网络逻辑在 [`chat_ui_gpui::service`]；这里只装诊断和开窗。

mod diag;

use chat_ui_gpui::app::ChatApp;
use chat_ui_gpui::app::widgets::text_input;

use gpui::{App, Application, Bounds, WindowBounds, WindowOptions, prelude::*, px, size};

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

        // 关掉窗口就把整个应用退掉：macOS 默认只是收起窗口，进程还活着，
        // 托管的房间会继续广播，本机再开一个实例也接不进去。
        cx.on_window_closed(|cx| {
            diag::log_line(&format!("[{}] 窗口已关闭，退出", diag::now_stamp()));
            cx.quit();
        })
        .detach();

        cx.activate(true);
    });
}
