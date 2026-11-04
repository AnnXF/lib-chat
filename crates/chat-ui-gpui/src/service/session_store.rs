//! 登录态落盘：重启后不用重新输密码。
//!
//! 只存令牌与连接信息，绝不存密码；文件权限收到 0600，先写临时文件再改名，
//! 避免读到写了一半的内容。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE: &str = "session.json";
const DEVICE_FILE: &str = "device.id";
const TEMP: &str = "session.json.tmp";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct SavedLogin {
    /// 登录时用的地址，恢复时原样重连。
    pub addr: String,
    /// 自签服务端的证书指纹；受信 CA 为 `None`。
    pub fingerprint: Option<String>,
    pub username: String,
    pub display_name: String,
    pub token: String,
}

fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

pub(super) fn save(dir: &Path, login: &SavedLogin) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let temp = dir.join(TEMP);
    std::fs::write(&temp, serde_json::to_vec_pretty(login).unwrap_or_default())?;
    restrict(&temp)?;
    std::fs::rename(&temp, path(dir))
}

pub(super) fn load(dir: &Path) -> Option<SavedLogin> {
    let raw = std::fs::read(path(dir)).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// 读本机的设备标识；没有就生成一个并落盘，重启之后还是同一个。
pub(super) fn load_or_create_device(dir: &Path) -> chat_core::DeviceId {
    let path = dir.join(DEVICE_FILE);
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(device) = text.trim().parse()
    {
        return device;
    }
    let device = chat_core::DeviceId::new();
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(&path, device.to_string());
    device
}

pub(super) fn clear(dir: &Path) {
    let _ = std::fs::remove_file(path(dir));
}

#[cfg(unix)]
fn restrict(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict(_: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("libchat-session-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn login() -> SavedLogin {
        SavedLogin {
            addr: "127.0.0.1:4433".into(),
            fingerprint: Some("a".repeat(64)),
            username: "alice".into(),
            display_name: "Alice".into(),
            token: "token-1".into(),
        }
    }

    #[test]
    fn round_trips_and_clears() {
        let dir = dir("round");
        assert!(load(&dir).is_none());
        save(&dir, &login()).unwrap();
        assert_eq!(load(&dir).unwrap(), login());
        clear(&dir);
        assert!(load(&dir).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_only_readable_by_the_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = dir("perm");
        save(&dir, &login()).unwrap();
        let mode = std::fs::metadata(dir.join(FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_device_id_survives_a_restart() {
        let dir = dir("device");
        let first = load_or_create_device(&dir);
        // 再读一次相当于重启，必须是同一个
        assert_eq!(load_or_create_device(&dir), first);
        // 文件坏掉时换一个新的，仍然可用
        std::fs::write(dir.join(DEVICE_FILE), b"not-a-uuid").unwrap();
        let _ = load_or_create_device(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_broken_file_reads_as_no_session() {
        let dir = dir("broken");
        std::fs::write(dir.join(FILE), b"{ not json").unwrap();
        assert!(load(&dir).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
