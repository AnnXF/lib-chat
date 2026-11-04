//! 编译期写死的连接配置。
//!
//! 本地开发阶段服务端固定在 `127.0.0.1:5566`，自签身份在仓库根的 `dev/` 里，
//! 构建脚本把地址与指纹注入进来，登录页就不用再让用户填这两项。
//! `dev/room.identity` 不存在时指纹为空，界面退回手动填写。

/// 内置的服务端地址。
pub const SERVER_ADDR: &str = env!("LIBCHAT_DEV_ADDR");

/// 内置的服务端证书指纹；为空表示没有内置。
pub const SERVER_FINGERPRINT: &str = env!("LIBCHAT_DEV_FINGERPRINT");

/// 是否已经内置了完整的连接信息。
pub const LOCKED: bool = !SERVER_FINGERPRINT.is_empty();

/// 登录页要用的地址与指纹：内置了就用内置的，否则用界面上填的。
pub fn credentials(addr: String, fingerprint: String) -> (String, Option<String>) {
    if LOCKED {
        return (
            SERVER_ADDR.to_string(),
            Some(SERVER_FINGERPRINT.to_string()),
        );
    }
    (addr, (!fingerprint.is_empty()).then_some(fingerprint))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_address_is_always_a_host_and_port() {
        assert!(
            SERVER_ADDR.contains(':'),
            "内置地址应带端口，实际 {SERVER_ADDR}"
        );
    }

    #[test]
    fn locked_mode_ignores_what_the_form_says() {
        let (addr, fingerprint) = credentials("10.0.0.1:1".into(), "dead".into());
        if LOCKED {
            assert_eq!(addr, SERVER_ADDR);
            assert_eq!(fingerprint.as_deref(), Some(SERVER_FINGERPRINT));
        } else {
            assert_eq!(addr, "10.0.0.1:1");
            assert_eq!(fingerprint.as_deref(), Some("dead"));
        }
    }
}
