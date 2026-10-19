//! 附件内容存储：按内容 SHA-256 命名，天然去重。
//!
//! 消息表只存元数据，内容放在这里；同一份文件被多人发送只落一份。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chat_core::{ChatError, Result};
use tokio::sync::RwLock;

fn storage_err(e: impl std::fmt::Display) -> ChatError {
    ChatError::Storage(e.to_string())
}

/// 附件内容的读写端口。按内容摘要寻址。
#[async_trait]
pub trait BlobStore: Send + Sync {
    async fn put(&self, sha256: &str, data: &[u8]) -> Result<()>;
    async fn get(&self, sha256: &str) -> Result<Vec<u8>>;
    /// 内容是否已在库中，用于发送前校验。
    async fn contains(&self, sha256: &str) -> bool;
}

/// 进程内附件存储，测试与无落库场景使用。
#[derive(Default)]
pub struct MemoryBlobs {
    entries: RwLock<HashMap<String, Arc<Vec<u8>>>>,
}

#[async_trait]
impl BlobStore for MemoryBlobs {
    async fn put(&self, sha256: &str, data: &[u8]) -> Result<()> {
        self.entries
            .write()
            .await
            .insert(sha256.to_string(), Arc::new(data.to_vec()));
        Ok(())
    }

    async fn get(&self, sha256: &str) -> Result<Vec<u8>> {
        let entries = self.entries.read().await;
        entries
            .get(sha256)
            .map(|data| data.as_ref().clone())
            .ok_or_else(|| ChatError::Protocol("附件不存在".into()))
    }

    async fn contains(&self, sha256: &str) -> bool {
        self.entries.read().await.contains_key(sha256)
    }
}

/// 磁盘附件存储：`<dir>/<sha256>`，先写临时文件再改名，避免留下半个文件。
pub struct DiskBlobs {
    dir: PathBuf,
}

impl DiskBlobs {
    /// `dir` 是该房间专属的附件目录，调用方按房间区分。
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(storage_err)?;
        Ok(Self { dir })
    }

    fn path_of(&self, sha256: &str) -> PathBuf {
        self.dir.join(sha256)
    }
}

#[async_trait]
impl BlobStore for DiskBlobs {
    async fn put(&self, sha256: &str, data: &[u8]) -> Result<()> {
        let target = self.path_of(sha256);
        if target.exists() {
            return Ok(());
        }
        let temp = self.dir.join(format!(".{sha256}.tmp"));
        tokio::fs::write(&temp, data).await.map_err(storage_err)?;
        tokio::fs::rename(&temp, &target).await.map_err(storage_err)
    }

    async fn get(&self, sha256: &str) -> Result<Vec<u8>> {
        match tokio::fs::read(self.path_of(sha256)).await {
            Ok(data) => Ok(data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(ChatError::Protocol("附件不存在".into()))
            }
            Err(e) => Err(storage_err(e)),
        }
    }

    async fn contains(&self, sha256: &str) -> bool {
        self.path_of(sha256).exists()
    }
}

impl DiskBlobs {
    /// 数据目录，用于清理与诊断。
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disk_blobs_deduplicate_by_content() {
        let dir = std::env::temp_dir().join(format!("libchat-blobs-{}", uuid::Uuid::new_v4()));
        let store = DiskBlobs::new(&dir).unwrap();
        let sha = "ab".repeat(32);

        assert!(!store.contains(&sha).await);
        store.put(&sha, b"hello").await.unwrap();
        store.put(&sha, b"hello").await.unwrap();
        assert!(store.contains(&sha).await);

        let entries = std::fs::read_dir(store.dir()).unwrap().count();
        assert_eq!(entries, 1, "同一内容只应落一份");

        assert_eq!(store.get(&sha).await.unwrap(), b"hello");
        assert!(store.get(&"cd".repeat(32)).await.is_err());

        std::fs::remove_dir_all(dir).unwrap();
    }
}
