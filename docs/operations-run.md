# 服务怎么被拉起来：`--run`

## 1. 这一层原来是什么

`stream-server.exe` 不能自己"知道"该读哪份配置、前端在哪、日志写哪。这些信息
过去由 `start-stream.cmd`（191 行）拼成命令行，再由 `run-hidden.vbs`（28 行）
把它藏起来。

那 219 行里真正属于业务的大约十几行，其余都在补偿 cmd 的两个先天缺陷：

| 缺陷 | 补偿方式 | 行数占比 |
|---|---|---:|
| 没有表达式，`if not exist … goto` 占三行；`%ARGS%` 在括号块里只展开一次 | 全部拼接写成标签跳转 | ~70% |
| console 子系统的程序被计划任务拉起时，Windows 会代它创建一个控制台窗口 | `run-hidden.vbs` 用 `WScript.Shell.Run` 带窗口模式 0 | 100%（那 28 行全部） |

第二条是那个窗口的全部来由：任务每分钟触发一次，于是每分钟闪一个黑框，并抢走
全屏游戏的前台焦点。

## 2. 现在是什么

```bat
stream-server.exe --run
```

参数由 exe 从**自身目录**推导，然后**展开成一张普通命令行**交给 `app::run`
—— 不是另一条启动路径，只是参数来源换了个人。配置解析、日志、绑定、采集循环
一行都没改，所以不存在"脚本启动和服务启动行为不同"这类只在一边复现的问题。

| 推导项 | 来源 | 缺了怎么办 |
|---|---|---|
| `--config` | `stream-config.toml`，其次 `config.toml` | **拒绝启动**（退出码 2） |
| `--dist` | `<根>\selkies-web` | 照常启动，`watchdog.log` 记一行（页面会 404） |
| `--log` | `<根>\logs\stream-server.log` | 建不出目录就拒绝启动 |
| `--tls-cert` / `--tls-key` | `<根>\cert.pem` + `key.pem` | 走明文 HTTP，记一行（剪贴板同步关闭） |
| `--upload-dir` | `%USERPROFILE%\Desktop`，没有则 `%USERPROFILE%` | 不起就跳过 |
| `--auth-token` | `<根>\auth-token.txt` 的第一行**非空**内容 | 没有令牌，记一行（`/api/*` 对能连上端口的人开放） |

**配置缺了为什么拒绝启动。** 缺了它服务照样能起 —— 用 1080p30 的默认值，看起
来一切正常，却把每一处调过的值都忽略了。那正是这套拆分最想消灭的故障：**静默**
地不用你的配置。所以这里退出，并且说清缺的是哪几份文件。

**追加参数排在末尾。**

```bat
:: 帧率临时提到 60，其余仍按 stream-config.toml
stream-server.exe --run --fps 60
```

这就是原来那个 `start-stream.60fps.cmd`。优先级规则一天都没变：默认值 <
配置文件 < 命令行。

## 3. GUI 子系统与三种 stdio 处境

`stream-server.exe` 现在是 **GUI 子系统**（`main.rs` 的
`#![cfg_attr(windows, windows_subsystem = "windows")]`）。这是"任务直接跑 exe
不闪黑框"的全部依据 —— console 子系统的进程在无控制台场合会被系统代建窗口，
GUI 子系统的进程不会。

代价是启动时标准句柄无效，而 Rust 的 `println!` 写失败会 **panic**（症状：服务
在某句日志处凭空消失，日志里什么都没有）。`boot::prepare` 分三种处境处理：

| 谁拉起的 | 标准句柄 | 处理 |
|---|---|---|
| 计划任务（无控制台、无重定向） | 无效 | 三个句柄指到 `NUL`：`println!` 变成成功的空操作 |
| 从 cmd / PowerShell 手动跑 | 无效（GUI 进程不继承控制台） | `AttachConsole` 接上父控制台，输出照常 |
| 被 `Command::output()` 用管道抓 | **有效** | 一个字都不动 |

第三行是 `--version json` 能被更新器读到的全部原因。无条件 `AttachConsole` 会
把那条管道顶掉，更新器就永远读不到版本 —— 而它的失败方式是"报 0.0.0"，看起来
像版本号坏了。

`cloud-setup.exe` 同样是 GUI 子系统：它没有一行 `println!`，挂 console 的代价
却是双击后一个黑框陪到最后。

