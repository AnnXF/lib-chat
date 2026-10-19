//! 附件相关的纯函数：校验、摘要、MIME 猜测。
//!
//! 不碰文件系统，读写与传输由上层负责。

use sha2::{Digest, Sha256};

use crate::proto::{Attachment, MAX_ATTACHMENT_BYTES};
use crate::{ChatError, Result};

/// 内容摘要（十六进制小写），与证书指纹同一套写法。
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 校验附件元数据是否可用：大小、摘要格式、文件名。
pub fn validate_attachment(attachment: &Attachment) -> Result<()> {
    if attachment.size > MAX_ATTACHMENT_BYTES {
        return Err(ChatError::Protocol(format!(
            "附件超过 {} MiB 上限",
            MAX_ATTACHMENT_BYTES / 1024 / 1024
        )));
    }
    let hash = attachment.sha256.as_bytes();
    if hash.len() != 64 || !hash.iter().all(u8::is_ascii_hexdigit) {
        return Err(ChatError::Protocol("附件摘要无效".into()));
    }
    let name = attachment.name.trim();
    if name.is_empty() || name.chars().count() > 120 || name.contains(['/', '\\', '\0']) {
        return Err(ChatError::Protocol("附件文件名无效".into()));
    }
    Ok(())
}

/// 按扩展名猜 MIME；猜不出时按二进制处理。
pub fn guess_mime(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::AttachmentId;

    fn attachment(name: &str, size: u64, sha: &str) -> Attachment {
        Attachment {
            id: AttachmentId::new(),
            name: name.into(),
            size,
            mime: guess_mime(name).into(),
            sha256: sha.into(),
        }
    }

    #[test]
    fn accepts_normal_attachment() {
        let meta = attachment("报告.pdf", 1024, &sha256_hex(b"hello"));
        validate_attachment(&meta).unwrap();
        assert_eq!(meta.mime, "application/pdf");
    }

    #[test]
    fn rejects_bad_name_hash_and_size() {
        let valid_hash = sha256_hex(b"x");
        assert!(validate_attachment(&attachment("a/b.txt", 1, &valid_hash)).is_err());
        assert!(validate_attachment(&attachment("", 1, &valid_hash)).is_err());
        assert!(validate_attachment(&attachment("a.txt", 1, "not-a-hash")).is_err());
        assert!(
            validate_attachment(&attachment("a.bin", MAX_ATTACHMENT_BYTES + 1, &valid_hash))
                .is_err()
        );
    }

    #[test]
    fn unknown_extension_falls_back_to_binary() {
        assert_eq!(guess_mime("blob"), "application/octet-stream");
        assert_eq!(guess_mime("无扩展名"), "application/octet-stream");
        assert_eq!(guess_mime("photo.PNG"), "image/png");
    }
}
