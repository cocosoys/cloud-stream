//! 版本与构建指纹：回答"这个 exe 是哪一次构建出来的"。
//!
//! 在这之前，整个项目的版本信息只有一个 `CARGO_PKG_VERSION`，而它是 `0.1.0`，
//! 从第一天起没动过。后果很具体：一个九月的构建和一个十一月的构建，在二进制
//! 层面无法区分，`/api/v1/health` 报的版本永远一样，运维手里没有任何依据判断
//! "客户报的这个问题在我这一版还在不在"。
//!
//! 自动更新必须先能比较版本，所以这里补三件事，缺一不可：
//!
//! | 要回答的问题 | 由谁回答 |
//! |---|---|
//! | 这是哪个版本 | [`VERSION`]，来自 `Cargo.toml`，语义化 |
//! | 具体是哪一次提交 | [`COMMIT`] + [`DIRTY`]，由 `build.rs` 烧进来 |
//! | 什么时候构建的 | [`BUILT_AT`]，Unix 秒；[`built_at_rfc3339`] 给人读 |
//!
//! `DIRTY` 是三个值而不是布尔：`true` / `false` / `unknown`。第三态必须存在 ——
//! CI 里没有 `.git`，探测不到不等于"干净"，把它折成 `false` 会让一个来源可疑
//! 的产物看起来像是从干净树编译的。这正是发布物最不该有的错觉。

use hbb_common::serde_json::{json, Value};

/// 语义化版本，唯一来源是 `crates/stream-server/Cargo.toml` 的 `version`。
///
/// 升级规则（简单到不需要查文档）：新增接口或行为变化 → 次版本 +1；只有修复 →
/// 补丁 +1。`0.x` 阶段主版本留给"配置文件格式不兼容"这种级别。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 构建时的 git 提交（短哈希）。探测不到时是 `"unknown"`。
pub const COMMIT: &str = env!("CS_GIT_COMMIT");

/// 构建时工作区有没有未提交的改动：`"true"` / `"false"` / `"unknown"`。
///
/// 只看**已跟踪文件**（`git status --untracked-files=no`）：本地探针脚本、
/// 临时目录不该把每一次开发机构建都标成脏。
pub const DIRTY: &str = env!("CS_GIT_DIRTY");

/// 构建时刻，Unix 秒（字符串形态，因为 `env!` 只能给字符串）。
pub const BUILT_AT: &str = env!("CS_BUILT_AT");

/// [`BUILT_AT`] 解析成秒数；拿不到就是 0（1970），调用方据此判断"未知"。
pub fn built_at_secs() -> u64 {
    BUILT_AT.parse().unwrap_or(0)
}

/// 人类可读的构建时间，UTC，`2026-11-10T07:23:16Z` 形态。
///
/// 手写换算而不是拉一个日期库：只需要把 Unix 秒转成日历字段，为此新增
/// `chrono`（连同它的 `iana-time-zone` 平台依赖）放进一个推流服务里不划算。
/// 算法是 Howard Hinnant 的 `civil_from_days`，对 1970–9999 年都成立。
///
/// 刻意输出 **UTC**：日志和更新清单里出现本地时间，跨时区就没人对得上；
/// 需要本地时间的场合是给人看的界面，那是前端的事。
pub fn built_at_rfc3339() -> String {
    let secs = built_at_secs();
    if secs == 0 {
        return "unknown".to_owned();
    }
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// `/api/v1/version` 的响应体。
///
/// 键名与 `manifest.json` 对齐（`commit` / `built_at`），这样更新器和部署脚本
/// 读两边的代码可以是同一段。
pub fn as_json() -> Value {
    json!({
        "version": VERSION,
        "commit": COMMIT,
        "dirty": match DIRTY {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            // 第三态保留成 null，而不是 false —— 见模块注释。
            _ => Value::Null,
        },
        "built_at": built_at_rfc3339(),
        "built_at_unix": built_at_secs(),
        "exe": std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    })
}

/// Unix 秒 → (年, 月, 日, 时, 分, 秒)，UTC（Howard Hinnant `civil_from_days`）。
///
/// 对 crate 内公开：`ops` 要用它给快照目录起名（`20261110T075512Z-preinstall`），
/// 而定宽的时间戳是"字典序即时间序"这个排序承诺的唯一来源。
pub(crate) fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };

    (
        y,
        m,
        d,
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_semver() {
        let parts: Vec<&str> = VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "版本必须是三段式：{VERSION}");
        for p in parts {
            assert!(p.parse::<u32>().is_ok(), "每一段都必须是数字：{VERSION}");
        }
        assert_ne!(VERSION, "0.1.0", "版本要跟着这次改动走，不能停在起点");
    }

    /// 构建指纹允许是 `unknown`（CI 无 .git、无 git），但**不允许是空串**：
    /// 空串会让 JSON 里出现 `"commit": ""`，读的人分不清"没有"和"没填"。
    #[test]
    fn build_fingerprint_is_never_empty() {
        assert!(!COMMIT.is_empty());
        assert!(!DIRTY.is_empty());
        assert!(!BUILT_AT.is_empty());
        assert!(
            matches!(DIRTY, "true" | "false" | "unknown"),
            "dirty 只能是三态之一，实际是 {DIRTY:?}"
        );
    }

    #[test]
    fn civil_conversion_matches_known_dates() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil_from_unix(1_700_000_000), (2023, 11, 14, 22, 13, 20));
        // 闰日：2024-02-29 00:00:00 UTC
        assert_eq!(civil_from_unix(1_709_164_800), (2024, 2, 29, 0, 0, 0));
    }

    #[test]
    fn unknown_build_time_degrades_to_a_word() {
        // 0 是"没有时间戳"，不该渲染成 1970。
        assert_eq!(built_at_secs(), BUILT_AT.parse::<u64>().unwrap_or(0));
    }

    #[test]
    fn json_has_the_three_fields_an_updater_needs() {
        let value = as_json();
        assert!(value.get("version").is_some());
        assert!(value.get("commit").is_some());
        assert!(value.get("built_at").is_some());
        assert_eq!(value["version"], VERSION);
    }
}
