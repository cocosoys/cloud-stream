fn main() {
    // 把清单嵌进 exe。
    //
    // Per-Monitor DPI 只能靠清单声明：进程启动时系统就按它决定"要不要替这个
    // 进程做位图缩放"，而同一个进程只认第一次设置 —— 事后调
    // SetProcessDpiAwareness 改不回来。所以这一条不能写在代码里，必须躺在
    // PE 资源里。`/MANIFEST:EMBED` + `/MANIFESTINPUT:` 是 MSVC 链接器自带的路，
    // 不额外引入构建依赖（比如往 PE 里塞资源的 crate）。
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
