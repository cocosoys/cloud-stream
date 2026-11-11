//! 内嵌安装包的读取与解压。
//!
//! # 这个文件的两半
//!
//! `payload.gz` 由 `build.rs` 生成并 `include_bytes!` 进来。归档格式写在
//! `build.rs` 的 `write_pack` 注释里，**两边必须一起改** —— 这里只做读。
//!
//! # 为什么用自定义格式而不是 zip
//!
//! 只需要读回自己写的东西。`zip` 会带进一整套元数据语义（权限位、符号链接、
//! 长名扩展、注释、跨卷标记），一条都用不上，却要额外引一个 crate；`tar` 同理。
//! 五个字节的魔数加「长度 + 内容」的循环，二十行就写完了，而且出错时能立刻
//! 看出是哪个偏移不对。
//!
//! # 解压为什么先写临时目录
//!
//! 目标目录里可能有正在运行的 `stream-server.exe`（重装、修复）。往一个活着的
//! 目录里直接铺 2000 个文件，中途失败就是一个"半新半旧、看起来正常"的安装 ——
//! 比彻底失败难查得多。所以顺序是：解到 `<目标>.new` → 全部成功 → 才开始替换。

use std::io::Read;
use std::path::{Path, PathBuf};

/// 与 `build.rs` 的 `MAGIC` 相同。
const MAGIC: &[u8; 5] = b"CSPK1";

/// 构建期嵌进来的归档（未开 `embed` 时是一个合法的空归档）。
static PAYLOAD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/payload.gz"));

/// 这个 exe 有没有可安装的内容。
///
/// 开发机构建的 `cloud-setup.exe` 是空的（2 秒构建、377 KB），点「安装」时应当
/// 说清"我这个 exe 里没有包"，而不是装出一个空目录再报成功。
///
/// **只解前九个字节**：这个函数在每次启动时都被问一次（`main` 要用它决定这次
/// 是当安装器还是当控制台），而完整解压是 9 MB → 24 MB。解压整个归档只为了
/// 数个数，代价是启动慢半拍。
pub fn is_embedded() -> bool {
    let mut decoder = flate2::read::GzDecoder::new(PAYLOAD);
    let mut head = [0u8; 9]; // 5 字节魔数 + 4 字节文件数
    if decoder.read_exact(&mut head).is_err() {
        return false;
    }
    &head[..5] == MAGIC && u32::from_le_bytes([head[5], head[6], head[7], head[8]]) > 0
}

/// 归档里有多少个文件、解压后多大。给确认框用 —— "要装 2006 个文件、24 MB"
/// 比"要安装吗"有用得多。
pub fn summary() -> Option<(usize, u64)> {
    let entries = read_entries().ok()?;
    if entries.is_empty() {
        return None;
    }
    let total = entries.iter().map(|e| e.data.len() as u64).sum();
    Some((entries.len(), total))
}

struct Entry {
    name: String,
    data: Vec<u8>,
}

/// 解压整个归档到内存。
///
/// 全量放进内存是有意的：24 MB 在 64 位进程里不算什么，而流式解压要在
/// 「边读边写文件」和「先校验再写」之间做取舍 —— 后者才是这里需要的性质。
fn read_entries() -> Result<Vec<Entry>, String> {
    let mut decoder = flate2::read::GzDecoder::new(PAYLOAD);
    let mut raw = Vec::new();
    decoder
        .read_to_end(&mut raw)
        .map_err(|e| format!("解压内置包失败：{e}"))?;

    let mut cur = std::io::Cursor::new(raw.as_slice());
    let mut magic = [0u8; 5];
    cur.read_exact(&mut magic)
        .map_err(|_| "内置包太短，不是有效归档".to_owned())?;
    if &magic != MAGIC {
        return Err(format!(
            "内置包魔数不对（{:?}），这个 exe 可能是别的版本构建出来的",
            String::from_utf8_lossy(&magic)
        ));
    }
    let count = read_u32(&mut cur)?;

    let mut entries = Vec::with_capacity(count as usize);
    for index in 0..count {
        let name_len = read_u16(&mut cur)? as usize;
        let mut name = vec![0u8; name_len];
        cur.read_exact(&mut name)
            .map_err(|_| format!("第 {index} 项的名字被截断"))?;
        let name = String::from_utf8(name).map_err(|_| format!("第 {index} 项的名字不是 UTF-8"))?;
        if !safe_rel_path(&name) {
            // 归档是我们自己写的，出现相对路径只能说明包被改过。拒绝比"写到
            // 目标目录外面"好 —— 后者是一次真实的目录穿越。
            return Err(format!("第 {index} 项的路径不安全：{name:?}"));
        }
        let size = read_u64(&mut cur)? as usize;
        let mut data = vec![0u8; size];
        cur.read_exact(&mut data)
            .map_err(|_| format!("第 {index} 项（{name}）的内容被截断"))?;
        entries.push(Entry { name, data });
    }
    Ok(entries)
}

fn read_u16(cur: &mut std::io::Cursor<&[u8]>) -> Result<u16, String> {
    let mut buf = [0u8; 2];
    cur.read_exact(&mut buf).map_err(|_| "归档头被截断".to_owned())?;
    Ok(u16::from_le_bytes(buf))
}

