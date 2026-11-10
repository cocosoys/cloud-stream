# 快照与回滚：四个动作的失败兜底

四动作标准化里，「失败可回滚」是唯一一列在改造前**格全红**的：安装是重跑一次碰运气，卸载是删了即没了，更新只在开发机上有退路，修改是手改文本无备份。

这一层补的就是那一列。它不是四个脚本各写一套备份逻辑，而是一个动作层：

```
stream-server.exe --ops <verb> [选项]
```

同一个 exe、同一份实现，被 `install.cmd`、`uninstall.cmd`、更新器、配置界面共同调用。目标机上只有 exe 是确定存在的，所以动作层放在这里，而不是再发一个 `backup.ps1`。

## 1. 为什么不放在配置层后面

`--ops` 在 `app::run` 的**第一行**执行，早于 `Config::parse`、早于日志、早于任何启动动作。

最需要回滚的时刻，通常是配置文件刚被改坏的那一刻。如果 `--ops rollback` 也要先解析配置，那份坏配置会让进程以退出码 2 立刻退出 —— 于是「用来救命的那条命令」和「把机器搞坏的那份配置」死在一起，唯一的恢复入口被自己锁掉。

直接从原始命令行读参数还有第二个好处：`--ops` 不在 `Config` 里，也就不可能被写进 `stream-config.toml`。一次性动作不进配置文件，这条规则在 `configfile` 的模块文档里已经写死了，这里只是没有给它开后门。

## 2. 六个动词

| 动词 | 做什么 | 退出码 |
|---|---|---|
| `state` | 只读汇总：运行根、版本、可退回的目标、配置文件是否在 | 0 |
| `snapshot` | 拍一份快照，标签由 `--ops-label` 给 | 0 / 3 |
| `list` | 列出全部快照，新的在前 | 0 |
| `verify` | 拿现状和某份快照比对，不写任何文件 | 0 一致 / 3 不一致 |
| `rollback` | 回滚到某份快照（默认最近一次**人主动拍的**） | 0 / 3 / 4 |
| `gc` | 只保留最近若干份（`--ops-keep`，默认 3） | 0 / 3 |

选项：

```
--ops-json            结果以 JSON 输出（供界面/脚本解析）
--ops-label <标签>     快照标签，会成为目录名的一部分
--ops-dir <目录>       备份目录，默认 <运行根>\state\backups
--ops-to <id>         指定快照（支持前缀），默认最近一次主动快照
--ops-keep <n>        gc 保留份数
--ops-paths <列表>    只备份这些相对路径（分号或逗号分隔，目录会递归）；空 = 全量
--config <路径>       仅用于 state 报告"实际读的是哪份配置"，不做解析
```

stdout 只有结果，解释性的话走 stderr —— 和 `--print-config` 同一口径，所以调用方可以放心 `| ConvertFrom-Json`。

## 3. 快照里装什么

运行根下的全部文件，递归，**除了**：

- `logs/` —— 日志记的是"过去发生过什么"，回滚它不会让机器回到过去的状态；
- `state/` —— 快照自己住在这儿，递归进去等于自己拷自己；
- `*.rollback-old` —— 上一回回滚让路留下的残骸，再把它们备进去，备份会一次比一次胖。

每份快照是一个目录：

```
state\backups\20261110T080223Z-preupdate\
    manifest.json     id / 时间 / 版本 / commit / 运行根 / 每个文件的相对路径+大小+FNV-1a 哈希
    files\            与运行根同构的文件副本
```

哈希用 FNV-1a 64 而不是 sha2：没有依赖、十几行、只用来回答「这个文件还是不是原来那个」，不需要抵抗有人构造碰撞。

## 4. 回滚怎么处理"正在运行的文件"

Windows 不允许覆盖一个正在运行的 exe，但**允许重命名**它。所以恢复的写法是"先改名让路，再拷入"：

```
stream-server.exe              ->  stream-server.exe.rollback-old
<备份>\files\stream-server.exe  ->  stream-server.exe
```

于是回滚可以在服务运行中执行，不必先停机。改名失败的（被独占的日志、被锁住的 DLL）会单独报出来，并且整体退出码是 3 —— 提示"先停服务再试"，而不是假装成功。

**多出来的文件也改名，不删除。** 更新引入的新 DLL、用户自己放进来的东西，回滚时一律让路成 `.rollback-old`。判错一次不该让人丢文件；清理是人的决定。

**回滚之前自动留底。** 每次 `rollback` 先拍一份 `pre-rollback` 快照，然后才动文件。回滚本身也会出错（目标选错、拷到一半断电），那时唯一能救命的正是刚被覆盖掉的那一份。用 `--ops-to <那个 id>` 就能退回去。

## 5. 两个容易踩反的语义

**默认目标不是"最新一份"。** 回滚一次之后，最新那份是刚才自动生成的 `pre-rollback`（内容是回滚前的状态）。如果默认取最新，连点两次回滚就等于来回翻 —— 一个"点了两次等于没点"的按钮。所以默认目标跳过自动留底，只认人主动拍的；确实要退回自动那份，用 `--ops-to` 指定。

