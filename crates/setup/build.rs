//! 构建脚本：把清单和（可选的）安装包塞进 exe。
//!
//! 两件事，各自独立：
//!
//! 1. **清单** —— Per-Monitor DPI 只能靠 PE 资源里的清单声明，见下面注释。
//! 2. **payload** —— `--features embed` 时把 `target/output/cloud-stream` 打成
//!    一个 gzip 归档，用 `include_bytes!` 嵌进 exe，于是分发物是**一个文件**。
//!
//! # 为什么归档里没有 cloud-setup.exe 自己
//!
//! 那是个循环：exe 里嵌着包含自己的包，包又嵌在 exe 里。解法是安装器解压完
//! 之后把 `current_exe()` 复制到目标目录 —— 于是安装好的那份就是控制台，而
//! 归档里不需要有它。这条约定写在 `pack.rs` 与 `install.rs` 的注释里，改任何
//! 一边都要看另一边。

use std::io::Write;
use std::path::{Path, PathBuf};

fn main() {
    embed_manifest();
    embed_payload();
}

/// 把清单嵌进 exe。
///
/// Per-Monitor DPI 只能靠清单声明：进程启动时系统就按它决定"要不要替这个进程
/// 做位图缩放"，而同一个进程只认第一次设置 —— 事后调 SetProcessDpiAwareness
/// 改不回来。所以这一条不能写在代码里，必须躺在 PE 资源里。
/// `/MANIFEST:EMBED` + `/MANIFESTINPUT:` 是 MSVC 链接器自带的路，不额外引入
/// 构建依赖（比如往 PE 里塞资源的 crate）。
fn embed_manifest() {
    println!("cargo:rerun-if-changed=cloud-setup.manifest");
    // 用 `current_dir` 而不是 `canonicalize`：后者在 Windows 上返回
    // `\\?\C:\...` 这种扩展长度路径，而 link.exe 打不开它（LNK1104，看起来
    // 像"文件不存在"，其实只是前缀不一样）。cargo 跑构建脚本时当前目录就是
    // 包根，拼一下就是绝对路径。
    let dir = std::env::current_dir().expect("拿不到当前目录");
    let manifest = dir.join("cloud-setup.manifest");
    if !manifest.exists() {
        panic!("找不到 {}", manifest.display());
    }
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    // 不加引号：rustc 会把引号原样带进链接命令行，于是 link.exe 去找一个名字
    // 里真的带引号的文件（LNK1104，报错里那对引号就是证据）。代价是路径不能
    // 有空格 —— 有的话链接会失败，而不是悄悄丢掉清单。
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
}

// ---------------------------------------------------------------------------
// payload
// ---------------------------------------------------------------------------

/// 归档魔数。四个字节 + 一个版本字节，读的时候先比这个再解析 —— 一个
/// "把别的文件当包解开"的错误比报错难查得多。
const MAGIC: &[u8; 5] = b"CSPK1";

fn embed_payload() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("没有 OUT_DIR"));
    let target = out_dir.join("payload.gz");

    // 不开 feature 时也写一个**合法的空归档**：`include_bytes!` 要求文件一定
    // 存在，而"没有 payload"是一个必须能被正常处理的状态（开发机上的
    // `cargo build` 就是这样），不是编译错误。
    if std::env::var_os("CARGO_FEATURE_EMBED").is_none() {
        write_empty(&target);
        println!("cargo:warning=cloud-setup: 未开 embed，exe 里没有安装包");
        return;
    }

    let root = payload_root();
    println!("cargo:rerun-if-changed={}", root.display());
    if !root.is_dir() {
        // 打包目录不存在时**不静默生成空包**：那会做出一个"看起来能装、装完
        // 什么都没有"的 exe。让构建失败，比让用户装出一个空目录好。
        panic!(
            "embed 需要打包目录 {}，但它不存在。先跑 deploy.ps1 -PackageOnly",
            root.display()
        );
    }

    let files = collect(&root);
    let bytes = write_pack(&files);
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(&bytes).expect("压缩失败");
    let gz = enc.finish().expect("压缩失败");
    std::fs::write(&target, &gz).expect("写 payload.gz 失败");
    println!(
        "cargo:warning=cloud-setup: 内嵌 {} 个文件，{:.1} MB 压缩后 {:.1} MB",
        files.len(),
        bytes.len() as f64 / 1e6,
        gz.len() as f64 / 1e6
    );
}

/// 打包源目录。
///
/// 优先读环境变量 `CS_PAYLOAD_DIR`：`deploy.ps1` 用它指到刚打好的
/// `target\output\cloud-stream`，这样安装器里嵌的**就是**那个包本身，不是
/// 一份另行组装的近似品。没设就按仓库布局推。
fn payload_root() -> PathBuf {
    if let Some(path) = std::env::var_os("CS_PAYLOAD_DIR") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    // crates/setup/build.rs -> ../../target/output/cloud-stream
    let here = std::env::current_dir().expect("拿不到当前目录");
    here.join("..")
        .join("..")
        .join("target")
        .join("output")
        .join("cloud-stream")
}

/// 收文件。路径一律用 `/` 分隔：归档在解压端会被拼回本机路径，而 `/` 是唯一
/// 不会和「盘符里的冒号」「UNC 里的反斜杠」打架的写法。
fn collect(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let name = rel.to_string_lossy().replace('\\', "/");
            // 安装器解压完会把自己复制进去，归档里放它只会造成"解压时覆盖正在
            // 运行的 exe"（Windows 允许改名不允许删除/覆盖运行中的 exe）。
            if name.eq_ignore_ascii_case("cloud-setup.exe") {
                continue;
            }
            // 日志不打包：那是上一台机器的现场，带到新机器上只会误导。
            if name.starts_with("logs/") {
                continue;
            }
            match std::fs::read(&path) {
                Ok(bytes) => files.push((name, bytes)),
                Err(err) => println!("cargo:warning=跳过 {}：{err}", path.display()),
            }
        }
    }
    // 排序只为让两次构建产出逐字节相同的归档（可复现，diff 才有意义）。
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

/// 归档格式：
///
/// ```text
/// "CSPK1"
/// u32  file_count
/// 每个文件：
///   u16  name_len
///   u8[] name        (UTF-8)
///   u64  size
///   u8[] bytes
/// ```
///
/// 手写而不是 tar/zip：只需要读回自己写的东西，而 `tar` crate 会带进一整套
/// 元数据语义（权限位、符号链接、长名扩展），那些在这个场景里一条都用不上。
/// 解析端在 `pack.rs`，两边**必须**一起改。
fn write_pack(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 << 20);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for (name, bytes) in files {
        let name = name.as_bytes();
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        out.extend_from_slice(bytes);
    }
    out
}

fn write_empty(target: &Path) {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&0u32.to_le_bytes());
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(&out).expect("压缩失败");
    let gz = enc.finish().expect("压缩失败");
    std::fs::write(target, &gz).expect("写 payload.gz 失败");
}
