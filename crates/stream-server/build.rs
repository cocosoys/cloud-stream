//! 构建指纹与 Windows 版本资源。
//!
//! 这个文件只做两件事，都服务于同一个问题：**"这个 exe 是哪来的"**。
//! 在此之前 exe 里只有 `CARGO_PKG_VERSION`，而那是 `0.1.0`，从项目第一天起
//! 没变过 —— 换句话说，一个 2026 年 9 月的构建和一个 11 月的构建在二进制上
//! 无法区分，`/api/v1/health` 报的 `0.1.0` 也永远一样。自动更新、灰度回滚、
//! "客户报的问题在我这一版还在不在"全都建立在版本可比较之上，所以必须先补。
//!
//! 1. **烧进三个常量**（`CS_GIT_COMMIT` / `CS_GIT_DIRTY` / `CS_BUILT_AT`）：
//!    `version.rs` 用 `env!` 读走，`/api/v1/version` 报出来。
//! 2. **生成 VERSIONINFO 资源**：Windows 属性页的"文件版本/产品名"就是它。
//!    没有这段，exe 在资源管理器里是一张白纸，SmartScreen 和部署方都没有任何
//!    依据可看。
//!
//! 三条硬规矩：
//!
//! * **构建永不因为这里失败**。git 不在、SDK 不在、rc.exe 找不到 —— 全部退化
//!   成 `unknown` 并继续。构建失败的代价是整条流水线停住，而"版本号少了 commit"
//!   的代价只是少一个诊断线索，两者不对等。
//! * **部署链可以注入**。CI 机器上常常没有 `.git`，`deploy.ps1` 手里却有准确
//!   的 commit 和 dirty 状态；`CS_GIT_*` 环境变量优先于探测。
//! * **不改版本号来源**。版本只从 `Cargo.toml` 来，这里一个字节都不重复写。

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // 切分支或新提交后必须重新生成常量。监听 HEAD 的内容而不是整个 .git：
    // 索引文件随时在变，监听它等于每次构建都重跑这一段。
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    // 监听 Cargo.toml，这样"只改了 version"也会真的重建。见下面
    // `CS_VERSION` 那一段 —— 这条指令是那个机制的一半。
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-env-changed=CS_GIT_COMMIT");
    println!("cargo:rerun-if-env-changed=CS_GIT_DIRTY");
    println!("cargo:rerun-if-env-changed=CS_BUILT_AT");

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_owned());

    // 顺序不能反：环境变量是"说得出理由的准确值"，探测是"能拿到就用"。
    let commit = injected("CS_GIT_COMMIT")
        .or_else(|| git(&["rev-parse", "--short", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_owned());
    // `--untracked-files=no`：未跟踪文件（本地探针脚本、临时目录）不算脏。
    // 否则每一台开发机上都永远是 dirty，这个字段就失去意义了。
    let dirty = injected("CS_GIT_DIRTY")
        .or_else(|| {
            git(&["status", "--porcelain", "--untracked-files=no"]).map(|out| {
                // 有输出 = 有已跟踪文件被改过。刻意不解析具体内容：这里只需要
                // 一个是/否，解析路径反而会引入平台相关的换行差异。
                if out.trim().is_empty() { "false" } else { "true" }.to_owned()
            })
        })
        .unwrap_or_else(|| "unknown".to_owned());
    let built_at = injected("CS_BUILT_AT").unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "0".to_owned())
    });

    println!("cargo:rustc-env=CS_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=CS_GIT_DIRTY={dirty}");
    println!("cargo:rustc-env=CS_BUILT_AT={built_at}");
    // 版本号也走 build.rs，而不是源码里的 `env!("CARGO_PKG_VERSION")`。
    //
    // 理由是一次实测出来的坑：**只把 Cargo.toml 的 version 从 0.3.0 改成
    // 0.4.0，`cargo build` 会说 "Finished" 但根本不重编**，于是产出的 exe
    // 仍然报 0.3.0 —— 一个"版本号写着改过、二进制里还是旧的"的假象，而这个
    // 项目做版本层就是为了根治这种假象。
    //
    // 经 build.rs 中转之后，version 变成了构建脚本的输出：Cargo.toml 一变，
    // 构建脚本重跑，`rustc-env` 的值变了，cargo 因此认定这个 crate 脏了，
    // 必须重编。上面那条 `rerun-if-changed=Cargo.toml` 是这个链条的另一半，
    // 少任何一半都不生效。
    println!("cargo:rustc-env=CS_VERSION={version}");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        emit_version_resource(&version, &commit, &built_at);
    }
}

