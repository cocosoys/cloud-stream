//! 会话令牌：请求侧的凭据提取与比对 —— #101。
//!
//! 这个模块只回答一个问题：**这次请求有没有出示正确的口令**。判定之后的
//! "谁能做什么"仍然归 [`crate::security::Policy`]，两者不混 —— 令牌是
//! "你是谁"的前置条件，策略是"你能干什么"。
//!
//! 载体与前端 `addons/selkies-web-core/lib/session-token.js` 逐条对齐。那份
//! 文件才是契约的源头，这里只是服务端的另一半：
//!
//! | 载体 | 前端函数 | 为什么需要它 |
//! |---|---|---|
//! | `Authorization: Bearer <t>` | `sessionAuthHeaders` | fetch / XHR |
//! | `Cookie: selkies_token=<t>` | `installSessionCookie` | 浏览器自己发的请求（iframe、下载、WS 握手） |
//! | `?token=<t>` | `withSessionToken` | 页面首次打开、被导航到的 URL |
//! | `Sec-WebSocket-Protocol: selkies.token.<b64url>` | `sessionTokenProtocols` | 信令握手，避免令牌进 URL |
//!
//! # 为什么 401 要带 `WWW-Authenticate: Bearer realm="Selkies"`
//!
//! 前端的 `auth-guard.js` 用这条响应头区分两种 401：Basic 墙（换一个文档才
//! 能重新登录，所以它重载页面）和令牌不匹配（重载也还是同一个令牌，所以
//! 它什么都不做）。少了这个头，输错口令的页面会每 30 s 自刷一次，而且永远
//! 好不了。

use base64::Engine as _;

use crate::security::{Credential, Principal};

/// 401 响应里 `WWW-Authenticate` 的 realm。前端按 `^Bearer realm="Selkies`
/// 匹配，改这里等于改协议。
pub const REALM: &str = "Selkies";

/// 前端写入的会话 cookie 名（`session-token.js` 的 `SESSION_TOKEN_COOKIE`）。
pub const COOKIE_NAME: &str = "selkies_token";

/// 信令握手携带令牌的 subprotocol 前缀。值本身是令牌的 base64url（无填充），
/// 因为 subprotocol 名只允许 token 字符。
pub const PROTOCOL_PREFIX: &str = "selkies.token.";

/// 一扇门：持有正确口令，能回答"这次请求对不对"。
///
/// 只存一个字符串，因为本项目是单用户单口令；多用户是上层项目的事，
/// 它们要实现自己的 [`crate::security::Policy`]，而不是把用户表塞进这里。
#[derive(Debug, Clone)]
pub struct TokenGate {
    token: String,
}

impl TokenGate {
    /// 空口令等于没设口令，返回 `None` —— 调用方据此退回当前行为（不鉴权）。
    /// 这样 `--auth-token ""` 不会把机器锁死，也不会让人误以为开着。
    pub fn new(token: impl Into<String>) -> Option<Self> {
        let token = token.into();
        if token.trim().is_empty() {
            None
        } else {
            Some(Self { token })
        }
    }

    /// 口令是否匹配。用常量时间比较：`==` 在第一个不同的字节就返回，等于把
    /// 口令前缀的长度泄露给能反复试的人。
    pub fn verify(&self, presented: &str) -> bool {
        constant_time_eq(self.token.as_bytes(), presented.as_bytes())
    }

    /// 从一次请求的各个载体里找出令牌并校验。
    ///
    /// 顺序即优先级：`Authorization` 是脚本显式声明的，压过浏览器自动带的
    /// cookie；信令握手没有 header 可放（浏览器的 WebSocket API 不让设），
    /// 所以它排最后但必须有。
    pub fn check_request(
        &self,
        headers: &[httparse::Header<'_>],
        path: &str,
    ) -> Option<Credential> {
        for candidate in presented(headers, path) {
            if self.verify(&candidate) {
                return Some(Credential::Bearer);
            }
        }
        None
    }
}

/// 按优先级枚举这次请求出示的令牌。返回所有候选而不是第一个，是因为一个
/// 页面可能同时带着过期的 cookie 和正确的新令牌（`?token=` 正是用来覆盖
/// 旧 cookie 的，见前端 `installSessionCookie` 的注释）。
fn presented(headers: &[httparse::Header<'_>], path: &str) -> Vec<String> {
    let mut out = Vec::new();

    if let Some(v) = header(headers, "authorization").and_then(bearer_value) {
        out.push(v);
    }
    if let Some(t) = query_param(path, "token") {
        out.push(t);
    }
    if let Some(v) = header(headers, "cookie").and_then(cookie_value) {
        out.push(v);
    }
    out.extend(
        header(headers, "sec-websocket-protocol")
            .map(subprotocol_values)
            .unwrap_or_default(),
    );

    out
}

/// 大小写不敏感地取一个 header（HTTP 头名不区分大小写，`httparse` 不做归一）。
fn header<'a>(headers: &'a [httparse::Header<'a>], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .and_then(|h| std::str::from_utf8(h.value).ok())
}

/// `Authorization: Bearer <t>` → `<t>`。其他 scheme（`Basic`）返回 `None`：
/// 本项目不做 Basic，遇到它按"没出示令牌"处理，让前端拿到带 realm 的 401。
fn bearer_value(value: &str) -> Option<String> {
    let rest = value
        .trim()
        .strip_prefix("Bearer ")
        .or_else(|| value.trim().strip_prefix("bearer "))?;
    let token = rest.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_owned())
    }
}

