//! 附件内容存储：按内容 SHA-256 命名，天然去重。
//!
//! 消息表只存元数据，内容放在这里；同一份文件被多人发送只落一份。
//! 接收走 [`BlobStore::begin`]：边收边写 `.part` 文件，收完再校验摘要并改名，
//! 所以整份文件不会被读进内存；断线重来也能从 `.part` 的长度接着传。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chat_core::{BlobWriter, ChatError, Result};
use sha2::{Digest, Sha256};
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
    /// 打开一个写入端，同时告诉调用方「已经有多少字节在库里」（断点续传）。
    async fn begin(&self, sha256: &str, expected: u64) -> Result<(Box<dyn BlobWriter>, u64)>;
    /// 删除内容；不存在按已删除处理。
    async fn remove(&self, sha256: &str) -> Result<()>;
}

/// 进程内附件存储，测试与无落库场景使用。
#[derive(Default)]
pub struct MemoryBlobs {
    entries: Arc<RwLock<HashMap<String, Arc<Vec<u8>>>>>,
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

    async fn begin(&self, sha256: &str, expected: u64) -> Result<(Box<dyn BlobWriter>, u64)> {
        let complete = self.contains(sha256).await;
        let have = if complete { expected } else { 0 };
        let writer = MemoryBlobWriter {
            entries: Arc::clone(&self.entries),
            sha256: sha256.to_string(),
            expected,
            data: Vec::new(),
            complete,
        };
        Ok((Box::new(writer), have))
    }

    async fn remove(&self, sha256: &str) -> Result<()> {
        self.entries.write().await.remove(sha256);
        Ok(())
    }
}

struct MemoryBlobWriter {
    entries: Arc<RwLock<HashMap<String, Arc<Vec<u8>>>>>,
    sha256: String,
    expected: u64,
    data: Vec<u8>,
    complete: bool,
}

#[async_trait]
impl BlobWriter for MemoryBlobWriter {
    async fn write(&mut self, chunk: &[u8]) -> Result<()> {
        if self.complete {
            return Ok(());
        }
        if self.data.len() as u64 + chunk.len() as u64 > self.expected {
            return Err(ChatError::Protocol("附件超过声明的大小".into()));
        }
        self.data.extend_from_slice(chunk);
        Ok(())
    }

    async fn finish(&mut self) -> Result<()> {
        if self.complete {
            return Ok(());
        }
        if self.data.len() as u64 != self.expected {
            return Err(ChatError::Protocol("附件大小与声明不符".into()));
        }
        if chat_core::sha256_hex(&self.data) != self.sha256 {
            return Err(ChatError::Protocol("附件摘要与声明不符".into()));
        }
        self.entries.write().await.insert(
            self.sha256.clone(),
            Arc::new(std::mem::take(&mut self.data)),
        );
        self.complete = true;
        Ok(())
    }

    async fn abort(&mut self) {
        self.data.clear();
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

    fn part_of(&self, sha256: &str) -> PathBuf {
        self.dir.join(format!(".{sha256}.part"))
    }

    /// 列出已归档的内容摘要，回收孤儿附件用。
    pub fn list(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.dir).map_err(storage_err)? {
            let entry = entry.map_err(storage_err)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') || name.len() != 64 {
                continue;
            }
            names.push(name.to_string());
        }
        Ok(names)
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

    async fn begin(&self, sha256: &str, expected: u64) -> Result<(Box<dyn BlobWriter>, u64)> {
        let target = self.path_of(sha256);
        if target.exists() {
            return Ok((Box::new(Finished), expected));
        }
        let part = self.part_of(sha256);
        // 上次没传完就接着传；比声明还长说明不是同一次传输，重来
        let mut have = tokio::fs::metadata(&part)
            .await
            .map(|meta| meta.len())
            .unwrap_or(0);
        if have > expected {
            have = 0;
        }
        let file = if have == 0 {
            tokio::fs::File::create(&part).await.map_err(storage_err)?
        } else {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(&part)
                .await
                .map_err(storage_err)?
        };
        let writer = DiskBlobWriter {
            target,
            part,
            file: Some(file),
            sha256: sha256.to_string(),
            expected,
            written: have,
        };
        Ok((Box::new(writer), have))
    }

