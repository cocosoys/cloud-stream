# 更新：`--update check|apply`

更新这件事在这台机器上原本只有一种做法：拿开发机上的 `deploy.ps1` 重新推一遍。
能跑，但它要求"推的人"知道这一包是给哪台机器的、旧的那份去哪了。矩阵里
「更新」这一行的两格（**可视化**、**程序可调用**）空着的就是这部分。

现在是一条命令，或者控制台上的一个按钮：

```text
stream-server.exe --update check    只看，不动
stream-server.exe --update apply    覆盖 + 验证，失败自动回滚
```

`stream-setting.exe` 上的【更新】按钮就是这两条命令：先 `check`，有新版就问一句，
然后 `apply`。

---

## 1. 一次更新由四段组成

```text
1. 问包      包里的 stream-server.exe 自己说它是哪一版
2. 比版本    比当前这一份新，才继续（--update-force 可以跳过这道门）
3. 留底+覆盖 先拍一份完整快照，再逐个文件拷过去；锁着的改名让路
4. 验版本    再问一次"你现在是哪一版"，答不上来或答错就自动回滚
```

第 4 段是这个功能存在的理由。没有它，"更新"就只是"拷贝一组文件" —— 拷完
之后机器处于什么状态，没有人知道。有了它，最坏结果是**退回原样**。

**第 1 段用的是 `--version json`，不是读 exe 的属性页。** 因为要回答的问题
不只是"这个文件标称几版"，还有"它跑得起来吗"。一个截断的下载、一个缺 DLL
的包，属性里可能还写着版本号，但它起不来 —— 而那正是最该在覆盖之前发现
的情况。代价是"检查更新"会执行包里那个 exe：对一条本来就要把它装上并运行
的路径，这个代价是零。

---

## 2. 更新包放哪，长什么样

默认位置：

```text
%ProgramData%\cloud-stream\update\
    stream-server.exe
    stream-config.toml
    selkies-web\...
```

就是 `deploy.ps1` 打出来的那种目录形态，和安装目录同构。用 `--update-from`
可以指到任何地方。

放在 `ProgramData` 而不是"exe 旁边的 `update\`"，有三个理由：

1. **不在运行目录里。** 更新前要拍完整快照，而快照扫的是运行目录。包要是
   住在里面，每更新一次就把几十兆的安装包也备一份，回滚时还会把包一起
   "还原"（内容一样，但屏幕上看起来像回滚没生效）。
2. **不随卸载消失。** 卸载会 `rmdir /s` 整个安装目录。包留在外面，
   "卸载 → 重装"可以直接用同一份。