/// `Cookie: a=1; selkies_token=<t>; b=2` → `<t>`。
///
/// 不能简单地找子串：`xselkies_token=` 里也有 `selkies_token=`。所以按 `;`
/// 切段，再对每段按第一个 `=` 切名值。
fn cookie_value(cookie_header: &str) -> Option<String> {
    for part in cookie_header.split(';') {
        let part = part.trim();
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        if name.trim() == COOKIE_NAME {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

/// `selkies, selkies.token.<b64url>` → 解码出的令牌。
///
/// 前端只在令牌来自 URL fragment 时才发这两个 subprotocol（fragment 不会被
/// 浏览器发出去，所以令牌进了握手而不是 URL）。解码失败就跳过 —— 一个畸形
/// 的 subprotocol 不能让整次握手按 500 处理。
fn subprotocol_values(header: &str) -> Vec<String> {
    header
        .split(',')
        .filter_map(|p| p.trim().strip_prefix(PROTOCOL_PREFIX))
        .filter_map(|b64| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(b64)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
        })
        .collect()
}

/// 从 `path?query` 里取一个参数。手写而不引 `url` crate：这里只需要
/// "取一个已知名字的 ASCII 值"，多一个依赖不值得。
fn query_param(path: &str, name: &str) -> Option<String> {
    let query = path.split_once('?')?.1;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key == name && !value.is_empty() {
            return Some(percent_decode(value));
        }
    }
    None
}

/// 只解 `%XX`，不解 `+`（query 里 `+` 是字面加号；表单编码才当空格）。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 长度不等直接失败（长度本身不是秘密，前端也没打算藏），等长时逐字节累积
/// 差异 —— 没有提前返回，也就没有"第几个字节错了"的侧信道。
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

/// 这次请求的完整身份：来源地址 + 凭据种类。
///
/// `credential` 为 `None` 表示没出示令牌（或没开鉴权），策略可以据此区分
/// "匿名观众"和"持有口令的操作员"。
pub fn principal(peer: std::net::IpAddr, credential: Credential) -> Principal {
    let mut who = Principal::from_peer(peer);
    who.credential = credential;
    who
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdrs(pairs: &[(&'static str, &'static str)]) -> Vec<httparse::Header<'static>> {
        pairs
            .iter()
            .map(|(n, v)| httparse::Header { name: n, value: v.as_bytes() })
            .collect()
    }

    fn gate(token: &str) -> TokenGate {
        TokenGate::new(token).expect("non-empty token")
    }

    #[test]
    fn an_empty_token_means_no_gate() {
        assert!(TokenGate::new("").is_none());
        assert!(TokenGate::new("   ").is_none());
        assert!(TokenGate::new("secret").is_some());
    }

    #[test]
    fn bearer_header_is_accepted() {
        let g = gate("s3cret");
        assert_eq!(
            g.check_request(&hdrs(&[("Authorization", "Bearer s3cret")]), "/"),
            Some(Credential::Bearer)
        );
        // Header names are case-insensitive.
        assert!(g
            .check_request(&hdrs(&[("authorization", "Bearer s3cret")]), "/")
            .is_some());
        // A different token is not.
        assert!(g
            .check_request(&hdrs(&[("Authorization", "Bearer wrong")]), "/")
            .is_none());
        // Basic is not this door.
        assert!(g
            .check_request(&hdrs(&[("Authorization", "Basic czNjcmV0")]), "/")
            .is_none());
    }

    #[test]
    fn the_session_cookie_is_accepted_among_others() {
        let g = gate("s3cret");
        let h = hdrs(&[("Cookie", "a=1; selkies_token=s3cret; b=2")]);
        assert!(g.check_request(&h, "/").is_some());
        // A lookalike name is not the cookie: substring matching would say yes.
        let h = hdrs(&[("Cookie", "xselkies_token=s3cret")]);
        assert!(g.check_request(&h, "/").is_none());
        let h = hdrs(&[("Cookie", "selkies_token=")]);
        assert!(g.check_request(&h, "/").is_none());
    }

    #[test]
    fn the_query_token_is_accepted_and_percent_decoded() {
        let g = gate("a b/c");
        assert!(g
            .check_request(&[], "/?token=a%20b%2Fc")
            .is_some());
        assert!(g.check_request(&[], "/?other=1&token=a%20b%2Fc").is_some());
        assert!(g.check_request(&[], "/?token=nope").is_none());
        // No query at all must not panic or match.
        assert!(g.check_request(&[], "/").is_none());
    }

    #[test]
    fn the_websocket_subprotocol_carries_the_token() {
        let g = gate("s3cret");
        // base64url("s3cret") without padding.
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("s3cret");
        let value = format!("selkies, selkies.token.{encoded}");
        let h = hdrs(&[("Sec-WebSocket-Protocol", Box::leak(value.into_boxed_str()))]);
        assert!(g.check_request(&h, "/api/webrtc/signaling/").is_some());
    }

    #[test]
    fn a_malformed_subprotocol_is_skipped_not_fatal() {
        let g = gate("s3cret");
        let h = hdrs(&[("Sec-WebSocket-Protocol", "selkies, selkies.token.!!!not-base64")]);
        assert!(g.check_request(&h, "/").is_none());
    }

    #[test]
    fn a_stale_cookie_beside_a_fresh_query_token_still_passes() {
        // The page was opened with `?token=new`, but the browser still holds the
        // old cookie. Order alone would lock the user out on a cookie-first read.
        let g = gate("new");
        let h = hdrs(&[("Cookie", "selkies_token=old")]);
        assert!(g.check_request(&h, "/?token=new").is_some());
    }

    #[test]
    fn comparison_is_constant_time_in_shape() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
