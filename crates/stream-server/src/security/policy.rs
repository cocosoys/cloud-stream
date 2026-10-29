//! 内置策略。
//!
//! 两个策略都是**测试工具**，不是安全措施：
//!
//! * [`AllowAll`] 是默认值，保持本项目现状（局域网内不鉴权）。
//! * [`DenyAll`] 用来验收骨架 —— `--policy deny-all` 起一次，画面上什么都没
//!   了、日志里却能看到 `authz deny surface=...` 一行行刷，就说明插桩点真的
//!   在工作；反过来，某个功能在 `deny-all` 下**照常可用**，那个入口就是漏了
//!   插桩。
//!
//! 真正的策略属于上层项目（webgame 等）。它们要做的是在这里之外实现
//! [`Policy`]，然后 `security::install(Arc::new(...))`。不提供"从文件读规则"
//! 之类的通用引擎 —— 权限模型是产品决定，不是框架能猜的。

use std::sync::Arc;

use super::{Event, EventKind, Policy, Verdict};

/// 全放行。所有请求都当作合法。
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAll;

impl Policy for AllowAll {
    fn name(&self) -> &'static str {
        "allow-all"
    }

    fn check(&self, _ev: &Event<'_>) -> Verdict {
        Verdict::Allow
    }
}

/// 全拒绝。只用于验收插桩覆盖率。
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyAll;

impl Policy for DenyAll {
    fn name(&self) -> &'static str {
        "deny-all"
    }

    fn check(&self, ev: &Event<'_>) -> Verdict {
        // `startup` 不是外部可达面 —— 它是进程自己在装配，没有"别人"能触发。
        // 拒掉它等于"禁止启动"，而 deny-all 是覆盖率探针，需要一台能起来的
        // 进程去探。第一版没放行，实测启动即 `bail`，探针根本跑不起来。
        if matches!(ev.kind, EventKind::Startup) {
            return Verdict::Allow;
        }
        Verdict::Deny("deny-all policy")
    }
}

/// 内置策略名，供 `--policy` 与错误提示使用。
pub const BUILTIN: &[&str] = &["allow-all", "deny-all"];

/// 按名字构造。未知名返回 `None`，由调用方决定是报错还是退回默认。
pub fn by_name(name: &str) -> Option<Arc<dyn Policy>> {
    match name.trim().to_ascii_lowercase().as_str() {
        "allow-all" | "allow" | "" => Some(Arc::new(AllowAll)),
        "deny-all" | "deny" => Some(Arc::new(DenyAll)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{ClipOp, Device, Endpoint, EventKind, MediaDir, Principal};

    #[test]
    fn names_resolve() {
        assert_eq!(by_name("allow-all").unwrap().name(), "allow-all");
        assert_eq!(by_name("DENY").unwrap().name(), "deny-all");
        // 空串按默认处理，这样 `--policy ""` 不会让进程起不来。
        assert_eq!(by_name("").unwrap().name(), "allow-all");
        assert!(by_name("role-based").is_none());
    }

    #[test]
    fn deny_all_covers_the_whole_surface() {
        let p = DenyAll;
        let who = Principal::default();
        for kind in [
            EventKind::Http(Endpoint::Upload),
            EventKind::Input(Device::Pointer),
            EventKind::Clipboard(ClipOp::Read),
            EventKind::Media(MediaDir::MicIn),
        ] {
            let ev = Event {
                kind,
                principal: &who,
            };
            assert!(!p.check(&ev).allowed(), "{} leaked through deny-all", kind.label());
        }
    }

    /// 探针必须能启动。`startup` 是进程装配，不是外部入口 —— 拒它等于把
    /// 探针自己关在门外（第一版就是这么错的，实测启动即 bail）。
    #[test]
    fn deny_all_still_lets_the_process_start() {
        let p = DenyAll;
        let who = Principal::default();
        let ev = Event {
            kind: EventKind::Startup,
            principal: &who,
        };
        assert_eq!(p.check(&ev), Verdict::Allow);
    }
}