fn injected(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    Some(text)
}

/// 生成并链接 `VERSIONINFO`。
///
/// 找不到 `rc.exe` 就打印一条 `cargo:warning` 然后返回 —— 构建继续，只是 exe
/// 没有版本属性。这是刻意的不对称：缺版本资源是可接受的退化，构建失败不是。
fn emit_version_resource(version: &str, commit: &str, built_at: &str) {
    let Some(out_dir) = std::env::var_os("OUT_DIR").map(PathBuf::from) else {
        return;
    };
    let Some(rc) = find_rc() else {
        println!(
            "cargo:warning=rc.exe 没找到，stream-server.exe 将不带版本资源（功能不受影响）"
        );
        return;
    };

    let numeric = numeric_version(version);
    let rc_path = out_dir.join("stream-server.rc");
    let res_path = out_dir.join("stream-server.res");
    // 描述一律用 ASCII：rc.exe 按系统 ANSI 代码页读取 .rc，写中文会在非中文
    // 系统上变成乱码，而这个文件本来是给"看到 exe 属性"的人读的。
    let text = format!(
        "#include <winver.h>\n\
         1 VERSIONINFO\n\
         FILEVERSION {numeric}\n\
         PRODUCTVERSION {numeric}\n\
         FILEFLAGSMASK 0x3fL\n\
         FILEFLAGS 0x0L\n\
         FILEOS 0x40004L\n\
         FILETYPE 0x1L\n\
         FILESUBTYPE 0x0L\n\
         BEGIN\n\
         \x20 BLOCK \"StringFileInfo\"\n\
         \x20 BEGIN\n\
         \x20   BLOCK \"040904B0\"\n\
         \x20   BEGIN\n\
         \x20     VALUE \"CompanyName\", \"cloud-stream\"\n\
         \x20     VALUE \"FileDescription\", \"cloud-stream streaming server\"\n\
         \x20     VALUE \"FileVersion\", \"{version} ({commit})\"\n\
         \x20     VALUE \"InternalName\", \"stream-server\"\n\
         \x20     VALUE \"OriginalFilename\", \"stream-server.exe\"\n\
         \x20     VALUE \"ProductName\", \"cloud-stream\"\n\
         \x20     VALUE \"ProductVersion\", \"{version}\"\n\
         \x20     VALUE \"Comments\", \"built {built_at}\"\n\
         \x20   END\n\
         \x20 END\n\
         \x20 BLOCK \"VarFileInfo\"\n\
         \x20 BEGIN\n\
         \x20   VALUE \"Translation\", 0x409, 1200\n\
         \x20 END\n\
         END\n"
    );
    if std::fs::write(&rc_path, text).is_err() {
        return;
    }

    let status = Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_path)
        .arg(&rc_path)
        .status();
    match status {
        Ok(s) if s.success() => {
            println!("cargo:rustc-link-arg={}", res_path.display());
        }
        Ok(s) => println!(
            "cargo:warning=rc.exe 退出码 {:?}，跳过版本资源（构建继续）",
            s.code()
        ),
        Err(err) => println!("cargo:warning=rc.exe 起不来（{err}），跳过版本资源"),
    }
}

/// `0.2.0` → `0,2,0,0`。VERSIONINFO 固定要四段，缺的补 0。
fn numeric_version(version: &str) -> String {
    let mut parts: Vec<u32> = version
        .split(['.', '-', '+'])
        .take(4)
        .map(|p| p.trim().parse::<u32>().unwrap_or(0))
        .collect();
    while parts.len() < 4 {
        parts.push(0);
    }
    parts
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// 找 `rc.exe`：先看 `RC` 环境变量，再在 PATH 里找（`build.ps1` 会先把 Windows
/// SDK 的 bin 放进去），最后放弃。
fn find_rc() -> Option<PathBuf> {
    if let Some(explicit) = injected("RC") {
        let path = Path::new(&explicit).to_path_buf();
        if path.is_file() {
            return Some(path);
        }
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("rc.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