**`gc` 不按"最近 N 份"删。** 同样因为自动留底会插在最前面：连续回滚几次后，`gc --ops-keep 1` 会删掉唯一有用的主动快照、留下谁都不想要的自动留底。现在的策略是分开算：主动快照留最新 `keep` 份，自动留底只留最新 1 份。

## 6. 四个动作各自怎么用

| 动作 | 调用点 | 做法 |
|---|---|---|
| 安装 | `install.cmd /restore` | 重装时先把上次卸载存的身份放回来（不默认执行，见下） |
| 卸载 | `uninstall.cmd` 第 5 步之前 | `--ops snapshot --ops-dir %ProgramData%\cloud-stream\uninstall-backup --ops-paths "stream-config.toml;config.toml;cert.pem;cert.key;logs\mic-default.json"` |
| 更新 | 更新器（后续） | 覆盖 exe/DLL/前端之前 `snapshot`，验证 `/api/v1/version` 不对就 `rollback` |
| 修改 | 配置界面（后续） | 写 `stream-config.toml` 之前 `snapshot`；"撤销上次修改"就是 `rollback` |

三处细节值得写下来。

**卸载的备份必须落在目录之外。** 放目录里等于没备，因为下一步就是 `rmdir /s`。

**卸载只备"身份"，不备整包。** 所以有了 `--ops-paths`：`stream-config.toml`（端口、帧率、令牌）、证书、`logs\mic-default.json`（录音设备原值）一共几 KB；把 30 多 MB 的前端 bundle 和 DLL 也搬进 `ProgramData` 是纯浪费 —— 那些东西重装时会原样再来一遍。同一台机器上实测：全量 12 个文件 33.3 MiB，`--ops-paths` 之后 1 个文件 4.5 KiB。

**显式点名时默认排除表要让路。** 全量快照排除 `logs/` 是为了省事，但卸载恰恰只要 `logs\mic-default.json`。第一次写这个过滤时就是这么错的：测试跑出来备份里一个文件都没有。现在规则是：全量用 `["logs","state"]`，显式点名用 `["state"]`（`state/` 永远不能进 —— 那里装的就是快照自己）。

**旧 exe 的能力探测。** 0.3.0 之前的 exe 不认识 `--ops`，`Config::parse` 会忽略未知开关，于是它**会把整个服务拉起来**：不返回、占住 8080、还锁着 exe，而脚本正在删这个目录。这和当初 `--restore-mic` 踩的坑是同一个。两个脚本因此都在调用前做一次 `findstr` 探测，针是 `ops snapshot failed`（ops.rs 里的 `eprintln!` 文本），**不是** `--ops` —— rustc 会把短开关字面量折成立即数比较，开关名根本不在二进制里（实测：`ops snapshot failed` 有，`--ops` 没有）。探不到就跳过备份并说明，绝不盲跑。

## 7. 验证记录（本机，2026-11-10）

调试版 exe，临时目录里放 `stream-server.exe` + `dist/index.html` + `stream-config.toml` + `logs/noise.log`：

```
$ stream-server.exe --ops state
root:      C:\...\cs-ops-e2e2
version:   0.2.0 (c987c75, dirty)
backups:   C:\...\cs-ops-e2e2\state\backups
rollback:  20261110T080448Z-before-change (2 files, 33.2 MiB)
snapshots: 2 (含自动留底)

# 改坏：改 index.html、删 stream-config.toml、加一个 leftover-new.dll
$ stream-server.exe --ops verify
verify against: 20261110T080223Z-preupdate
  ok:      1
  changed: dist/index.html
  missing: stream-config.toml
  extra:   leftover-new.dll
ops verify: 当前状态与 ... 不一致          (exit 3)

$ stream-server.exe --ops rollback
rollback: -> 20261110T080223Z-preupdate
  pre:      20261110T080232Z-pre-rollback (回滚本身也能退回去)
  restored: 2
  unchanged:1
  stashed:  1 (改名成 .rollback-old，没有删除)

$ stream-server.exe --ops verify
  ok:      3                              (exit 0)
```

另外验过三件事：

1. **连点两次回滚不会翻回去**：第二次 `restored: 0`；
2. **配置文件写坏（`port = `）时 `--ops state` 与 `--ops rollback` 照常工作**，暴露的正是分层顺序的价值；
3. **动词拼错退出码 2**，且不会悄悄回落成"起了个服务"。

单元测试 18 条（`cargo test -p stream-server --lib`，共 371 passed / 0 failed），覆盖：跳过 `logs`/`state`、标签里的路径分隔符被清洗、回滚恢复改动与被删文件、多余文件让路、回滚可再回滚、二次回滚是空操作、`verify` 三分类、`gc` 的两条保留策略、前缀匹配与歧义拒绝、外置备份目录、`--ops-paths` 的精确匹配与目录递归。

**未验证**：实机（`.150` 自 11-10 15:40 起离线）。上面的记录全部来自本机调试版 exe；release 产物与目标机行为要等机器回来后补。`install.cmd` / `uninstall.cmd` 的改动只做了静态校验（跳转目标全部可解析、纯 ASCII、CRLF 未破）和命令行级别的验证（把脚本实际会执行的 `--ops snapshot --ops-paths ...` 原样跑了一遍），**没有在真机上完整跑过安装/卸载**。
