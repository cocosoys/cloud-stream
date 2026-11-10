# 参与开发

面向第一次碰这个仓库的人。目标：**克隆下来 30 分钟内能编过、跑起来、知道该改哪里。**

先读 [`docs/architecture.md`](docs/architecture.md)（模块边界与依赖规则），再回来看这里。

---

## 1. 环境

只支持 **Windows**。DXGI 桌面复制、WASAPI、`SendInput` 都是 Windows API，没有跨平台计划。

| 依赖 | 版本 / 说明 | 检查方式 |
|---|---|---|
| Rust | **1.75** + `x86_64-pc-windows-msvc` 目标 | `rustc -V` |
| VS 2022 Build Tools | 勾「使用 C++ 的桌面开发」，含 Windows SDK | `toolchain/build.ps1` 会打印探到的版本 |
| vcpkg | 三元组 `x64-windows-static`，需要 `libyuv`、`libvpx[realtime]` | `%VCPKG_ROOT%` |
| LLVM | `bindgen` 需要 libclang | `%LIBCLANG_PATH%` |
| libopus | 静态库，cmake 自建（见 `crates/stream-server/Cargo.toml:67`） | `%LIBOPUS_LIB_DIR%` 下有 `opus.lib` |

装 vcpkg 的两个库：

```powershell
toolchain\install-vcpkg-libs.cmd libyuv
toolchain\install-vcpkg-libs.cmd "libvpx[realtime]"
```

> 那三个环境变量如果没设，`build.ps1` 会去常见位置探测（`C:\dev\vcpkg`、`C:\dev\llvm\bin`、`C:\dev\opus-static\lib`）。探不到会打 warning 并说明缺了会怎样。**换机器时优先设环境变量，不要去改脚本里写死的路径** —— 以前 `install-vcpkg-libs.cmd` 和 `build-ws.cmd` 就是写死的，这是别人克隆下来编不过的头号原因。

## 2. 构建：用脚本，别直接敲 cargo

```powershell
.\toolchain\build.ps1 -Task check                    # 不链接，最快
.\toolchain\build.ps1 -Task build                    # 出二进制
.\toolchain\build.ps1 -Task test -CargoArgs '--lib'  # 单元测试
.\toolchain\build.ps1 -Task clippy                   # -D warnings
```

为什么要这层壳，两个真实的坑：

| 环境 | 直接 cargo 的现象 | 原因 |
|---|---|---|
| Git Bash | `link: missing operand after '\377\376'` | PATH 里的 `link` 命中了 PortableGit 的 **GNU coreutils `link`**，不是 MSVC 的 `link.exe`。`\377\376` 是 UTF-16 BOM，跟你的代码没关系 |
| 裸 PowerShell | `linker link.exe not found` | MSVC 环境没导入 |

`cargo check` 不链接，所以它会**全绿**，然后 `cargo build` 立刻倒下 —— 这是最容易让人误判"代码有 bug"的地方。

脚本做的事：vswhere + 目录扫描定位 VS/SDK → 拼 `PATH`/`LIB`/`INCLUDE` → 探测三个原生依赖 → 转手给 cargo。行为和"x64 Native Tools Command Prompt"里手敲一致。

首次全量编译内存峰值较高，2 vCPU / 8 GB 的机器上加 `-CargoArgs '-j','2'`。

## 3. 测试

```powershell
.\toolchain\build.ps1 -Task test -CargoArgs '--lib'
```

当前基线：**336 passed, 1 ignored**，约 3 秒。

- 新逻辑要有测试。纯 Win32 调用包在 `#[cfg(windows)]` 里，或者抽出一个可测的纯函数（`pipeline::qos`、`security::token` 都是这么做的）。
- 涉及真实设备的路径（采集、麦克风、手柄）不要写需要硬件的测试，写成"决议函数 + 输入输出"的形式。
- **端到端路径目前没有自动断言**（见 `docs/architecture.md` 欠账 #1）。改动跨模块时要手动验一遍：起服务 → 开页面 → 看首帧 → 收尾。

## 4. 提交

格式沿用仓库历史：**conventional commits + 中文正文**。

```
fix(input): 落点按实测画面矩形换算，幽灵指针改除（#118）

两条根因，表现都是"指针整体错开、反复出现"：

一、`_sinkBox()` 前面挂了一道门禁 …（为什么错、错在哪、怎么发现的）
二、`_ghostMove()` 把服务端像素增量乘了 `_pointerScale()`，应该除以它 …

改法：… （每个改动的理由，不是改了什么的流水账）
```