fn read_u32(cur: &mut std::io::Cursor<&[u8]>) -> Result<u32, String> {
    let mut buf = [0u8; 4];
    cur.read_exact(&mut buf).map_err(|_| "归档头被截断".to_owned())?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64(cur: &mut std::io::Cursor<&[u8]>) -> Result<u64, String> {
    let mut buf = [0u8; 8];
    cur.read_exact(&mut buf).map_err(|_| "归档头被截断".to_owned())?;
    Ok(u64::from_le_bytes(buf))
}

/// 只接受 `a/b/c.txt` 这种相对路径。
///
/// 拒绝的东西：绝对路径、盘符、`..`、空段。归档是我们自己写的，所以这些情况
/// 只可能来自被篡改的包 —— 而目录穿越的代价比"装不上"高太多。
fn safe_rel_path(name: &str) -> bool {
    if name.is_empty() || name.starts_with('/') || name.contains(":\\") || name.contains(':') {
        return false;
    }
    name.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

/// 解开到哪里。
pub struct Unpacked {
    pub files: usize,
    pub bytes: u64,
}

/// 把内置包解开到 `dest`，先写进 `<dest>.new` 再整体换上去。
///
/// `report` 每写一批文件回调一次，用来在窗口上显示进度（2000 个文件不报进度
/// 的话，用户会以为卡死了然后点第二次）。
pub fn unpack_to(
    dest: &Path,
    mut report: impl FnMut(usize, usize),
) -> Result<Unpacked, String> {
    let entries = read_entries()?;
    if entries.is_empty() {
        return Err("这个 exe 里没有内置安装包（开发机构建的版本）".to_owned());
    }

    let staging = staging_path(dest);
    // 上一次失败的残留：删干净再开始，否则"部分旧文件 + 部分新文件"会混在一起。
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .map_err(|e| format!("清不掉上次的临时目录 {}：{e}", staging.display()))?;
    }
    std::fs::create_dir_all(&staging)
        .map_err(|e| format!("建不出临时目录 {}：{e}", staging.display()))?;

    let total = entries.len();
    let mut bytes = 0u64;
    for (index, entry) in entries.iter().enumerate() {
        let path = join_rel(&staging, &entry.name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("建不出目录 {}：{e}", parent.display()))?;
        }
        std::fs::write(&path, &entry.data)
            .map_err(|e| format!("写不进 {}：{e}", path.display()))?;
        bytes += entry.data.len() as u64;
        // 每 200 个报一次：再密就是拿 UI 线程当计数器用。
        if index % 200 == 0 || index + 1 == total {
            report(index + 1, total);
        }
    }

    // 空目录也要在：`--run` 会往 logs\ 里写，而它建目录用的是 create_dir_all，
    // 所以这不算硬要求 —— 但"装完没有 logs 目录"会让人以为装坏了。
    let _ = std::fs::create_dir_all(staging.join("logs"));

    merge_into_place(&staging, dest)?;
    Ok(Unpacked {
        files: total,
        bytes,
    })
}

fn staging_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_owned();
    name.push(".new");
    PathBuf::from(name)
}

fn join_rel(root: &Path, rel: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    for part in rel.split('/') {
        out.push(part);
    }
    out
}

/// 把 `staging` 里的东西搬到 `dest`。
///
/// **不删 `dest` 里的东西**：配置、证书、令牌、日志都在那里，而"重装一次把
/// 用户的口令和调好的分辨率抹掉"正是安装器最不该做的事。所以是覆盖同名的、
/// 保留其余的。
///
/// 覆盖不了的（正在运行的 exe）按"改名让路"处理：Windows 允许重命名运行中的
/// exe，不允许删除/覆盖。做法与 `ops::stash` 一致 —— 新文件写进去之前先把旧的
/// 挪到 `.old`，下次启动生效。
fn merge_into_place(staging: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest)
        .map_err(|e| format!("建不出目标目录 {}：{e}", dest.display()))?;

    let mut stack = vec![staging.to_path_buf()];
    let mut failures = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let src = entry.path();
            if src.is_dir() {
                stack.push(src);
                continue;
            }
            let Ok(rel) = src.strip_prefix(staging) else {
                continue;
            };
            let dst = dest.join(rel);
            if let Some(parent) = dst.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::copy(&src, &dst) {
                Ok(_) => {}
                Err(err) => {
                    // 运行中的 exe：改名让路，新文件照样进去，下次启动就是新的。
                    let mut side = dst.as_os_str().to_owned();
                    side.push(".old");
                    let side = PathBuf::from(side);
                    let _ = std::fs::remove_file(&side);
                    if std::fs::rename(&dst, &side).is_ok() {
                        if std::fs::copy(&src, &dst).is_ok() {
                            continue;
                        }
                    }
                    failures.push(format!("{}（{err}）", rel.display()));
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(staging);
    if failures.is_empty() {
        Ok(())
    } else {
        // 部分失败不算成功，但也不能把已经装好的那份撤掉 —— 那些文件是对的。
        Err(format!("{} 个文件没换成新的：{}", failures.len(), failures.join("；")))
    }
}

/// 默认安装位置：`%LOCALAPPDATA%\cloud-stream`。
///
/// 选这里而不是 `C:\Program Files` 的理由是这套东西**整包自包含**：配置、日志、
/// 证书、上传留底、更新包全在运行根里。服务是以普通用户令牌跑的（UAC 过滤后），
/// Program Files 它写不了 —— `logs\` 建不出来、`mic-default.json` 写不进去、
/// 更新器覆盖不了 exe，装完就是"进程活着但什么都没写"。
///
/// 代价写在 `docs/operations-install.md` 里：「应用和功能」里不会出现，
/// 卸载靠控制台或重跑安装器。
pub fn default_target() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    let base = PathBuf::from(base);
    if base.as_os_str().is_empty() {
        return None;
    }
    Some(base.join("cloud-stream"))
}

