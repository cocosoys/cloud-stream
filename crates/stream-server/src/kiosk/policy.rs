//! 策略层：用注册表把"用户自己就能绕过去"的那几条路关掉。
//!
//! 这是四档锁定里的第 1 档，管的是**系统级**行为，注入端和键盘钩子都管不到：
//!
//! | 键 | 关掉的东西 |
//! |---|---|
//! | `NoWinKeys` | `Win` 键的所有系统组合（开始菜单、`Win+R`、`Win+D`……） |
//! | `DisableTaskMgr` | 任务管理器 —— 它一弹出来就压在游戏上面，而串流采的是整屏 |
//!
//! 两项都在 `HKCU` 下，只影响当前用户，不需要管理员，改回来只要删掉值。
//!
//! ## 一条硬规则：先写欠条，再动系统
//!
//! 进程可能在任何一刻被强杀（今天就发生过：`taskkill /F`、崩溃、断电）。如果在
//! **改完注册表之后**才去写还原记录，那么被杀的瞬间原值就永久丢了 —— 那台机器
//! 会带着"任务管理器被禁用"活到下一次有人手工改回来。
//!
//! 所以顺序是死的：**读原值 → 落盘 → 改注册表**。落地失败就什么都不改。
//!
//! 同一个理由，[`engage`] 一开始会先 [`restore`] 一次：上一次残留的记录必须先
//! 还回去，否则这次的"原值"读到的是我们上次改过的值（`1`），下一次还原就把
//! `1` 当成了用户的原设置。

use std::path::Path;

use serde::{Deserialize, Serialize};

/// `NoWinKeys` 所在的键。
pub const KEY_EXPLORER: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
/// `DisableTaskMgr` 所在的键。
pub const KEY_SYSTEM: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\System";

/// 我们要改的两条值：`(键路径, 值名)`。
pub const VALUES: &[(&str, &str)] = &[(KEY_EXPLORER, "NoWinKeys"), (KEY_SYSTEM, "DisableTaskMgr")];

/// 还原记录的文件名（落在 `games\runtime\` 下）。
pub const STATE_FILE: &str = "kiosk-policy.json";

/// 记录格式版本。将来字段语义变了靠它做迁移，而不是靠猜。
pub const SCHEMA: u32 = 1;

/// 一条值的原状。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Saved {
    pub path: String,
    pub name: String,
    /// 原本存在与否。不存在意味着还原动作是**删掉我们建的值**，而不是写回。
    pub existed: bool,
    /// 原本的 DWORD 值。`existed == false` 时无意义。
    pub value: Option<u32>,
}

impl Saved {
    /// 还原这条值该做什么。抽成纯函数是为了能测 —— 真正的注册表调用在测试里
    /// 不该发生（那会改掉跑测试那台机器的设置）。
    pub fn restore_action(&self) -> RestoreAction {
        match (self.existed, self.value) {
            (true, Some(v)) => RestoreAction::Set(v),
            // 原本存在、但值不是 DWORD（比如 REG_SZ）：我们读不出原值，也不敢
            // 猜一个写回去。删掉是最小破坏 —— 一个类型不对的值本来也不会被系统
            // 当成策略生效。
            (true, None) => RestoreAction::Delete,
            (false, _) => RestoreAction::Delete,
        }
    }
}

/// 还原动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreAction {
    Set(u32),
    Delete,
}

/// 落盘的还原记录。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Snapshot {
    pub schema: u32,
    /// 记录建立时刻（Unix 秒），只用于日志与面板展示。
    pub at_unix: u64,
    pub keys: Vec<Saved>,
}

/// 有没有一份"欠着的"还原记录。
pub fn pending(file: &Path) -> bool {
    file.is_file()
}

fn read_snapshot(file: &Path) -> Result<Snapshot, String> {
    let text = std::fs::read_to_string(file)
        .map_err(|err| format!("读不到还原记录 {}：{err}", file.display()))?;
    serde_json::from_str(&text)
        .map_err(|err| format!("还原记录 {} 不是合法 JSON：{err}", file.display()))
}

fn write_snapshot(file: &Path, snapshot: &Snapshot) -> Result<(), String> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("建目录 {} 失败：{err}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(snapshot)
        .map_err(|err| format!("序列化还原记录失败：{err}"))?;
    // 先写临时文件再改名：写到一半被杀会留下一个**半截 JSON**，而那份文件的
    // 存在本身就是"还有策略没还原"的信号 —— 半截文件会让这个信号永久成立，
    // 却永远解析不出来。
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|err| format!("写 {} 失败：{err}", tmp.display()))?;
    std::fs::rename(&tmp, file).map_err(|err| format!("改名到 {} 失败：{err}", file.display()))
}

