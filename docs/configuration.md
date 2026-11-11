# 配置文件

`stream-server.exe` 接受一个 TOML 配置文件。这份文档讲三件事：配置从哪来、
每个键是什么意思、改错了会怎样。

## 1. 为什么会有这个东西

在这之前，三十二个参数只存在于 `start-stream.cmd` 里一段由 `set "ARGS=%ARGS% ..."`
拼出来的字符串里，还带着三处条件分支（证书存在才加、探测桌面当上传根、逐行读
`auth-token.txt`）。那个脚本已经删了，同样的三条分支现在是 `run.rs` 里三次
`if`，而参数本体在这里。后果有三个，都不致命但每天都硌人：

- 改一个分辨率要编辑 cmd、数引号、再重启计划任务；
- 改完**没有任何地方能回显"现在生效的是什么"**，只能去信那份字符串；
- 一个可视化安装器没有地方可以写配置 —— 它只能重新生成那个 cmd，而 cmd 里的
  条件逻辑没法用表单表达。

## 2. 三层与优先级

```
默认值  <  配置文件  <  命令行
```

**命令行永远赢。** 这不是随手定的：配置文件是"这台机器的常态"，命令行是
"这一次的临时覆盖"。反过来会让调试时敲的临时参数在下次看门狗重启后变成常态，
而那时已经没人记得是谁改的。

这条规则还被用在一个具体的地方：`--run` 里那些"文件存在才传"的参数
（`--tls-cert`、`--upload-dir`、`--auth-token`）**必须**留在命令行，因为
配置文件表达不了"这个文件要是有才传"。

## 3. 文件在哪

| 情形 | 位置 | 文件不存在时 |
|---|---|---|
| `--config <path>` | 指定的路径 | **报错退出**（码 2）。说了有这么一份，找不到就是假设错了 |
| 没写 `--config` | `<exe 所在目录>/config.toml` | 静默跳过，全部用默认值 + 命令行 |

默认位置先找 `config.toml`、再找 `stream-config.toml`（`configfile::resolve`，
`Config::parse` / `--ops` / `--run` 共用这一处）。部署包里的模板叫后者 ——
之所以不叫 `config.toml`，
是因为 `toolchain/config.toml` 已经是 cargo 的镜像配置，两个东西用同一个名字
迟早会有人改错。

## 4. 键表

键名与命令行一一对应：`--some-flag` 写成 `some_flag`。三个例外保留简写
（`app` / `mode` / `bitrate`），因为它们是改得最勤的三行。

| 键 | 默认 | 对应参数 | 说明 |
|---|---|---|---|
| `host` | `0.0.0.0` | `--host` | 监听地址 |
| `port` | `8080` | `--port` | 监听端口；有证书时同一端口就是 HTTPS |
| `app` | `webrtc` | `--app` | 信令路径里的 app 段 |
| `mode` | `websockets` | `--mode` | 新页面默认传输：`websockets` / `webrtc` |
| `dist` | `selkies-web` | `--dist` | 前端产物目录（相对路径相对于工作目录） |
| `width` / `height` | `1920` / `1080` | `--width` / `--height` | 采集与编码的输出尺寸 |
| `fps` | `30` | `--fps` | 目标帧率 |
| `bitrate` | `2500` | `--bitrate` | 目标码率，kbps |
| `repeat_every` | `2` | `--repeat-every` | 静止画面每 N 拍重发一帧 |
| `keyframe_secs` | `10` | `--keyframe-secs` | 周期关键帧间隔（钳在 1–120） |
| `video_codec` | `vp9` | `--video-codec` | `vp9` / `vp8` |
| `display` | `0` | `--display` | 启动时采集哪块屏（DXGI 序号） |
| `encoder` | `auto` | `--encoder` | `auto` / `software` / `nvenc` / `amf` / `qsv` |
| `tune` | `tuned` | `--tune` | `tuned` / `legacy` |
| `tls_cert` | 空 | `--tls-cert` | 空串 = 不启用 HTTPS |
| `tls_key` | 空 | `--tls-key` | 同上 |
| `log` | `logs\stream-server.log` | `--log` | 空串 = 只写标准输出 |
| `input_policy` | `exclusive` | `--input-policy` | `exclusive`（只有持锁会话能操作）/ `shared` |
| `max_sessions` | `8` | `--max-sessions` | 并发信令会话上限 |
| `ghost_cursor_idle` | `5` | `--ghost-cursor-idle` | 幽灵指针静止多少秒淡出 |
| `mic_device` | `auto` | `--mic-device` | 下行音频写哪 |
| `mic_default_device` | `auto` | `--mic-default-device` | `auto` / `off` |
| `upload_dir` | 空 | `--upload-dir` | 空串 = 关闭文件传输 |
| `upload_quota_mb` | `512` | `--upload-quota-mb` | 每租户配额，`0` = 不限 |
| `webrtc_udp_port_min` / `_max` | `40000` / `40049` | 同名 | ICE 的 UDP 区间 |
| `policy` | `auto` | `--policy` | `auto` / `allow-all` / `role-based` / `deny-all` |
| `auth_token` | 空 | `--auth-token` | 空串 = 局域网不鉴权 |
| `auth_grants` | `[]` | `--auth-grant`（可重复） | `["租户:角色:令牌"]`，**整体替换**不是追加 |
| `no_hdr` | `false` | `--no-hdr` | 跳过 HDR 采集尝试 |
| `dump_frame` | 空 | `--dump-frame` | 帧转储目录 |