    async fn remove(&self, sha256: &str) -> Result<()> {
        let _ = tokio::fs::remove_file(self.path_of(sha256)).await;
        let _ = tokio::fs::remove_file(self.part_of(sha256)).await;
        Ok(())
    }
}

/// 内容已经在库里：什么都不用写。
struct Finished;

#[async_trait]
impl BlobWriter for Finished {
    async fn write(&mut self, _: &[u8]) -> Result<()> {
        Ok(())
    }

    async fn finish(&mut self) -> Result<()> {
        Ok(())
    }

    async fn abort(&mut self) {}
}

struct DiskBlobWriter {
    target: PathBuf,
    part: PathBuf,
    file: Option<tokio::fs::File>,
    sha256: String,
    expected: u64,
    written: u64,
}

#[async_trait]
impl BlobWriter for DiskBlobWriter {
    async fn write(&mut self, chunk: &[u8]) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let Some(file) = self.file.as_mut() else {
            return Err(ChatError::Protocol("附件写入端已关闭".into()));
        };
        if self.written + chunk.len() as u64 > self.expected {
            return Err(ChatError::Protocol("附件超过声明的大小".into()));
        }
        file.write_all(chunk).await.map_err(storage_err)?;
        // 立刻落盘：中途断掉时 `.part` 的长度就是下次续传的起点
        file.flush().await.map_err(storage_err)?;
        self.written += chunk.len() as u64;
        Ok(())
    }

    async fn finish(&mut self) -> Result<()> {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;
        if let Some(mut file) = self.file.take() {
            file.flush().await.map_err(storage_err)?;
        }
        if self.written != self.expected {
            return Err(ChatError::Protocol("附件大小与声明不符".into()));
        }
        // 流式核对摘要：续传的前半段在文件里，这里一起算
        let mut file = tokio::fs::File::open(&self.part)
            .await
            .map_err(storage_err)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).await.map_err(storage_err)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let hex: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if hex != self.sha256 {
            let _ = tokio::fs::remove_file(&self.part).await;
            return Err(ChatError::Protocol("附件摘要与声明不符".into()));
        }
        tokio::fs::rename(&self.part, &self.target)
            .await
            .map_err(storage_err)
    }

    async fn abort(&mut self) {
        self.file = None;
        let _ = tokio::fs::remove_file(&self.part).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("libchat-blob-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn disk_blobs_deduplicate_by_content() {
        let dir = temp_dir("dedup");
        let blobs = DiskBlobs::new(&dir).unwrap();
        let data = b"same".to_vec();
        let sha = chat_core::sha256_hex(&data);
        blobs.put(&sha, &data).await.unwrap();
        blobs.put(&sha, &data).await.unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        assert_eq!(blobs.get(&sha).await.unwrap(), data);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn streaming_writer_rejects_a_wrong_digest_and_cleans_up() {
        let dir = temp_dir("digest");
        let blobs = DiskBlobs::new(&dir).unwrap();
        let (mut writer, _) = blobs.begin(&"a".repeat(64), 4).await.unwrap();
        writer.write(b"data").await.unwrap();
        assert!(writer.finish().await.is_err());
        // 摘要不符时不能留下半个文件
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn an_interrupted_upload_resumes_from_where_it_stopped() {
        let dir = temp_dir("resume");
        let blobs = DiskBlobs::new(&dir).unwrap();
        let data = b"0123456789".to_vec();
        let sha = chat_core::sha256_hex(&data);

        let (mut first, _) = blobs.begin(&sha, data.len() as u64).await.unwrap();
        first.write(&data[..4]).await.unwrap();
        drop(first);

        // 第二次从第 4 字节接着写，前半段由已有的 .part 补上
        let (mut second, have) = blobs.begin(&sha, data.len() as u64).await.unwrap();
        assert_eq!(have, 4, "已传的字节数要报给调用方");
        second.write(&data[4..]).await.unwrap();
        second.finish().await.unwrap();
        assert_eq!(blobs.get(&sha).await.unwrap(), data);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn memory_store_writes_through_the_same_writer_interface() {
        let blobs = MemoryBlobs::default();
        let data = b"memory".to_vec();
        let sha = chat_core::sha256_hex(&data);
        let (mut writer, _) = blobs.begin(&sha, data.len() as u64).await.unwrap();
        writer.write(&data).await.unwrap();
        writer.finish().await.unwrap();
        assert_eq!(blobs.get(&sha).await.unwrap(), data);
    }
}