3. **和卸载备份同一处**（`%ProgramData%\cloud-stream\`）。运维只需要记住
   一个目录。

刻意**不支持** zip 和 URL：那需要引入解压库和一个 HTTP 客户端，而当前的
部署链本来就是"拷目录到目标机"。真要加，加的是"把 zip 解到 `update\`"这
一步，而不是改这个模块的语义。

---

## 3. 两条不能违反的规则

### 3.1 只增不删

包里有、目录里没有的文件会被加进去；目录里有、包里没有的文件**不动**。

删文件的判断只能靠一份"应该有什么"的清单，而猜错一次就是删掉用户的存档。
旧版本留下的多余 DLL 占几百 KB，代价远小于误删。

### 3.2 这台机器的身份不覆盖

下面这些文件即使包里有同名的也**不覆盖**：

```text
stream-config.toml   config.toml   auth-token.txt   cert.pem   cert.key
```

它们和机器绑定 —— 端口、口令、证书指纹、帧率。包里的那一份是模板：一辆车
的合格证不该在下一次保养时被换成新车的那张。

实测：`stream-config.toml` 里写着 `fps = 15`（4 vCPU 虚拟机），包里的模板
写着 `fps = 60`，更新之后仍然是 15。

确实要覆盖（比如证书轮换）用 `--update-overwrite-config`。

---

## 4. 锁着的文件：改名让路

Windows 不允许覆盖一个正在运行的 exe，但**允许重命名**它。所以覆盖失败的
文件先改名成 `<名字>.rollback-old`，再把新的拷进去：

```text
stream-server.exe              ->  stream-server.exe.rollback-old
<包>/stream-server.exe         ->  stream-server.exe
```

于是更新可以在**服务运行中**执行，不必先停任务。生效要等重启（写文件的是
这个进程，跑的还是旧镜像），所以 `apply` 的最后会打一行 `hint:`。

留底名字被占、又删不掉时（占着它的往往就是**正在运行的自己** —— Windows
不允许删除运行中的 exe），改成编号名字继续让路：`.rollback-old.1`、
`.rollback-old.2`……宁可多留一份文件，也要把"退回去"做成。

---

## 5. 三种验版本口径

覆盖之后要知道"换上去的到底是什么"，但包不总是带 exe：

| 情况 | 验什么 | `verify:` 输出 |
|---|---|---|
| 包里带 exe，问得出它的版本 | 装上之后必须报这个版本 | `通过` |
| 包里带 exe，问不出它的版本（`--update-force` 放行） | **至少要求它跑得起来** | `通过` / `未通过` |
| 包里没有 exe（纯前端更新） | 不验 | `跳过（包里没有 exe，不验版本）` |

第二行是踩出来的：一开始"问不出版本"和"没有 exe"共用同一个 `None`，于是
一个跑不起来的 exe 被当成"没有 exe"跳过验证、装上、报成功。**验证在最需要
它的那一刻静默失效，比没有验证更危险** —— 它给了一个假的"通过"。现在这两
种情况是两个不同的值，有测试守着。

---

## 6. 退出码

| 码 | `check` | `apply` |
|---:|---|---|
| 0 | 有新版可装 | 更新完成并验证通过 |
| 2 | 用法错（动词不认识） | 用法错 |
| 3 | 读包失败 | 更新失败（已回滚或回滚也失败） |
| 4 | 找不到更新包目录 | 找不到更新包目录 |
| 5 | 没有比当前更新的版本 | 包里的版本不比当前新（要装同一版加 `--update-force`） |

`check` 用 `5` 而不是 `0` + 一句话，是为了让调用方**不解析文本**就能分流。

---

## 7. 常用命令

```powershell
# 只看：包里是哪一版、会动哪些文件
stream-server.exe --update check

# 看机器读的版本（更新前后各跑一次）
stream-server.exe --version          # 0.4.0 (c987c75)
stream-server.exe --version json

# 演练：把计划打出来，不写任何文件
stream-server.exe --update apply --update-dry-run

# 真更新
stream-server.exe --update apply

# 重装同一版 / 降级
stream-server.exe --update apply --update-force

# 更新失败之后手工退回（自动回滚失败时用）
stream-server.exe --ops list
stream-server.exe --ops rollback --ops-to <preupdate 的 id>

# 清历史，只留最近三份
stream-server.exe --ops gc --ops-keep 3
```

---

## 8. 验证记录（2026-11-10，本机）

用两个真实构建做的端到端：包里 0.4.0、现场 0.3.0。

| 步骤 | 结果 |
|---|---|
| `--version` / `--version json` | `0.3.0 (db50d0b)+dirty`；JSON 带 version/commit/dirty/built_at |
| `--update check` | `current 0.3.0` / `available 0.4.0 有新版` / `2 覆盖 1 相同 1 保留`，exit 0 |
| `--update apply` | 快照 `…-preupdate`；exe 被占 → 留底 → 覆盖；`verify 通过`；exit 0 |
| 更新后 | `--version` 报 0.4.0；`stream-config.toml` 仍是 `fps = 15`；前端换成新包 |
| 坏包（exe 是文本文件）`check` | `available: 未知 —— 起不来（os error 216）`，exit 5 |
| 坏包 `apply`（不带 force） | 拒绝执行，提示 `--update-force`，exit 3 |
| 坏包 `apply --update-force` | 覆盖 → `verify 未通过` → **自动回滚**；exit 3；现场回到 0.4.0 + 前端 v040 |
| 回滚后的目录 | `stream-server.exe.rollback-old`、`.rollback-old.1` 都在（留底没被删） |

单元测试 22 条（`update` 模块）覆盖：版本比较（含 `0.10 > 0.9`）、增/覆盖/
相同/保留四分类、身份文件保护、`state`/`logs` 不进包、空包报错、包与运行目录
的关系、留底命名与编号让路、验版本失败必回滚、坏二进制必回滚、
`Expect` 三态、命令行解析。全部 392 passed / 0 failed。

**未验证**：目标机（`.150` 自 11-10 15:40 起离线）；控制台【更新】按钮只验到
布局与 `check` 输出，弹窗确认那一步需要人点，没有自动化。
