# vendor/vigem — ViGEmClient.dll (x64)

虚拟手柄（#17）的唯一二进制依赖。服务端**动态加载**它：`dispatch/gamepad.rs`
用 `LoadLibraryW("ViGEmClient.dll")` 从 exe 同目录取，取不到就降级成"没有手柄"，
不影响启动。

## 文件

| 文件 | 说明 |
|---|---|
| `ViGEmClient.dll` | x64，140,288 B，`/MT`（自带 CRT），11 个导出符号 |
| `LICENSE` | 上游 MIT，版权归 Nefarius Software Solutions e.U. |

```
sha256  42613CE46C2DB30ECE222A47D124D5D980E175B93C37CAEA699B420B8022B22A
source  github.com/nefarius/ViGEmClient @ b66d02d57e32cc8595369c53418b843e958649b4 (2023-09-08)
built   toolchain/build-vigem-client.ps1  (MSVC 14.44 / SDK 10.0.22621)
```

## 重新构建

```powershell
pwsh toolchain/build-vigem-client.ps1
```

脚本会校验 11 个导出符号；缺一个就失败。这一步必要，原因见下。

## 三个必须知道的坑

1. **本机 vcpkg 没有 `vigemclient` 端口**（2883 个端口里搜不到），所以
   "装一下"这条路不存在，只能自己编。

2. **上游 CMakeLists 的 SHARED 目标没有定义 `VIGEM_DYNAMIC` / `VIGEM_EXPORTS`。**
   `VIGEM_API` 于是展开成空，链接器导出 0 个符号 —— DLL 能加载，但每个
   `GetProcAddress` 都失败，症状是"别的都对，就是没手柄"。`.vcxproj` 里有这两个
   宏，CMake 路径没有。所以脚本直接调 `cl.exe`，不走 CMake。

3. **cmake 找不到本机的 VS**（BuildTools-only 安装），错误是
   `could not find any instance of Visual Studio`。脚本按目录自己拼
   `PATH/INCLUDE/LIB`，和 `deploy.ps1` 的 `Initialize-BuildEnv` 同一套做法。

## 部署

`deploy.ps1` 把它作为 `ViGEmClient.new.dll` 上传，`deploy-remote.ps1` 在**停进程之后**
才覆盖 `C:\dev\stream\ViGEmClient.dll` —— 已加载的 DLL 被进程持有，运行时覆盖会
共享冲突（和 exe 必须走 `.new` 是同一个原因）。

## 被控机的前置条件（一次性的）

驱动本身不在这个仓库里（6 MB 安装包，且是机器级安装而非构建依赖）。

1. 装 ViGEmBus 1.22.0（终版，2023-11 归档，WHQL 签名，Win11 24H2+ 可用）：

   ```
   ViGEmBus_1.22.0_x64_x86_arm64.exe /qn /norestart
   sha256 89220A7865076B342892F98865F3499FB7C4CFD673159E89D352C360FD014C6A
   ```

2. **把 in-box 的 `xusb22.inf` 预置进 DriverStore。** 这一步不做，`js,c` 一定失败，
   而且失败信息会骗人：

   ```
   pnputil /add-driver C:\Windows\INF\xusb22.inf /install
   ```

   没做这一步时的完整症状链（值得记住，排查会绕很久）：

   * `vigem_target_add` 返回 `VIGEM_ERROR_TARGET_NOT_PLUGGED_IN`（0xE0000007）。
     这是**二次错误**：子设备没就绪后 ViGEmClient 会顺手 `target_remove`，而它在
     未连接状态下返回的正是这个码（`ViGEmClient.cpp:634`），真正的失败被盖住了。
   * PnP 事件 430：`设备 USB\VID_045E&PID_028E\01 需要进一步安装`。
   * `pnputil /enum-drivers` 里搜不到 `xusb22` —— Windows 11 把这个 in-box 驱动
     改成了按需下发，虚拟机没有外网就永远等不到。

   预置之后，子设备在创建时立刻绑定，**不需要重启**（安装器自己报了
   `0x00000bc3 ERROR_SUCCESS_REBOOT_REQUIRED`，但那是总线驱动包的状态，
   与手柄子设备无关；不要为此重启一台没有配自动登录的串流机 —— 重启后
   DXGI 会落在锁屏桌面）。

3. 也可以直接跑 `pwsh toolchain/install-vigem.ps1`，它把上面两步和验收合成一条。

## 验收

不需要浏览器：

```powershell
cargo test --release -p stream-server hardware_roundtrip -- --ignored --nocapture
```

这个测试走的是真实代码路径（`Api::load` → `target_add` → `x360_update`），再用
`XInputGetState` 把状态读回来 —— 它验证的是 12 字节 `XUSB_REPORT` 按值传递的
ABI，以及 A/扳机/摇杆的映射。通过时输出形如：

```
XInput index 0: XinputGamepad { buttons: 4096, left_trigger: 64, right_trigger: 128, lx: 32767, ly: 32767, ... }
```

浏览器侧再验一遍：插手柄 → 游戏能动 → 双马达震动可复现 → 关页签后
`joy.cpl` 里设备消失。
