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

use super::{Endpoint, Event, EventKind, FileOp, Policy, Role, Verdict};

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

/// 按 [`Role`] 裁决 —— #110 起这是**唯一**一个真的读 `Principal` 的内置策略。
///
/// `allow-all` / `deny-all` 都不看身份，于是「令牌带租户与角色」这件事在策略层
/// 是空转的：装上一个不看身份的策略，再好的身份也只是日志里的一个字段。这个
/// 策略把 [`Role`] 的三级翻译成一张动作表：
///
/// | 动作类 | `Host` | `Operator` | `Viewer` |
/// |---|---|---|---|
/// | 观测（状态、清单、画面、声音、统计） | 放行 | 放行 | 放行 |
/// | 建连（`signaling`） | 放行 | 放行 | 放行 |
/// | 输入 / 剪贴板 / 采音摄像 / 上传 | 放行 | 放行 | 拒绝 |
/// | 流参数（码率、帧率、分辨率、码控） | 放行 | 放行 | 拒绝 |
/// | `http.control`（踢会话、重开采集器） | 放行 | 拒绝 | 拒绝 |
/// | `file.write-outside` | 放行 | 拒绝 | 拒绝 |
///
/// 三条设计取舍，都是踩过坑才写下来的：
///
/// * **`signaling` 对观众放行**。观众要的正是画面，而画面只能从信令建起来；
///   把建连挡掉等于「按角色分级」直接把这个角色废掉。
/// * **`http.control` 是主机级**。那条路径会踢掉别人的会话、重开共享采集器 ——
///   它动的不只是自己那一份，所以不能跟着 `Operator` 走。
/// * **观测类是所有人的底线**。`is_observational` 已经给出了「什么算只读」的
///   唯一答案，这里不再复述一遍，两个地方各写一份必然有一天对不上。
#[derive(Debug, Default, Clone, Copy)]
pub struct RoleBased;

impl Policy for RoleBased {
    fn name(&self) -> &'static str {
        "role-based"
    }

    fn check(&self, ev: &Event<'_>) -> Verdict {
        // `startup` 是进程自己在装配，没有"别人"能触发它 —— 拒掉它等于禁止
        // 启动，而 `main` 会因为这一条直接 bail。`deny-all` 也踩过同一个坑。
        if matches!(ev.kind, EventKind::Startup) {
            return Verdict::Allow;
        }
        // 观测与建连没有角色之分 —— 先答掉最大的一类，后面的 match 只写差异。
        if ev.kind.is_observational() || matches!(ev.kind, EventKind::Signaling) {
            return Verdict::Allow;
        }
        // 压缩协商是**每条连接自己的**选择（gzip 与否只影响这一条 socket），
        // 不动主机也不动别人，所以观众发它也放行。`viewer_may_run` 的
        // allow-list 里同样有它，两层必须给出同一个答案。
        if matches!(ev.kind, EventKind::Control("control.gzip")) {
            return Verdict::Allow;
        }
        match ev.principal.role {
            Role::Host => Verdict::Allow,
            // #110: 判据走 `Principal::may_operate`，不走 `Role` —— 后者不知道
            // "这部署根本没有门"。用 `Role` 的话，一个没配令牌表、却显式写了
            // `--policy role-based` 的单用户机器会把自己的键鼠全挡住，而日志
            // 里每一行都写着"拒绝 viewer"，看起来完全正常。
            _ if ev.principal.may_operate() => match ev.kind {
                EventKind::Http(Endpoint::Control) => Verdict::Deny("host-only surface"),
                EventKind::File(FileOp::WriteOutside) => Verdict::Deny("host-only surface"),
                _ => Verdict::Allow,
            },
            _ => Verdict::Deny("viewer role"),
        }
    }
}

/// 内置策略名，供 `--policy` 与错误提示使用。
///
/// `auto` 排在第一个是刻意的：它是默认值，也是唯一需要看配置才能定下来的名字
/// （见 `main` —— 有令牌表才升级成 `role-based`）。
pub const BUILTIN: &[&str] = &["auto", "allow-all", "role-based", "deny-all"];

