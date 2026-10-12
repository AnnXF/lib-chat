//! 诊断：把启动、退出与 panic 落到文件。
//!
//! 起因是「界面自己就没了」却查无实据。三种收场方式留下的痕迹不同，日志能区分：
//!
//! | 收场 | 日志表现 |
//! |---|---|
//! | 正常退出 | 有「界面启动」也有「界面退出」 |
//! | panic（普通 unwind） | 有「界面启动」+ panic 行，**没有**「界面退出」 |
//! | panic 无法 unwind / 信号杀死 | 只有「界面启动」 |
//!
//! 注意：只有 abort 类崩溃才会生成系统崩溃报告（`.ips`），普通 panic 不会。

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// 日志文件：`$HOME/.lib-chat/libchat-ui.log`，家目录不可用时退回临时目录。
pub fn log_path() -> Option<PathBuf> {
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(".lib-chat");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("libchat-ui.log"))
}

/// 同时写到标准错误与日志文件，界面被关掉后仍能回看现场。
///
/// 刻意不用 `eprintln!`：它在写入失败时会 panic，而本函数会在 panic 钩子里被调用，
/// 钩子里再 panic 会直接 abort，反而丢掉现场。
pub fn log_line(message: &str) {
    let _ = writeln!(std::io::stderr(), "{message}");
    let Some(path) = log_path() else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{message}");
    }
}

/// 当前时间的可读形式。
pub fn now_stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_utc(secs)
}

/// 安装 panic 钩子：把 panic 的位置与内容同时写到标准错误与日志文件。
///
/// 默认钩子只打到标准错误，界面从终端启动、或崩溃信息被滚动淹没时就会丢失。
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("未命名线程");
        log_line(&panic_line(thread, info.payload(), info.location()));
    }));
}

/// 把 panic 信息整理成一行。抽成纯函数是为了能在不写文件的前提下测试。
fn panic_line(
    thread: &str,
    payload: &(dyn std::any::Any + Send),
    location: Option<&std::panic::Location<'_>>,
) -> String {
    let location = location
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "未知位置".to_string());
    let payload = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "非字符串 panic 负载".to_string());

    format!(
        "[{}] panic [{thread}] {payload} @ {location}",
        now_stamp()
    )
}

/// 把 Unix 秒格式化成 UTC 的 `YYYY-MM-DD HH:MM:SS`。
///
/// 只为诊断服务，不值得为一行时间戳引入时间库；日期换算用 Howard Hinnant 的
/// civil_from_days 算法。
fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_epoch_as_utc() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_utc(1_000_000_000), "2001-09-09 01:46:40 UTC");
        assert_eq!(format_utc(1_791_763_200), "2026-10-12 00:00:00 UTC");
        // 闰年 2 月 29 日也要正确
        assert_eq!(format_utc(1_709_164_800), "2024-02-29 00:00:00 UTC");
    }

    #[test]
    fn formats_panic_payload_variants() {
        let payload: &(dyn std::any::Any + Send) = &"boom";
        let line = panic_line("主线程", payload, None);
        assert!(line.contains("panic [主线程] boom @ 未知位置"), "{line}");

        // 带格式参数的 panic 负载是 String，不是 &str
        let owned = "算不出来: 1".to_string();
        let payload: &(dyn std::any::Any + Send) = &owned;
        let line = panic_line("chat-backend", payload, None);
        assert!(line.contains("panic [chat-backend] 算不出来: 1"), "{line}");
    }
}
