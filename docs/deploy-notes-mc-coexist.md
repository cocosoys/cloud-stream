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
计划任务：`MCGame`（/IT 交互式 /RU Administrator /RP Admin@2026，/SC ONCE /ST 00:00，schtasks /Run 触发）

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