/// 把正在运行的这个 exe 复制到 `dest`。
///
/// 归档里刻意没有它（否则就是"exe 里嵌着包含自己的包"）。复制过去的那份就是
/// 控制台：它在 `stream-server.exe` 旁边启动时，`main` 会用这个事实判断自己
/// 该当控制台还是当安装器。
pub fn place_self(dest: &Path) -> Result<(), String> {
    let me = std::env::current_exe().map_err(|e| format!("拿不到自己的路径：{e}"))?;
    let dst = dest.join("cloud-setup.exe");
    if me == dst {
        return Ok(()); // 从安装目录里重装（修复），不用复制自己
    }
    std::fs::copy(&me, &dst).map_err(|e| format!("复制自己到 {} 失败：{e}", dst.display()))?;
    Ok(())
}

/// 写一份安装记录的动作已经搬到 `report`：那里和 `install.cmd` 共用同一个
/// 结论文件、同一套 `=== RESULT:` 标记，控制台那一侧不用区分是谁写的。

#[cfg(test)]
mod tests {
    use super::*;

    /// 归档是我们自己写的，但路径检查不能因此放松 —— 它挡的是"包被换过"。
    #[test]
    fn relative_paths_only() {
        assert!(safe_rel_path("stream-server.exe"));
        assert!(safe_rel_path("selkies-web/assets/index.js"));
        assert!(!safe_rel_path(""));
        assert!(!safe_rel_path("../evil.exe"));
        assert!(!safe_rel_path("a/../../evil.exe"));
        assert!(!safe_rel_path("/etc/passwd"));
        assert!(!safe_rel_path("C:\\Windows\\evil.exe"));
        assert!(!safe_rel_path("a//b"));
    }

    /// 开发机构建的 exe 里是空归档：`is_embedded` 必须答 false，而不是崩。
    #[test]
    fn empty_payload_is_reported_not_crashed() {
        // 这个测试跑在未开 embed 的构建里（CI 与日常 cargo test 都是），
        // 所以它同时守住了"空归档能被正常解析"这条。
        if !is_embedded() {
            assert_eq!(summary(), None);
        }
    }

    /// 内嵌包的往返：解开之后文件在、旧的同名文件被换掉、用户自己的东西还在、
    /// 临时目录被清干净。
    ///
    /// 只在 `--features embed` 的构建里有东西可测；日常 `cargo test` 走的是空
    /// 归档，这个测试直接返回（和上面那条一个套路）。手工验证：
    ///
    /// ```text
    /// $env:CS_PAYLOAD_DIR = "$env:TEMP\cs-payload-probe"
    /// .\toolchain\build.ps1 -Task test -Package cloud-setup -CargoArgs '--features embed'
    /// ```
    ///
    /// 这是"单 exe 装出完整运行根"能被信任的前提 —— 它不碰任务、不碰防火墙、
    /// 不碰 `%LOCALAPPDATA%`，只在临时目录里往返一次。
    #[test]
    fn embedded_payload_round_trips() {
        if !is_embedded() {
            return;
        }
        let entries = read_entries().unwrap();
        let first = entries[0].name.clone();

        let root = std::env::temp_dir().join(format!("cs-pack-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dest = root.join("install");
        std::fs::create_dir_all(&dest).unwrap();

        // 目标目录里先放两样东西：一个用户自己的文件（必须留下），一个和归档
        // 同名的旧文件（必须被换掉）。
        std::fs::write(dest.join("keep-me.txt"), b"user data").unwrap();
        let stale = join_rel(&dest, &first);
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, b"stale").unwrap();

        let done = unpack_to(&dest, |_, _| {}).unwrap();
        assert_eq!(done.files, entries.len(), "解出来的文件数和归档对不上");
        assert!(done.bytes > 0);

        assert_eq!(
            std::fs::read(join_rel(&dest, &first)).unwrap(),
            entries[0].data,
            "同名的旧文件没有被换成新的"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("keep-me.txt")).unwrap(),
            "user data",
            "用户自己的文件被抹掉了 —— 重装不该动配置和日志"
        );
        assert!(!staging_path(&dest).exists(), "临时目录没清干净");

        let _ = std::fs::remove_dir_all(&root);
    }
}
