# 架构：模块边界与依赖方向

这是一份**约束文档**，不是介绍。改模块归属、加新模块、跨层调用之前先读它；改完如果边界变了，同步改它。

拉新人时先看 [`README.md`](../README.md) 的能力现状，再看这里。

---

## 1. 为什么这个 crate 有 `lib.rs`

到 #123 为止，`crates/stream-server` 只有一个 `main.rs`，八个领域模块全是私有的 `mod`。这在两个 bin 的玩具项目里没问题，在这里卡住了三件事：

| 被卡住的 | 具体后果 |
|---|---|
| 集成测试 | `tests/` 下的用例是独立 crate，只能依赖 lib。没有 lib 就没法测"启动→信令→推流"这条端到端路径，337 个测试全部停在单元层 |
| 复用 | 想写个工具驱动服务、或者把采集管线接到别的传输上，只能复制源码 |
| 文档 | `cargo doc` 只跑 lib，bin 里的模块注释是死文本 |

现在 `main.rs` 只剩三行交接，装配顺序搬到了 [`app`](../crates/stream-server/src/app.rs)。**新增 bin 时从 `main.rs` 抄三行，不要去复制 `app::run` 的片段** —— 那会让第二份启动顺序和第一份慢慢分叉。

## 2. 模块地图

| 模块 | 职责 | 行数 | 主要入口 |
|---|---|---:|---|
| `app` | 装配顺序：日志→鉴权→编码器→麦克风→监听面 | ~400 | `app::run` |
| `http` | 路由：静态资源、信令升级、健康、文件传输、WS 媒体 | 6.3k | `http::serve` |
| `signaling` | 一条浏览器 WS 会话：握手状态机、SDP/ICE 交换 | 3.6k | `websession` |
| `webrtc` | PeerConnection 装配与生命周期 | 0.9k | `MediaSession` |
| `pipeline` | 采集→编码→推流循环、音频、QoS、共享模式 | 9.8k | `pipeline::mod` 的 beat 循环 |
| `dispatch` | 上行命令路由、下行白名单、键鼠注入、手柄、光标、剪贴板 | 6.1k | `commands`、`win_input` |
| `platform` | 平台适配：硬件编码探测、虚拟麦克风、默认录音设备、主机指标 | 2.3k | `hwenc::probe`、`virtual_mic::probe` |
| `security` | 监听面鉴权骨架：事件名、主体、策略 | 1.5k | `install`、`EventKind`、`Policy` |
| `config` | 命令行解析、默认值、配置文件优先级 | 0.9k | `Config::parse` |
| `configfile` | TOML 配置：解析、覆盖、`--print-config` 回环 | 0.6k | `configfile::load`、`render` |
| `version` | 版本与构建指纹，`/api/v1/version` 的数据源 | 0.2k | `version::as_json` |
| `ops` | 快照/回滚/校验：四个动作（装/卸/更新/改）的失败兜底 | 1.0k | `ops::try_run`、`snapshot`、`rollback` |

## 3. 一次推流经过哪些模块

```
浏览器
  │  GET /            http::static           静态 bundle
  │  GET /api/.../signaling/（WS upgrade）    http::server → security::check
  ▼
http::ws_media / signaling::websession        握手状态机（hello → offer → answer → ICE）
  │
  ▼
webrtc::MediaSession                          PeerConnection、video track、两条 DataChannel
  │
  ▼
pipeline                                      采集线程 ──► 编码 ──► track.sample()
  │                                              ▲
  │                                              └── capture::CaptureWorker（独立线程阻塞在 frame()）
  ▼
scrap（vendor）                               DXGI Desktop Duplication / GDI 兜底
```

反方向（输入）走 `http::ws_media → dispatch::commands → dispatch::win_input → SendInput`。

`http` 是唯一同时碰到"外部世界"和"内部管线"的地方，这也是它 6.3k 行的原因 —— 这不是设计缺陷，是它本来就在边界上。

## 4. 依赖规则

**硬规则（新增代码必须遵守）：**

1. **`security` 是叶子。** 它目前不引用任何其他模块（`grep crate:: config|http|pipeline|... security/*.rs` 为空），不要打破。
2. **`http` 不做业务判断。** 路由、鉴权、序列化可以；帧格式、编码参数、采集状态不行 —— 那些属于 `pipeline`。
3. **`app` 之外不许有启动副作用。** 装鉴权策略、切默认录音设备、探测编码器，只有 `app::run` 能做，且顺序即契约（策略必须在 bind 之前装好，否则前几个请求会按默认放行）。
4. **跨层通信走既有类型，不要拉私有字段。** 需要新数据时先加一个 `pub` 方法或结构体，而不是把 `pub(crate)` 改成 `pub` 了事。
5. **`ops` 必须先于 `config` 执行，且不依赖它。** `--ops` 在 `app::run` 的一开头、`Config::parse` 之前被拦下。理由不是分层洁癖：最需要回滚的时刻往往是配置文件刚被改坏的那一刻，而那时 `Config::parse` 会以退出码 2 结束进程 —— 恢复入口不能依赖它要恢复的东西。任何"顺手"把 `ops` 挪到 `config` 之后的改动都是在拆这条保险。

**已知违反（欠账，见第 7 节）：**

- `config` 反向依赖了 `security`（`GrantSpec`/`TokenGate`）、`dispatch`（`ghost::DEFAULT_IDLE_SECS`、`ghost::clamped`）、`platform`（`default_mic::Policy`）。配置层本不该知道业务类型，这是历史积累。

## 5. 五个常见扩展点

### 加一个编码器

