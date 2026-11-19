//! 云游戏客户端 → Minecraft 服务端的「我在线」上报。
//!
//! # 这件事为什么在客户端做
//!
//! Minecraft 那侧本来就有 90s 心跳去 `GET /api/v1/health` 拉状态，不依赖本模块。
//! 启动上报多做的是**把客户端自己知道、服务端猜不到的东西先送过去**：听得见的
//! 端口、进程版本、机器名、可连地址。心跳只能验证"这个地址通不通"，遇到服务端
//! 里根本没登记过这台机器（新机器、换了 IP）时，只能等管理员手填。
//!
//! # 为什么不用 reqwest
//!
//! 需求只有一个 POST：请求体几十字节，响应体我们不读。为它引入一整套 TLS 栈和
//! 异步 HTTP 客户端，会让部署包的依赖面和升级面都变大。这里用阻塞
//! `TcpStream` 手写 HTTP/1.1，跑在独立线程里 —— 失败就重试，绝不能拖住启动。
//!
//! 只支持 `http://`：Minecraft 那侧的 SOYS 网关默认就是明文 HTTP（25574），
//! `https://` 在这里会直接报"不支持的 scheme"，而不是悄悄降级成明文。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::config::Config;

/// 单次请求的连接/读写超时。
const IO_TIMEOUT: Duration = Duration::from_secs(3);
/// 首报最多尝试几次（服务端可能还在启动，端口刚绑定）。
const RETRIES: usize = 3;
/// 两次尝试之间的间隔。
const BACKOFF: Duration = Duration::from_secs(2);

/// 起一个后台线程做启动上报。任何失败都只写日志，不影响推流。
pub fn spawn(cfg: &Config) {
    if !cfg.mc_report {
        return;
    }
    let url = match cfg.mc_report_url.as_deref() {
        Some(u) if !u.trim().is_empty() => u.trim().to_owned(),
        _ => {
            log::info!("mc_report 开着但 mc_report_url 为空，跳过启动上报。");
            return;
        }
    };
    // 上报密钥留空时退到 auth_token：两者默认同值，服务端按 TOFU 记住首报值。
    let key = cfg
        .mc_report_key
        .clone()
        .or_else(|| cfg.auth_token.clone())
        .unwrap_or_default();
    if key.trim().is_empty() {
        log::warn!("mc_report_key 与 auth_token 都是空，服务端会拒绝无密钥的登记，跳过上报。");
        return;
    }
    let ip = cfg
        .mc_report_ip
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| local_ipv4(&url));
    let port = cfg.port;
    let version = env!("CARGO_PKG_VERSION").to_owned();
    let name = host_name();
    // 配了证书就告诉服务端"我这边是 https"：ERP 的入口地址和心跳探针都据此选协议。
    let tls = cfg.tls_cert.is_some() && cfg.tls_key.is_some();

    let spawned = std::thread::Builder::new()
        .name("mc-report".to_owned())
        .spawn(move || run(&url, &key, &name, &ip, port, &version, tls));
    if let Err(err) = spawned {
        log::warn!("启动上报线程起不来：{err}");
    }
}

fn run(url: &str, key: &str, name: &str, ip: &str, port: u16, version: &str, tls: bool) {
    // 不带 mcVersion：那个字段在服务端是"这台机器承载的 Minecraft 版本"，
    // 由管理员填；客户端只知道自己的版本号，拿它去覆盖会把 ERP 的版本列写脏。
    let body = serde_json::json!({
        "name": name,
        "ip": ip,
        "port": port,
        "version": version,
        "tls": tls,
        "client": "cloud-stream",
    })
    .to_string();

    for attempt in 1..=RETRIES {
        match post_json(url, key, &body) {
            Ok(status) if (200..300).contains(&status) => {
                log::info!(
                    "启动上报成功：{url} -> HTTP {status}（{name} {ip}:{port} v{version}）"
                );
                return;
            }
            Ok(status) => log::warn!("启动上报被拒：{url} -> HTTP {status}（第 {attempt}/{RETRIES} 次）"),
            Err(err) => log::warn!("启动上报失败：{url} -> {err}（第 {attempt}/{RETRIES} 次）"),
        }
        if attempt < RETRIES {
            std::thread::sleep(BACKOFF);
        }
    }
    log::warn!("启动上报放弃：{url}。服务端 90s 心跳仍可在设备已登记时纠正状态。");
}

/// 手写一次 HTTP/1.1 POST，返回状态码。
fn post_json(url: &str, key: &str, body: &str) -> Result<u16, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("只支持 http:// 上报地址，收到 {url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(format!("上报地址缺少主机部分：{url}"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| format!("端口不合法：{p}"))?),
        None => (authority, 80),
    };

    let mut stream = TcpStream::connect((host, port)).map_err(|e| format!("连接 {host}:{port} 失败：{e}"))?;
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|e| format!("设置读超时失败：{e}"))?;
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|e| format!("设置写超时失败：{e}"))?;

    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {authority}\r\n\
         Content-Type: application/json\r\n\
         X-Cloud-Key: {key}\r\n\
         User-Agent: cloud-stream/{version}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n{body}",
        version = env!("CARGO_PKG_VERSION"),
        len = body.len(),
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("发送失败：{e}"))?;
    stream.flush().ok();

    let mut buf = [0u8; 512];
    let n = stream.read(&mut buf).map_err(|e| format!("读取响应失败：{e}"))?;
    if n == 0 {
        return Err("对端没有返回任何内容".to_owned());
    }
    let head = String::from_utf8_lossy(&buf[..n]);
    let status_line = head.lines().next().unwrap_or("");
    // "HTTP/1.1 200 OK"
    let code = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("响应行无法解析：{status_line:?}"))?
        .parse::<u16>()
        .map_err(|_| format!("状态码不是数字：{status_line:?}"))?;
    Ok(code)
}

/// 机器名：Windows 用 `COMPUTERNAME`，其余退回 `HOSTNAME`。
fn host_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "cloud-stream".to_owned())
}

/// 自动探测局域网 IPv4。
///
/// 监听 `0.0.0.0` 时 `cfg.host` 不是可连地址，而"随便挑一块网卡"在多网卡机器上
/// 经常挑到 VPN。这里用一次不发包的 UDP `connect`：由系统按路由表选出口网卡，
/// 拿到的就是"从本机发出去时用的那个地址"，通常也正是 MC 那侧能连回来的那个。
///
/// 目标选**上报地址里的主机**（也就是 MC 服务器），不是 `127.0.0.1`：连回环地址
/// 时系统必然选回环网卡，探出来永远是 `127.0.0.1` —— 那个地址报给 MC 之后既不能
/// 代表这台机器，心跳也永远打不通。
fn local_ipv4(url: &str) -> String {
    let fallback = "127.0.0.1".to_owned();
    let authority = match url.strip_prefix("http://").and_then(|rest| rest.split('/').next()) {
        Some(a) if !a.is_empty() => a,
        _ => return fallback,
    };
    // 去掉端口；IPv6 的 `[::1]:8080` 这里不展开（上报地址按运维惯例写 IPv4/域名）。
    let host = match authority.rsplit_once(':') {
        Some((h, _)) if !authority.starts_with('[') => h,
        _ => authority,
    };
    let probe_host = if host.is_empty() { "127.0.0.1" } else { host };
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|sock| {
            // 端口 9（discard）不需要有服务在听：UDP 的 connect 只查路由，不发包。
            sock.connect((probe_host, 9))?;
            sock.local_addr()
        })
        .map(|addr| addr.ip().to_string())
        .unwrap_or(fallback)
}