要求：

- **标题**：`type(scope): 中文摘要`，`type` 用 `feat`/`fix`/`refactor`/`docs`/`chore`/`build`/`perf`。有 issue 号带上。
- **正文**：写**为什么**和**取舍**。这个仓库的注释密度偏高，是因为绝大多数改动的价值在"为什么不是另一种做法"上，不在代码本身。正文写不出这个，说明改动可能不该做。
- **一次一件事**。同一个文件里两个主题，用 `git add -p` 拆开。用户可见行为变化和内部重构不要混在一笔里 —— 出问题时没法只回滚一半。
- **不 push 到 main**。开分支，PR 里说明验证方式。

### 版本号什么时候动

`crates/stream-server/Cargo.toml` 的 `version` 是**唯一**的版本来源：
`--print-config`、`/api/v1/health`、`/api/v1/version`、exe 属性页的 VERSIONINFO
全部从它来，`build.rs` 里一个字节都不重复写。

| 改了什么 | 动哪一位 | 例 |
|---|---|---|
| 新增接口、新增配置键、用户可见行为变化 | 次版本 | `0.2.0` → `0.3.0` |
| 只修 bug | 补丁 | `0.2.0` → `0.2.1` |
| 配置文件格式不兼容 | 主版本 | `0.x` 阶段也照做 |

在同一个提交里带上版本号，不要攒着。更新器靠它判断"要不要升级"，两个不同的
构建共用一个版本号等于把灰度与回滚的锚点丢了。`commit` / `dirty` / `built_at`
由 `build.rs` 自动烧进去，不用手填；部署链可以用 `CS_GIT_COMMIT` /
`CS_GIT_DIRTY` / `CS_BUILT_AT` 环境变量覆盖（CI 里通常没有 `.git`）。

## 5. 改东西之前先看这几处

| 你要动的 | 必须同步改 | 为什么 |
|---|---|---|
| 加 HTTP 路由 / WS 端点 | `docs/listener-surface.md` + `security::EventKind` | 漏插桩的入口会照常工作，这是鉴权骨架最难发现的地方 |
| 加 / 改命令行参数 | `docs/host-knobs.md`、`README` 的运行章节 | 用户唯一能查的地方 |
| 改模块归属、加模块 | `docs/architecture.md` 第 2、4 节 | 边界文档过期比没有更糟 |
| 改采集或编码后端 | `docs/stage1-pipeline-loop.md`、`docs/codec-choice.md` | 背压与降档的约束写在里面 |
| 前端能看到的字段 | 另一个仓 `selkies-webclient` | 两边 wire 名对不上，面板选了不生效 |

## 6. 几条硬规矩

1. **启动副作用只能在 `app::run` 里**。装鉴权策略、切默认录音设备、探测编码器，全在那一处，且**顺序即契约**（策略必须在 bind 之前装好）。
2. **`security` 保持是叶子**。它现在不引用任何其他模块，不要打破。
3. **`vendor/` 下是 fork**（`hbb_common` / `scrap` / `base` / `vigem`）。能不动就不动；必须动时在提交正文里写清"上游哪一行、为什么非改不可、后续怎么跟上游对齐"。
4. **失败要能降级，不要拉着整个服务一起死**。麦克风不可用、手柄 DLL 缺失、HDR 不支持 —— 这些都该是一条 warn，不是启动失败。这是本项目的既有取向，别改。
5. **不要为了"整齐"重排大文件**。32k 行里每一处 `pub` 的可见性都牵动别人，纯格式化的 diff 会和真实改动混在一起，审不动。

## 7. 现在最值得做的三件事

如果你不知道从哪下手，这三项的收益最高（对应 `docs/architecture.md` 第 7 节）：

- [ ] **起一个集成测试**：`tests/` 下启动服务 → 连 WS → 收到第一帧 → 收尾。它会立刻变成最有价值的一个测试。
- [ ] **给 `config` 解耦**：它现在反向依赖 `security`/`dispatch`/`platform`，加个参数要 import 三个模块。
- [ ] **`pub` 收口**：`lib.rs` 加进来之后，模块内所有 `pub` 在语义上都成了对外 API。按模块推进，一次一个提交，每次跑全测。