1. `pipeline::codec::VideoCodec` 加变体与 wire 名（`from_wire`/`wire`）；
2. `pipeline::encoder` 里实现编码路径。**注意 `README` 里写明的现状：VP8/VP9 软编已接，H.264/AV1 未接** —— 加硬编不是"再写一个分支"，`platform::hwenc` 的探测与决议要先支持；
3. 前端另有一份 `encoderOptions`（在 `selkies-webclient` 仓），**服务端的 wire 名必须和它对上**，否则面板选了不生效。

### 加一个采集后端

1. `vendor/scrap` 里加 `CapturerType` 变体；
2. `pipeline::capture` 的 `Capturer::new`（约 130 行）选路；
3. 如果要保留运行时切换，接到现有的 `set_gdi()` 开关上（`capture.rs:161`），它已经在 `stream_info` 里上报了。

⚠️ `scrap` 的 DXGI 路径是单适配器（`EnumAdapters1(0, ...)`）。任何"跨适配器"的需求（虚拟显示器、多显卡）都得先改这一处。

### 加一个配置项

这是最常见的改动，一共六处，漏了任何一处都有对应测试或症状把它顶出来：

1. `config.rs`：`Config` 加字段 + `Default` 加默认值 + `parse` 的 `match` 加一个 `--flag` 分支；
2. `configfile.rs`：`FileConfig` 加同名字段（`Option<T>`，需要改键名就加 `#[serde(rename)]`）；
3. `configfile.rs` `apply()`：把文件值盖到 `Config` 上；
4. `configfile.rs` `from_config()`：反向，供 `--print-config` 用；
5. `configfile.rs` `render_toml()`：加一行 `kv_xxx(...)` 和一句注释；
6. 跑 `cargo test -p stream-server --lib`。**第 5 步漏了会被 `tomf_round_trip_is_identity` 直接抓住** —— 那个测试把手写的 `render_toml` 再解析回来逐字段比对，它就是为"手写格式化和结构体悄悄分叉"准备的。

不要在两个地方实现值域校验：枚举型参数（`input_policy`、`mic_default_device`）在 `apply()` 里调用同一个 `parse()`，识别不了就收进 `warnings` 而不是报错 —— 与命令行"拼错保留默认并告警"保持一致。

### 加一个 HTTP 接口

1. `http/` 下加模块；
2. `http/server.rs` 挂路由；
3. **在 `docs/listener-surface.md` 登记**，并给 `security::EventKind` 加一个名字 —— 漏插桩的入口会照常工作，正是这个骨架最难发现的地方；
4. 除了 `Static` 和 `Status`，所有路由都要先过 `TokenGate::check_request`。

### 加一个鉴权策略

实现 `security::Policy`（`security/mod.rs:499`），在 `security/policy.rs:120` 的 `BUILTIN` 里注册，`by_name` 加分支。策略名拼错时的行为是退回 `allow-all` 并 warn，不是退出 —— 这是有意的：一个拼写错误不该变成一次停服。

## 6. 构建与测试

**不要直接在 Git Bash 或裸 PowerShell 里跑 `cargo build`。** 有两个真实的坑，都会让人以为是代码有问题：

| 环境 | 现象 | 原因 |
|---|---|---|
| Git Bash | `link: missing operand after '\377\376'` | PATH 里 `link` 命中的是 PortableGit 的 **GNU coreutils `link`**（要求"源 目标"两参数），Windows 给 `link` 补 `.exe` 后正好抢在 MSVC 前面 |
| 裸 PowerShell | `linker link.exe not found` | 没有导入 MSVC 环境 |

`cargo check` 不链接，所以它会全绿 —— 这让判断更具欺骗性。

统一入口：

```powershell
.\toolchain\build.ps1 -Task check       # 不链接，最快
.\toolchain\build.ps1 -Task build
.\toolchain\build.ps1 -Task test -CargoArgs '--lib'
```

脚本自己用 vswhere + 目录扫描定位 VS 与 Windows SDK，拼出 `PATH`/`LIB`/`INCLUDE` 再转手给 cargo。它在这台开发机上必须靠目录扫描 —— **`vswhere` 对未登记的 BuildTools 返回 `[]`**，这是常态不是异常。

测试现状：`336 passed, 1 ignored`（`cargo test -p stream-server --lib`，约 3 s）。全部是单元测试，**端到端路径仍无自动断言**，这是下一项要补的。

## 7. 模块化欠账（按优先级）

| # | 欠账 | 影响 | 建议 |
|---|---|---|---|
| 1 | 无集成测试 | 改动只能靠单元测试保，跨模块回归靠人肉 | `tests/` 起一个：启动服务 → 连 WS → 收到第一帧 → 收尾 |
| 2 | `config` 反向依赖业务层 | 配置层无法独立复用，加配置项要 import 三个模块 | 把枚举定义下沉到 `config::values`，业务模块从那里取 |
| 3 | 三个巨文件 | `websession.rs` 3188 / `pipeline/mod.rs` 2342 / `ws_media.rs` 1986 | 只拆有明确边界的（如 `websession` 的 hello/offer/ICE 三段），不要为拆而拆 |
| 4 | `pub` 未收口 | 加 lib 后，模块内所有 `pub` 在语义上都成了对外 API | 逐个改 `pub(crate)`，按模块推进，一次一个提交 |
| 5 | edition 2018 | 72 条 `panic message contains an unused formatting placeholder` 警告 | 升 2021 是独立改动，会影响 `assert!` 的 format 语义，单独做并跑全测 |
| 6 | 无 CI | 三人以上协作第一天就开始漂 | GitHub Actions：`cargo check` + `test` + `fmt --check` |
| 7 | 无 LICENSE | 别人不能合法使用/贡献 | 需要选协议，属产品决策 |

## 8. 一条容易忘的

`main.rs` 变成薄壳之后，**调试启动逻辑时断点要下在 `app::run` 里**，不在 `main`。IDE 的"Step into"会先经过三行交接再进去。