**不进配置文件的两个参数**：`--restore-mic` 和 `--print-config`。它们描述的是
"这次要做什么"，不是"这台机器是什么样"。写进文件会让每次启动都去还原一遍
录音设备。

## 5. 空串与删行

| 写法 | 含义 |
|---|---|
| 删掉整行 / 文件里没有这个键 | 用默认值（或被命令行覆盖） |
| `key = ""` | **显式关掉**（只对有意义的项：`tls_cert`、`tls_key`、`log`、`upload_dir`、`dump_frame`） |
| `key = 0` | 数值项的 0，含义由该项自己定义（如 `upload_quota_mb = 0` 是不限） |

这两种写法的区别是必要的：`log = ""` 是"我知道默认会写文件，我要关掉它"，
而删掉这一行是"我没意见"。折成一种就没有办法关掉带默认值的项。

## 6. 打印当前生效值

```powershell
# 人看：带注释的 TOML，可直接当模板
stream-server.exe --config stream-config.toml --print-config > mine.toml

# 程序读：同样的键名，JSON
stream-server.exe --print-config json
```

两条硬保证：

1. **打出来的是生效值，不是默认值。** 包括命令行覆盖之后的结果。
2. **能回环。** 把 TOML 输出存成文件再启动，得到的是同一个配置 —— 逐字节相同。
   有一个测试（`configfile::tests::tomf_round_trip_is_identity`）守着这一条。

来源信息走 **stderr**（`# source: ...`），stdout 只有配置本体。这是为了让
`> mine.toml` 得到的文件可以原样拿去用。

## 7. 改错了会怎样

| 错误 | 行为 | 为什么 |
|---|---|---|
| 键名拼错（`widht`） | **启动失败**，退出码 2，报错点名是哪个键 | 静默忽略会表现成"我改了没生效"，比启动失败贵一个量级 |
| 类型不对（`port = "8080"`） | 启动失败，报错带行列号 | 同上 |
| 语法坏掉（`port =`） | 启动失败，报错带行列号 | 同上 |
| 枚举值不认识（`input_policy = "everyone"`） | 保留默认，**stderr 一行告警**，继续启动 | 与命令行完全一致。一个拼错的值不该让被控机起不来 |
| `auth_grants` 里某一条格式坏 | 跳过那一条，告警里点名它 | 一条坏令牌不该让整机起不来，但更不能静默少一条 |

退出码的区分是有用的：**2 = 配置错**（改文件才可能好），其它非零 = 运行期失败
（端口被占之类，重试可能好）。看门狗重启一次配置错误一万次也不会好，所以它不
该被当成普通失败处理。

## 8. 部署时谁覆盖谁

部署包里带 `stream-config.toml`，跟着每次部署一起上传覆盖 —— 行为与"命令行原本
写在 `start-stream.cmd` 里、跟着部署覆盖"完全同构，一天都没变过。

"每台机器的调整要留住、不要被部署抹掉"是**安装器/更新器**那一层要解决的
问题（配置与程序分离、升级时保留配置），不在这一步。在那之前，机器差异要么
接受被覆盖，要么写进 `stream-config.toml`（它同样会被覆盖），要么部署后手工改。

## 9. 怎么改（不用记事本）

三条路，同一条后端：

```bat
:: 看现在写着什么（原文 + 类型表；不用打开文件）
stream-server.exe --ops config-get

:: 改一项；--ops-set 可重复，--ops-unset 删掉整行（= 回默认）
stream-server.exe --ops config-set --ops-set fps=30 --ops-set video_codec=vp9
stream-server.exe --ops config-set --ops-unset bitrate

:: 图形界面：控制台窗口的「改配置」，或 cloud-setup.exe --config
```

`config-set` 保证的四件事：

1. **只改那一行。** 注释、分组、行尾备注原样留着，CRLF 也不会变成 LF ——
   "读成结构体再整个写回"会把这份文件的价值（注释）抹掉。
2. **先校验再动文件。** 键名拼错、超范围、类型不对都在"保存"这一刻失败，
   而不是停在下次启动；一批里有一项不合法就整批不改。
3. **留底。** 配置在运行目录里时拍一份 `preedit` 快照（`--ops rollback`
   能退回）；在运行目录之外时在旁边写一份 `.pre-edit` 副本。留底失败就不写。
4. **原子写。** 先写 `.toml.tmp` 再改名。半截配置的下场是服务起不来，那正是
   这里最不该制造的状态。

空串与删行仍然是两件事：`--ops-set tls_cert=` 会因为"不能为空"被拒绝，
要去掉这一项用 `--ops-unset tls_cert`。

## 10. 与版本的关系

`/api/v1/version` 报 `version` / `commit` / `dirty` / `built_at`。它和配置文件
没有直接关系，但两者解决的是一类问题：**让"这台机器现在是什么状态"可被程序
回答**。更新器要判断"要不要升级"，看的是前者；配置界面要回填表单，看的是
`--print-config json`。
