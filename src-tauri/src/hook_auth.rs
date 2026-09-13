//! 本次运行的 hook 鉴权令牌。
//!
//! 令牌在进程内随机生成一次（每次启动都不同），通过环境变量注入到 PTY 子进程，
//! 再由 hook bridge 脚本回填进 payload。监听器只接受携带匹配令牌的请求。
//!
//! 两条不可妥协的性质：
//! 1. fail closed：缺失 / 为空 / 不匹配 / 令牌本身生成失败，一律拒绝，
//!    绝不退化成“无法校验所以全部放行”。
//! 2. 常量时间比较：回环上攻击者可以无限重试，逐字节提前返回会形成计时侧信道。
//!
//! 令牌绝不写入任何日志。

use std::sync::OnceLock;

/// 32 字节随机数的十六进制表示（64 个字符）。
/// `None` 表示本次运行没有可用令牌，此时所有校验都会失败。
static RUN_TOKEN: OnceLock<Option<String>> = OnceLock::new();

/// 注入到 PTY 子进程的环境变量名，hook bridge 从这里读取令牌。
pub const HOOK_TOKEN_ENV: &str = "CODE_BAR_HOOK_TOKEN";

/// payload 中承载令牌的字段名。
pub const HOOK_TOKEN_FIELD: &str = "code_bar_hook_token";

fn generate() -> Option<String> {
    use std::fmt::Write as _;

    let mut buf = [0u8; 32];
    // 取自操作系统 CSPRNG。失败时返回 None——此后所有校验都会拒绝。
    getrandom::fill(&mut buf).ok()?;

    let mut out = String::with_capacity(buf.len() * 2);
    for byte in buf {
        let _ = write!(out, "{byte:02x}");
    }
    Some(out)
}

/// 返回本次运行的令牌；生成失败时返回 `None`。
pub fn run_token() -> Option<&'static str> {
    RUN_TOKEN.get_or_init(generate).as_deref()
}

/// 常量时间比较：始终遍历完整长度，不因首个不同字节提前返回。
/// 长度本身不是秘密（固定 64 个十六进制字符），因此长度不等可以直接返回。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 校验 payload 携带的令牌。
/// 缺失、为空、长度不符、不匹配，以及本次运行没有可用令牌时，一律返回 false。
pub fn verify(candidate: Option<&str>) -> bool {
    let Some(expected) = run_token() else {
        return false;
    };
    let Some(candidate) = candidate else {
        return false;
    };
    if candidate.is_empty() {
        return false;
    }
    constant_time_eq(candidate.as_bytes(), expected.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> &'static str {
        run_token().expect("本次运行应当能生成令牌")
    }

    #[test]
    fn token_is_64_hex_chars() {
        let token = token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn token_is_stable_within_one_run() {
        assert_eq!(run_token(), run_token());
    }

    #[test]
    fn rejects_absent_token() {
        assert!(!verify(None));
    }

    #[test]
    fn rejects_empty_token() {
        assert!(!verify(Some("")));
    }

    #[test]
    fn rejects_wrong_token_of_same_length() {
        let mut wrong = token().to_string();
        let last = wrong.pop().expect("非空");
        wrong.push(if last == 'a' { 'b' } else { 'a' });
        assert_eq!(wrong.len(), 64);
        assert!(!verify(Some(&wrong)));
    }

    #[test]
    fn rejects_truncated_token() {
        let token = token();
        assert!(!verify(Some(&token[..token.len() - 1])));
    }

    #[test]
    fn rejects_token_with_correct_prefix() {
        let longer = format!("{}0", token());
        assert!(!verify(Some(&longer)));
    }

    #[test]
    fn rejects_all_zero_token() {
        assert!(!verify(Some(&"0".repeat(64))));
    }

    #[test]
    fn accepts_exact_token() {
        assert!(verify(Some(token())));
    }

    #[test]
    fn constant_time_eq_matches_semantics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }
}
