// Stage 0 smoke check: 工具链、MSVC 链接器、依赖镜像链路一次跑通。
// 不引用 scrap / hbb_common / webrtc；任何输出异常都说明环境未就绪。

use serde::Serialize;

#[derive(Serialize)]
struct Smoke {
    cargo_pkg: &'static str,
    arch: &'static str,
    os: &'static str,
    env: String,
    note: &'static str,
}

fn main() {
    let info = Smoke {
        cargo_pkg: env!("CARGO_PKG_NAME"),
        arch: std::env::consts::ARCH,
        os: std::env::consts::OS,
        env: std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default(),
        note: "stage0 shell: toolchain + linker + crates mirror OK",
    };

    let json = serde_json::to_string_pretty(&info).expect("serialize smoke struct");
    println!("{json}");
    println!("[cloud-shell] build & link OK on {}", std::env::consts::OS);
}
