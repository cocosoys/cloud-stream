# 装配记录 — MC 共存复测（#37-5 / #48）

日期：2026-10-26（部署 148，stream-server PID 9480 #56 码率版）
环境：被控 CLIENT001（192.168.59.148，Win11 Pro 4vCPU/8GB，VMware SVGA 3D）

## 一、JDK8 部署（被控原无 Java）

| 项 | 值 |
|---|---|
| 下载源 | `https://mirrors.tuna.tsinghua.edu.cn/Adoptium/8/jdk/x64/windows/OpenJDK8U-jdk_x64_windows_hotspot_8u504b01.zip`（106,457,258 B） |
| 安装位置 | `C:\dev\jdk8\jdk8u504-b01\bin\java.exe`（zip 免安装，解压即用） |
| 版本 | OpenJDK 1.8.0_504 (Temurin)，64-Bit Server VM 25.504-b01 |

## 二、MC 客户端环境（用户已复制 .minecraft 到被控）

- 根目录：`C:\Users\Administrator\Desktop\.minecraft`
- 版本：**1.12.2-Forge_14.23.5.2864**（mainClass=`net.minecraft.launchwrapper.Launch`）
- json：61 libraries（55 个 jar 就位，6 个 natives/Mac-only 跳过）；assetIndex=1.12（assets/indexes/1.12.json 就位）
- natives：`natives-tmp\` 已含 lwjgl.dll / jinput 全套
- mods：Custom NPCs 等 5 个 mod 加载成功（FML "successfully loaded 5 mods"）
- 顶层另含 PCL 启动器目录、saves、resourcepacks、config、customnpcs

## 三、启动方式（免启动器命令行直启）

脚本：`C:\dev\mc\start-mc.ps1`（本机备份 `cloud-stream\toolchain\start-mc.ps1`）
计划任务：`MCGame`（交互式，`/SC ONCE /ST 00:00`，`schtasks /Run` 触发）。这台机器上的
老任务是用 `/RU Administrator /RP <口令>` 建的 —— 口令存在任务里，也不再写进文档；
重建时改用 `InteractiveToken`（现在是 `stream-setting.exe --install` 生成的任务 XML），任务里就不剩凭据了。

启动命令等价（单字符串参数，避免 Start-Process 数组拆断含空格路径）：
```
C:\dev\jdk8\jdk8u504-b01\bin\java.exe -Xmx2G -Xms512M
  "-Djava.library.path=C:\Users\Administrator\Desktop\.minecraft\natives-tmp"
  -cp "<libraries 55 项 + version jar>"
  net.minecraft.launchwrapper.Launch --username CloudPlayer
  --version 1.12.2-Forge_14.23.5.2864 --gameDir <mc> --assetsDir <mc>\assets
  --assetIndex 1.12 --uuid 00000000-... --accessToken 0 --userType legacy
  --versionType Forge --tweakClass net.minecraftforge.fml.common.launcher.FMLTweaker
```

踩坑记录：
1. classpath 拼接：json `downloads.artifact.path` 相对 **libraries 根**（`<mc>\libraries\<path>`），非 `<mc>\<path>`——首版少 55 个 jar 直接 `classpath_items=1` 启动失败。
2. `Start-Process -RedirectStandardOutput` 与脚本内 `Out-File` 争用同一日志句柄（IOException）→ 去掉重定向（MC 自写 `logs\latest.log`）。
3. `Start-Process -ArgumentList` 传数组会把含空格路径拆断 → 手动拼单字符串并为路径加双引号。

## 四、共存复测（#37-5）

启动顺序：stream-server（PID 9480）先运行 → MC 启动 → 两者共存。

| 指标 | 实测（MC 主菜单 + 串流连接） |
|---|---|
| 总 CPU | 45–48% |
| 内存 | 总量 8GB，空闲 ~4GB |
| java (MC) | PID 10128，WorkingSet ~1,188 MB |
| stream-server | PID 9480，WorkingSet ~87 MB |

结论：4vCPU/8GB 下 **MC 1.12.2-Forge 与 1080p@60 软编串流可共存**，主菜单空闲水位 CPU<50%。进入游戏世界（区块加载/渲染）后需复测 CPU，若超 85% 需降档（720p@60 或 1080p@30）。

## 五、待用户手工验收

1. 浏览器刷新 https://192.168.59.148:8080/ → 应显示 **MC 主菜单**画面
2. 鼠标移动/点击/滚轮操作主菜单（输入注入 #54 已部署）
3. 进入游戏后验证：WASD 移动、视角、背包、聊天输入（含中文 IME）
4. 退出 MC 复验串流回空桌面无异常
5. 断开浏览器重连（重连已内置）验证画面恢复

## 六、.150 复刻（2026-11-13）

`.148` 之后的重装机器，形态和上面那份记录有不少出入，照抄会卡住。目标是云游戏链路
的第一环：**游戏进程得先在看得见的桌面上跑起来**，串流才有画面可送。

### 6.1 这台机器和 .148 的差别

| 项 | .148 | .150（DESKTOP-0L2MMH3） |
|---|---|---|
| 用户 | `Administrator` | `Admin` |
| mc 路径 | `C:\Users\Administrator\Desktop\.minecraft` | `C:\Users\Admin\Desktop\.minecraft` |
| JDK | `C:\dev\jdk8\jdk8u504-b01`（jdk，含 javac） | **原无** → 新增 `C:\mc\jre8`（jre，1.8.0_512） |
| 测试脚本落脚 | `C:\msi` | `C:\msi` **不存在**；`C:\mc` 是这次新建的 |
| D 盘 | 数据盘 | **Windows 安装介质**（`boot\ / efi\ / sources\ / AutoUnattend.xml`），别往上写东西 |
| 其它 | — | 装了 VMware Tools，`C:\rdpw` 是 RDP 包装 |

`.minecraft` 内容本身一致：1369 个文件 220.6 MB，`1.12.2-Forge_14.23.5.2864`，
`mods/` 里 `CustomNPCs_1.12.2-(05Jul20).jar` + `nashorn.jar`，启动后
`5 mods loaded, 5 mods active`。`natives-tmp/` 里 lwjgl / jinput / OpenAL 的 DLL 齐全，
所以 classpath 上那几个 `*-platform-*.jar` 缺着也不影响（它们只是 DLL 的容器）。

### 6.2 JRE 8 从哪来

GitHub 直连在这台机器上只有 **32–50 KB/s**（40 MB 要十几分钟，中途还会被掐），
清华镜像 **9.6 MB/s**，4 秒下完：

| 项 | 值 |
|---|---|
| 源 | `https://mirrors.tuna.tsinghua.edu.cn/Adoptium/8/jre/x64/windows/OpenJDK8U-jre_x64_windows_hotspot_8u512b09.zip` |
| 大小 | 40,124,908 B |
| 位置 | `C:\mc\jre8`（zip 解压即用，解出的顶层目录 `jdk8u512-b09-jre` 直接改名） |
| 版本 | OpenJDK 1.8.0_512 |

