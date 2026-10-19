use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use chat_core::{ChatError, Result, RoomId};

fn error(err: impl std::fmt::Display) -> ChatError {
    ChatError::Storage(format!("房间信任存储失败: {err}"))
}

pub fn validate_fingerprint(fingerprint: &str) -> Result<()> {
    if fingerprint.len() != 64 || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ChatError::Protocol(
            "证书指纹须为 64 位十六进制 SHA-256".into(),
        ));
    }
    Ok(())
}

pub struct TrustStore {
    path: PathBuf,
}

impl TrustStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("known-rooms.json"),
        }
    }

    fn with_locked<T>(
        &self,
        update: impl FnOnce(&mut HashMap<RoomId, String>) -> Result<(T, bool)>,
    ) -> Result<T> {
        let dir = self.path.parent().ok_or_else(|| error("缺少数据目录"))?;
        std::fs::create_dir_all(dir).map_err(error)?;
        let lock_path = dir.join("known-rooms.lock");
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .map_err(error)?;
        lock.lock().map_err(error)?;
        let mut entries: HashMap<RoomId, String> = if self.path.exists() {
            let mut bytes = Vec::new();
            File::open(&self.path)
                .map_err(error)?
                .read_to_end(&mut bytes)
                .map_err(error)?;
            serde_json::from_slice(&bytes).map_err(error)?
        } else {
            HashMap::new()
        };
        for fingerprint in entries.values() {
            validate_fingerprint(fingerprint)?;
        }
        let (value, changed) = update(&mut entries)?;
        if changed {
            let temp = dir.join(format!(".known-rooms-{}.tmp", uuid::Uuid::new_v4()));
            let result = (|| {
                let mut options = OpenOptions::new();
                options.create_new(true).write(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options.open(&temp).map_err(error)?;
                let bytes = serde_json::to_vec(&entries).map_err(error)?;
                file.write_all(&bytes).map_err(error)?;
                file.sync_all().map_err(error)?;
                std::fs::rename(&temp, &self.path).map_err(error)?;
                File::open(dir).map_err(error)?.sync_all().map_err(error)
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&temp);
            }
            result?;
        }
        Ok(value)
    }

    pub fn get(&self, room: RoomId) -> Result<Option<String>> {
        self.with_locked(|entries| Ok((entries.get(&room).cloned(), false)))
    }

    pub fn pin(&self, room: RoomId, fingerprint: &str) -> Result<()> {
        validate_fingerprint(fingerprint)?;
        let fingerprint = fingerprint.to_ascii_lowercase();
        self.with_locked(|entries| match entries.get(&room) {
            Some(existing) if existing != &fingerprint => Err(ChatError::Protocol(format!(
                "房间 {room} 的证书指纹已变更；原指纹 {existing}，新指纹 {fingerprint}。已拒绝连接"
            ))),
            Some(_) => Ok(((), false)),
            None => {
                entries.insert(room, fingerprint);
                Ok(((), true))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_survive_reopen_and_cannot_be_replaced() {
        let dir = std::env::temp_dir().join(format!("libchat-trust-{}", uuid::Uuid::new_v4()));
        let room = RoomId::new();
        let first = "a".repeat(64);
        let store = TrustStore::new(&dir);
        store.pin(room, &first).unwrap();
        store.pin(room, &first.to_ascii_uppercase()).unwrap();
        assert_eq!(
            TrustStore::new(&dir).get(room).unwrap().as_deref(),
            Some(first.as_str())
        );
        assert!(TrustStore::new(&dir).pin(room, &"b".repeat(64)).is_err());
        std::fs::write(dir.join("known-rooms.json"), b"broken").unwrap();
        assert!(store.get(room).is_err());
        assert!(store.pin(RoomId::new(), &"c".repeat(64)).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_first_pins_never_overwrite_one_another() {
        let dir = std::env::temp_dir().join(format!("libchat-trust-race-{}", uuid::Uuid::new_v4()));
        let room = RoomId::new();
        let handles: Vec<_> = ['a', 'b']
            .into_iter()
            .map(|value| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    TrustStore::new(&dir).pin(room, &value.to_string().repeat(64))
                })
            })
            .collect();
        assert_eq!(
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .filter(Result::is_ok)
                .count(),
            1
        );
        let pinned = TrustStore::new(&dir).get(room).unwrap().unwrap();
        assert!(pinned == "a".repeat(64) || pinned == "b".repeat(64));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
