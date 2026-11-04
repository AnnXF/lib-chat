//! 把本地开发用的服务端地址与证书指纹编译进客户端。
//!
//! 开发阶段服务端固定在 `127.0.0.1:5566`，自签身份放在仓库根的 `dev/` 下；
//! 这里读那份身份算出指纹，界面就不用再让用户填地址和指纹。
//! 身份文件不存在时留空，客户端退回「手动填地址与指纹」的老界面。

use std::path::Path;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// 与服务端 `room.identity` 一致：只关心证书字节。
#[derive(Deserialize)]
struct PersistedIdentity {
    certificate: Vec<u8>,
}

fn main() {
    println!("cargo:rerun-if-changed=../../dev/room.identity");
    println!("cargo:rerun-if-env-changed=LIBCHAT_DEV_ADDR");

    let addr = std::env::var("LIBCHAT_DEV_ADDR").unwrap_or_else(|_| "127.0.0.1:5566".to_string());
    println!("cargo:rustc-env=LIBCHAT_DEV_ADDR={addr}");

    let fingerprint = std::fs::read(Path::new("../../dev/room.identity"))
        .ok()
        .and_then(|raw| serde_json::from_slice::<PersistedIdentity>(&raw).ok())
        .map(|identity| {
            Sha256::digest(&identity.certificate)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        })
        .unwrap_or_default();
    println!("cargo:rustc-env=LIBCHAT_DEV_FINGERPRINT={fingerprint}");
}
