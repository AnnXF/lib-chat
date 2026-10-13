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
        cx.activate(true);
    });

    // 能走到这里说明是正常退出；崩溃与信号杀死都到不了这一行
    diag::log_line(&format!("[{}] 界面退出", diag::now_stamp()));
}
