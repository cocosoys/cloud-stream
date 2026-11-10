//! stream-server 的库入口。
//!
//! 这个 crate 直到 #123 都只有 `main.rs`，八个领域模块全是私有的 `mod`。后果
//! 不是"代码不好看"，而是三条硬性的路被堵死：
//!
//! 1. **集成测试写不了。** `tests/` 下的用例是独立的 crate，只能依赖 lib。没有
//!    lib 时它们只能 `include!` 源文件，或者干脆不测 —— 所以端到端路径（启动 →
//!    信令 → 推流）到今天为止一条自动断言都没有，337 个测试全在单元层。
//! 2. **外部无法复用。** 想写一个工具去驱动服务、或者把采集管线接到别的传输上，
//!    只能复制源码。
//! 3. **文档生成不了。** `cargo doc` 只跑 lib，bin 里的注释是死文本。
//!
//! # 模块边界
//!
//! 这里的 `pub mod` 是**领域边界**，不是"所有条目都对外承诺"。八个模块各自
//! 的 `pub` 条目历史上是为 crate 内互相调用而加的，加 lib 之后它们在语义上变
//! 成了对外 API —— 逐步收口成 `pub(crate)` 是后续的独立工作，不在这一个提交里
//! 混做，否则 diff 会大到没人能审。
//!
//! 依赖方向（自上而下，禁止反向）：
//!
//! ```text
//! app        装配：按固定顺序把下面几层串起来，唯一的"知道所有人"的地方
//! http       监听面：路由、鉴权、静态资源、文件传输
//! signaling  信令协议：hello/offer/answer/ICE 的文本契约
//! webrtc     媒体会话：PeerConnection、Track、DataChannel
//! pipeline   采集与编码：抓屏、编码、音频、共享模式
//! dispatch   输入与设备：键鼠注入、手柄、光标、显示控制
//! platform   平台适配：硬件编码探测、虚拟麦克风、默认录音设备
//! security   横切：鉴权策略、主体、事件
//! ops        横切：快照/回滚/校验，四个动作的共同底座（先于 config 执行）
//! update     横切：检查/覆盖/验版本，更新动作的实现（先于 config 执行）
//! configedit 横切：按键读改配置 + 校验，替代"用记事本改 TOML"（被 ops 使用）
//! config     横切：命令行解析，无下游依赖
//! ```
//!
//! `config` 与 `security` 是被所有层依赖的叶子，它们不允许反过来依赖上层。
//! 这条规则写在 `docs/architecture.md`，改动模块归属时先改文档再改代码。

pub mod app;
pub mod config;
pub mod configedit;
pub mod configfile;
pub mod dispatch;
pub mod http;
pub mod ops;
pub mod pipeline;
pub mod platform;
pub mod security;
pub mod signaling;
pub mod update;
pub mod version;
pub mod webrtc;