/// 上锁：记录原值 → 落盘 → 改注册表。返回本次记录了哪些值。
pub fn engage(file: &Path) -> Result<Snapshot, String> {
    // 残留必须先还回去，否则"原值"读到的会是上一次我们写进去的 1。
    if pending(file) {
        let n = restore(file)?;
        log::info!("kiosk: 上锁前先还原了上一次的 {n} 条策略");
    }

    let mut keys = Vec::with_capacity(VALUES.len());
    for (path, name) in VALUES {
        keys.push(win::read_value(path, name)?);
    }

    let snapshot = Snapshot {
        schema: SCHEMA,
        at_unix: crate::game::now_unix(),
        keys,
    };
    // 欠条先落地。这一步失败就原样返回，改动一条都不做。
    write_snapshot(file, &snapshot)?;

    for saved in &snapshot.keys {
        if let Err(err) = win::write_dword(&saved.path, &saved.name, 1) {
            // 改到一半失败：把已经改的还回去，别留一个"半锁"的桌面。
            log::warn!("kiosk: 写 {} 失败（{err}），开始回滚", saved.name);
            let _ = restore(file);
            return Err(err);
        }
    }
    Ok(snapshot)
}

/// 解锁：按记录还原，删除记录。返回还原了几条。
///
/// 幂等：记录不在时返回 `Ok(0)`。这条性质让 `--unlock` 可以无脑调用 —— 无论
/// 服务当时是否还活着、记录是否已经被对方还过。
pub fn restore(file: &Path) -> Result<usize, String> {
    if !file.is_file() {
        return Ok(0);
    }
    let snapshot = match read_snapshot(file) {
        Ok(s) => s,
        Err(err) => {
            // 记录坏了也不能把策略留在机器上。删掉值是最保守的选择：它们是
            // "关闭功能"的策略，删掉等于"恢复默认"，而默认是用户想要的那一侧。
            log::warn!("kiosk: {err}；按最保守的方式清理已知的策略值");
            let mut n = 0;
            for (path, name) in VALUES {
                let _ = win::delete_value(path, name);
                n += 1;
            }
            let _ = std::fs::remove_file(file);
            return Ok(n);
        }
    };

    let mut n = 0;
    let mut failed = 0;
    for saved in &snapshot.keys {
        let result = match saved.restore_action() {
            RestoreAction::Set(v) => win::write_dword(&saved.path, &saved.name, v),
            RestoreAction::Delete => win::delete_value(&saved.path, &saved.name),
        };
        match result {
            Ok(()) => n += 1,
            Err(err) => {
                failed += 1;
                log::warn!("kiosk: 还原 {} 失败：{err}", saved.name);
            }
        }
    }
    if failed == 0 {
        let _ = std::fs::remove_file(file);
    }
    Ok(n)
}

// ------------------------------------------------------------------ Win32