## 4. 单实例

命名互斥量 `Local\cloud-stream-server-instance`。第二个实例直接 `exit 0`，只在
`watchdog.log` 留一行"已有一个实例在跑"。

- **`Local\` 而不是 `Global\`**：同一台机器上两个用户各跑一份是允许的（不同端
  口、不同目录），`Global\` 会把它们互相判成重复。
- **比 `tasklist` 按映像名匹配准**：两个不同目录的同名 exe 会被 `tasklist` 认
  成同一个，于是第二个永远起不来。
- **建不出互斥量时按"没有别的实例"继续**：真正防止重复绑端口的是 bind 自己，
  这里只是让"每分钟触发一次"不变成每分钟一条端口占用的错误日志。

#81 的自愈设计不变：服务活着时任务保持 Running，后来的触发被"不启动新实例"
策略挡掉；服务死了，下一次触发才会真正往下走。

## 5. 日志分两处

| 文件 | 写什么 | 谁写 |
|---|---|---|
| `logs\watchdog.log` | 起没起来、为什么没起来、这次推导出了什么 | `run.rs`、`boot.rs`（panic）、`app.rs`（配置错） |
| `logs\stream-server.log` | 起来之后发生了什么 | 服务自己（flexi_logger） |
| `logs\crash.log` | panic 的现场 | `boot.rs` 的 panic hook |

分开的理由：任务每分钟触发一次，把"已有一个实例在跑"混进服务日志会把真正的
内容冲成噪声。而 `crash.log` 单独一份是因为 GUI 子系统下 panic 的默认输出指向
`NUL` —— 不自己写一份，崩溃就真的不留痕迹。

## 6. 退出码

| 码 | 含义 | 谁会看到 |
|---:|---|---|
| 0 | 正常退出；或"已有实例在跑" | 任务调度器（不需要干预） |
| 2 | 推不出路径 / 缺配置 / 配置解析失败 | `watchdog.log` 的最后一行 |

配置错用 2 与运行期失败（端口被占）分开，是为了让"要不要重试"这个判断有依据：
前者只能人改文件，看门狗把机器重启一万次也不会好。

## 7. 与脚本的对应关系

| 原来 | 现在 |
|---|---|
| `start-stream.cmd` | `stream-server.exe --run` |
| `start-stream.60fps.cmd` | `--run --fps 60` |
| `run-hidden.vbs` | 删除（GUI 子系统） |
| `probe-listen.cmd`（30 秒后探端口） | `stream-server.exe --probe`，由 `--run` 自己 spawn |

`--probe` 仍然是个**独立进程**，理由和原来一样：`--run` 马上就要阻塞在服务
循环上（那是看门狗机制的前提），没有线程可以拿去轮询端口。区别只是它现在是
同一份二进制的一个参数，而不是一个要跟着包走的 `.cmd`。

端口由 `--probe` 自己用同一套 `Config::parse` 算出来，不是从 `--run` 传进去的
数字：`--run --port 9000` 这种覆盖必须被探测进程看见，否则它会在 8080 上探一个
根本没人监听的端口，然后报「没起来」。

## 8. 验证记录

| 场景 | 预期 | 实测 |
|---|---|---|
| 管道捕获 `--version json` | 输出完整 JSON | ✅（GUI 子系统下仍然工作） |
| 缺配置文件 | 退出码 2，`watchdog.log` 说明缺哪几份 | ✅ |
| 完整 bundle | 起来并读到 `stream-config.toml` 的端口/分辨率/帧率 | ✅（18099 / 320×240@1fps） |
| 无证书 | 明文 HTTP，`watchdog.log` 记一行 | ✅ |
| 有 `auth-token.txt` | `--auth-token` 生效 | ✅（日志显示 token REQUIRED） |
| 第二次 `--run` | 退出码 0，`watchdog.log` 记"已有一个实例在跑" | ✅ |
| 配置里拼错键 | 退出码 2，`watchdog.log` 有 TOML 行号与可接受键名 | ✅ |
| 双击 `cloud-setup.exe` | 只有窗口，没有黑框 | ✅（子系统 = 2） |

**未验证**：真的从计划任务拉起（需要注册任务，那是 `--install` 那一步的事）；
`.150` 全天离线，实机验收未做。