用 jre 而不是 jdk：这台只需要 `javaw.exe` 跑游戏，不需要编译。省一半体积。

### 6.3 启动

```cmd
ssh mc-150 "powershell -NoProfile -ExecutionPolicy Bypass -File C:\mc\start-mc.ps1 -Deliver"
```

`-Deliver` 是这台机器上必须的（见 6.4 第 1 条）：脚本发现自己在 session 0 就会用
`schtasks /IT` 建一个 `MCGame` 任务（和 `.148` 上同名），把自己重新投进 console 会话，
然后在那边启动 `javaw.exe`。日志在 `%TEMP%\mc-launch.log`，游戏自己的日志在
`.minecraft\logs\latest.log`。

实测：投递到主菜单约 **55 秒**（含 Forge 的 tweak 链与 512×512 纹理图集），
窗口 870×519 居中，工作集稳定后 ~1.2 GB。

### 6.4 新踩的四个坑

1. **ssh 落在 session 0，窗口不属于任何人看得见的桌面。** 症状极具误导性：进程活着、
   `latest.log` 一路写到 `Narrator library for x64 successfully loaded`、CPU 也在动，
   但屏幕上什么都没有。`query session` 能看到 `console  Admin  1  运行中`，
   投递进 session 1 才看得见。这和卸载器那次 `--uninstall-step` 开隐藏窗口是同一类
   问题——**进程存在 ≠ 人看得见**。
2. **`Start-Process -WindowStyle Hidden` 会把 `SW_HIDE` 传染给第一个窗口。**
   Minecraft 只有一个 LWJGL 窗口，于是它继承了这个设置：`EnumWindows` 能看到
   `cls=LWJGL title=[Minecraft 1.12.2]` 但 `vis=False`、尺寸 1x1。游戏在后台完美运行，
   就是没有画面。改用 `javaw.exe`（本身无控制台）后一切正常，别再加 Hidden。
3. **`.ps1` 里有中文注释就必须带 UTF-8 BOM。** PS 5.1 读无 BOM 文件按 GBK 解，
   中文注释的字节被错解后会吞掉换行，随后 `param(...)` 整块解析失败，报的错是
   `第 24 行 意外的标记 ")"` —— 看起来像语法写错，其实是编码。带 BOM 后（`EF BB BF`）
   PS 5.1 按 UTF-8 读，一次通过。
4. **`schtasks /TR` 不解释 `>` 重定向**，也不接受超过 261 字符的命令行。要日志就让
   脚本自己写文件，不要把重定向塞进 `/TR`。

### 6.5 怎么确认"真的在屏幕上"

跨会话截屏拿不到别会话的画面，所以把截屏脚本也投递进去：

```cmd
ssh mc-150 "powershell -File C:\mc\deliver.ps1 -Script C:\mc\shot.ps1 -TaskName MCSHOT"
```

`shot.ps1` 干三件事：先 `ShowWindow(GetConsoleWindow(), SW_HIDE)` 把自己的黑窗口藏掉
（否则它会出现在截图里，还会正好挡住游戏——这个自指问题折腾了两轮），再
`CopyFromScreen` 存 PNG，最后用 `EnumWindows` 把可见窗口列出来存档。
`win:` 行里出现 `5768 | 870x519 | Minecraft 1.12.2` 就是成了。

### 6.6 还没做

- [ ] 全屏启动。现在 870×519 窗口化，云游戏该跟上串流端的分辨率（`options.txt` 的
      `fullscreen` 或启动后 Alt+Enter），1080p 桌面下小窗看起来不像"云游戏"
- [ ] 与 `stream-server` 的共存复测（`.148` 那份是 1080p@60 软编，主菜单空闲时
      总 CPU 45–48%）
- [ ] 退出游戏：现在只能 `Stop-Process javaw`，没有体面的收尾（存档、麦克风归还）
- [ ] 把这条链收进 Rust：`CreateProcessAsUser` 投递 + 实例管理，替掉 PowerShell 与
      schtasks 这层胶水