#[cfg(windows)]
mod win {
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyW, RegDeleteValueW, RegQueryValueExW, RegSetValueExW,
        HKEY_CURRENT_USER, REG_DWORD, REG_VALUE_TYPE,
    };

    use super::Saved;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 打开（必要时创建）一条键，交给 `f`。键句柄一定会被关。
    ///
    /// `RegCreateKeyW` 对已存在的键是"打开"，对不存在的键是"逐级创建"，正好是
    /// 我们要的两件事。它请求的是完全访问，所以不需要再传 `samdesired` —— 这也
    /// 是选它而不是 `RegCreateKeyExW` 的原因：那个的九参数签名在不同版本之间
    /// 变过，而这里只是读一个 DWORD。
    fn with_key<T>(path: &str, f: impl FnOnce(windows_sys::Win32::System::Registry::HKEY) -> T) -> Option<T> {
        let sub = wide(path);
        // windows-sys 里 `HKEY` 就是 `isize`，句柄 0 表示失败。
        let mut key: windows_sys::Win32::System::Registry::HKEY = 0;
        let status = unsafe { RegCreateKeyW(HKEY_CURRENT_USER, sub.as_ptr(), &mut key) };
        if status != ERROR_SUCCESS {
            log::warn!("kiosk: 打开 HKCU\\{path} 失败（{status}）");
            return None;
        }
        let out = f(key);
        unsafe {
            let _ = RegCloseKey(key);
        }
        Some(out)
    }

    /// 读一条 DWORD。值不存在返回 `existed: false`（不是错误）；类型不对返回
    /// `existed: true, value: None`。
    pub fn read_value(path: &str, name: &str) -> Result<Saved, String> {
        let mut saved = Saved {
            path: path.to_owned(),
            name: name.to_owned(),
            existed: false,
            value: None,
        };
        let name_w = wide(name);
        let mut data = 0u32;
        let mut cb = std::mem::size_of::<u32>() as u32;
        let mut ty: REG_VALUE_TYPE = 0;

        let mut found = Ok(());
        with_key(path, |key| {
            let status = unsafe {
                RegQueryValueExW(
                    key,
                    name_w.as_ptr(),
                    ptr::null(),
                    &mut ty,
                    &mut data as *mut u32 as *mut u8,
                    &mut cb,
                )
            };
            match status {
                ERROR_SUCCESS => {
                    saved.existed = true;
                    if ty == REG_DWORD && cb == 4 {
                        saved.value = Some(data);
                    }
                }
                // 键存在、值不存在：这是绝大多数机器的正常状态。
                ERROR_FILE_NOT_FOUND => {}
                other => found = Err(format!("读 HKU\\{path}\\{name} 失败（{other}）")),
            }
        })
        .ok_or_else(|| format!("打开 HKCU\\{path} 失败"))?;

        found.map(|()| saved)
    }

    pub fn write_dword(path: &str, name: &str, value: u32) -> Result<(), String> {
        let name_w = wide(name);
        let bytes = value.to_le_bytes();
        let done = with_key(path, |key| unsafe {
            RegSetValueExW(
                key,
                name_w.as_ptr(),
                0,
                REG_DWORD,
                bytes.as_ptr(),
                bytes.len() as u32,
            )
        })
        .ok_or_else(|| format!("打开 HKCU\\{path} 失败"))?;
        if done != ERROR_SUCCESS {
            return Err(format!("写 HKCU\\{path}\\{name} 失败（{done}）"));
        }
        Ok(())
    }

    pub fn delete_value(path: &str, name: &str) -> Result<(), String> {
        let name_w = wide(name);
        let done = with_key(path, |key| unsafe { RegDeleteValueW(key, name_w.as_ptr()) })
            .ok_or_else(|| format!("打开 HKCU\\{path} 失败"))?;
        // 值本来就不在 = 已经是我们要的状态。
        if done == ERROR_SUCCESS || done == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        Err(format!("删 HKCU\\{path}\\{name} 失败（{done}）"))
    }
}

#[cfg(not(windows))]
mod win {
    use super::Saved;

    pub fn read_value(path: &str, name: &str) -> Result<Saved, String> {
        Ok(Saved {
            path: path.to_owned(),
            name: name.to_owned(),
            existed: false,
            value: None,
        })
    }
    pub fn write_dword(_path: &str, _name: &str, _value: u32) -> Result<(), String> {
        Ok(())
    }
    pub fn delete_value(_path: &str, _name: &str) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_action_follows_the_original_state() {
        let set = Saved {
            path: KEY_EXPLORER.into(),
            name: "NoWinKeys".into(),
            existed: true,
            value: Some(0),
        };
        // 原来显式写着 0（Win 键可用），还原就必须写回 0，而不是删掉。
        assert_eq!(set.restore_action(), RestoreAction::Set(0));

        let absent = Saved {
            existed: false,
            value: None,
            ..set.clone()
        };
        assert_eq!(absent.restore_action(), RestoreAction::Delete);

        let wrong_type = Saved {
            existed: true,
            value: None,
            ..set
        };
        assert_eq!(wrong_type.restore_action(), RestoreAction::Delete);
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let snapshot = Snapshot {
            schema: SCHEMA,
            at_unix: 1_700_000_000,
            keys: VALUES
                .iter()
                .map(|(p, n)| Saved {
                    path: (*p).to_owned(),
                    name: (*n).to_owned(),
                    existed: true,
                    value: Some(0),
                })
                .collect(),
        };
        let text = serde_json::to_string(&snapshot).unwrap();
        let back: Snapshot = serde_json::from_str(&text).unwrap();
        assert_eq!(back.keys.len(), VALUES.len());
        assert_eq!(back.keys[0].name, "NoWinKeys");
        assert_eq!(back.keys[0].value, Some(0));
    }

    #[test]
    fn restore_without_a_record_is_a_no_op() {
        let dir = std::env::temp_dir().join("cs-kiosk-policy-test");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("absent.json");
        let _ = std::fs::remove_file(&file);
        assert_eq!(restore(&file).unwrap(), 0);
        assert!(!pending(&file));
    }

    #[test]
    fn a_corrupt_record_still_clears_the_known_values() {
        // 记录坏掉时也不能把策略留在机器上。这里只断言"文件被删掉、返回条数
        // 不为零"——真正的注册表调用在测试机上不该发生（那会改掉跑测试的机器）。
        let dir = std::env::temp_dir().join("cs-kiosk-policy-test");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("corrupt.json");
        std::fs::write(&file, "{ not json").unwrap();
        let n = restore(&file).unwrap();
        assert_eq!(n, VALUES.len());
        assert!(!pending(&file));
    }
}