/// 按名字构造。未知名返回 `None`，由调用方决定是报错还是退回默认。
///
/// `auto` **不在这里**解析：它要读配置（有没有令牌表），而这一层只认名字。
/// 调用方先把它换成具体名字再进来。
pub fn by_name(name: &str) -> Option<Arc<dyn Policy>> {
    match name.trim().to_ascii_lowercase().as_str() {
        "allow-all" | "allow" | "" => Some(Arc::new(AllowAll)),
        "role-based" | "roles" | "rbac" => Some(Arc::new(RoleBased)),
        "deny-all" | "deny" => Some(Arc::new(DenyAll)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{
        check_with, ClipOp, Device, Endpoint, EventKind, MediaDir, Principal, TenantId,
    };

    #[test]
    fn names_resolve() {
        assert_eq!(by_name("allow-all").unwrap().name(), "allow-all");
        assert_eq!(by_name("DENY").unwrap().name(), "deny-all");
        assert_eq!(by_name("role-based").unwrap().name(), "role-based");
        assert_eq!(by_name("RBAC").unwrap().name(), "role-based");
        // 空串按默认处理，这样 `--policy ""` 不会让进程起不来。
        assert_eq!(by_name("").unwrap().name(), "allow-all");
        // `auto` 要读配置才能定，这一层不认它 —— 认了会让调用方以为已经解析过。
        assert!(by_name("auto").is_none());
        assert!(by_name("sudo").is_none());
    }

    fn who(role: Role) -> Principal {
        Principal::default().with_grant(TenantId::new("alpha"), role)
    }

    /// 三级角色的差别必须真的体现在裁决上，否则「令牌带身份」在策略层是空转。
    #[test]
    fn role_based_separates_the_three_roles() {
        let p = RoleBased;
        let check = |role: Role, kind: EventKind| check_with(&p, kind, &who(role));

        // 输入：观众不行，操作者与主机都行。
        let kbd = EventKind::Input(Device::Keyboard);
        assert_eq!(check(Role::Viewer, kbd), Verdict::Deny("viewer role"));
        assert_eq!(check(Role::Operator, kbd), Verdict::Allow);
        assert_eq!(check(Role::Host, kbd), Verdict::Allow);

        // 跨会话运维：只有主机级。
        let recycle = EventKind::Http(Endpoint::Control);
        assert_eq!(check(Role::Operator, recycle), Verdict::Deny("host-only surface"));
        assert_eq!(check(Role::Host, recycle), Verdict::Allow);

        // 观测是所有人的底线 —— 观众看不到画面就没有存在意义。
        for kind in [
            EventKind::Http(Endpoint::Status),
            EventKind::Http(Endpoint::Health),
            EventKind::Media(MediaDir::VideoOut),
            EventKind::Media(MediaDir::AudioOut),
            EventKind::Diagnostics("control.stats"),
        ] {
            assert_eq!(
                check(Role::Viewer, kind),
                Verdict::Allow,
                "viewer should keep {}",
                kind.label()
            );
        }

        // 建连对观众放行：画面只能从这里来。
        assert_eq!(check(Role::Viewer, EventKind::Signaling), Verdict::Allow);
        // 压缩协商是每条连接自己的事。
        assert_eq!(
            check(Role::Viewer, EventKind::Control("control.gzip")),
            Verdict::Allow
        );
        // 写上传目录之外的位置：主机级专属。
        assert_eq!(
            check(Role::Operator, EventKind::File(FileOp::WriteOutside)),
            Verdict::Deny("host-only surface")
        );
        // 读剪贴板仍是「看」，写才是「动」。
        assert_eq!(
            check(Role::Viewer, EventKind::Clipboard(ClipOp::Read)),
            Verdict::Deny("viewer role")
        );
    }

    /// `startup` 在三个角色下都必须放行 —— 它是进程自己，不是外部入口。
    #[test]
    fn role_based_never_blocks_startup() {
        for role in [Role::Host, Role::Operator, Role::Viewer] {
            assert_eq!(
                check_with(&RoleBased, EventKind::Startup, &who(role)),
                Verdict::Allow
            );
        }
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
