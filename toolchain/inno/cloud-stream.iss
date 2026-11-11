; ===========================================================================
; cloud-stream 安装器（Inno Setup 6.6+）
;
; 这一层**只做外壳**：向导、目录选择、铺文件、注册控制面板卸载项。
;
; 真正的系统集成仍然在 cloud-setup.exe 里，两条命令：
;   --finish  --dir <目录>                         装完后注册防火墙/任务并启动
;   --uninstall --keep --yes --quiet --dir <目录>   升级或卸载前先收走服务和规则
;
; 所以 Inno、将来的 WiX、以及那个便携的单文件 exe，三个外壳共用同一套判断。
; 把"防火墙要放行哪个端口""任务用哪个登录类型"搬进这里，就会变成第二套实现 ——
; 而第二套实现迟早和第一套分叉（这条教训写在 install.rs 的模块头里）。
;
; 构建时由 toolchain\deploy.ps1 设置三个环境变量，见下面的 #define。
; ===========================================================================

#define AppName "cloud-stream"
#define AppExeName "stream-server.exe"
#define ConsoleExe "cloud-setup.exe"
#define TaskName "MCStreamSrv"
#define AppPublisher "SOYS"

; ---------------------------------------------------------------------------
; 构建输入（环境变量 → 预处理常量）
;
; 用环境变量而不是命令行 /D：deploy.ps1 那边已经是"设一个变量、跑一个命令"
; 的写法（编安装器时用的 CS_PAYLOAD_DIR 也是这么传的），保持一致。
; ---------------------------------------------------------------------------

#ifndef AppVersion
  #define AppVersion GetEnv("CS_INNO_VERSION")
#endif
#if AppVersion == ""
  #error 缺少 CS_INNO_VERSION（形如 0.4.0）。用 toolchain\deploy.ps1 构建会自动设置。
#endif

#ifndef PayloadDir
  #define PayloadDir GetEnv("CS_INNO_PAYLOAD")
#endif
#if PayloadDir == ""
  #error 缺少 CS_INNO_PAYLOAD（指向 target\output\cloud-stream）。用 toolchain\deploy.ps1 构建会自动设置。
#endif

; 控制台那一份 exe（**不带 embed**）：它要进运行根，替代 payload 里被刻意
; 排除掉的那个。带 embed 的放进来就是"exe 里又有整个包"，白占 8 MB。
#ifndef ConsoleExePath
  #define ConsoleExePath GetEnv("CS_INNO_CONSOLE")
#endif
#if ConsoleExePath == ""
  #error 缺少 CS_INNO_CONSOLE（指向不带 embed 的 cloud-setup.exe）。
#endif

#ifndef OutDir
  #define OutDir GetEnv("CS_INNO_OUT")
#endif
#if OutDir == ""
  #define OutDir "..\..\target\output"
#endif

[Setup]
; AppId 是这台机器上的身份：改了它，升级会变成"又装了一份"，控制面板里
; 会出现两条。**永远不要动这一行。**
AppId={{2148B123-EA2A-4B9C-A6A8-FE4854FC1795}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
DefaultDirName={autopf}\cloud-stream
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
; 安装要建计划任务、改防火墙 —— 两件事都只有管理员能做成。不提权装完
; 会得到"文件都在、但没有服务"的状态，那比装失败更难查。
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutDir}
OutputBaseFilename=cloud-stream-setup
Compression=lzma2/max
SolidCompression=yes
LZMAUseSeparateProcess=yes
; 6.6 起的能力：跟随系统明暗自动切换，套 Windows 11 配色。
; 强制暗色只多 220 KB，dynamic 再多 75 KB —— 比自带一套位图便宜得多。
WizardStyle=modern dynamic windows11
; 装完那几秒是黑盒（注册任务 + 等端口应答），给一句状态文字，别让人以为卡住。
; 真正的进度条在"正在安装"页，由文件复制驱动。
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\{#ConsoleExe}
; 我们的 exe 是常驻服务，Inno 的"检测占用程序"会把它当成用户没关的窗口。
; 占用问题自己在 PrepareToInstall 里处理（先 --uninstall --keep）。
CloseApplications=no
RestartApplications=no
SetupLogging=yes
AllowNoIcons=yes
; 版本资源显式写。两件事分清：
;
;   * "应用和功能"那张表读的是 AppVerName / DisplayVersion 这两个注册表值，
;     不读这里 —— 所以这里填错不会让卸载列表显示错版本。
;   * 这里填的是 exe 自己的属性页（右键 → 属性 → 详细信息）。不写会怎样：
;     实测一个只有 AppName/AppVersion 的最小 .iss，编出来的 FileVersion
;     整格是空的，ProductVersion 才有推导值。而这两格正是别人核对"你装的
;     到底是哪一版"的地方（一起发出去的还有便携版与 zip）。
VersionInfoVersion={#AppVersion}.0
VersionInfoProductVersion={#AppVersion}
VersionInfoProductTextVersion={#AppVersion}
VersionInfoTextVersion={#AppVersion}
VersionInfoCompany={#AppPublisher}
VersionInfoProductName={#AppName}
VersionInfoDescription={#AppName} 安装程序
VersionInfoOriginalFileName=cloud-stream-setup.exe
;
; 核对这几个值**别用 PowerShell 的 Format-List**：版本资源的字符串是按定长
; 补空格存的（Inno 有意如此，纯空格填充、没有 NUL），Format-List 遇到超过列宽
; 的值会折行，看起来就像尾巴上多了字符 —— 上一轮据此误判成"Inno 在字符串后
; 加 X"，还以为是加 VersionInfo* 之前的事。要核对就取属性本身
; （$v.FileVersion / $v.ProductName）或看字符编码：实测末尾是 0x20。

[Languages]
Name: "chinesesimp"; MessagesFile: "ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Messages]
; 欢迎页第一屏就回答"这玩意会对我做什么" —— 现在的控制台窗口是直接弹出，
; 用户不知道点了"安装"之后系统里会多出什么。
chinesesimp.WelcomeLabel2=这将把 %1 装到这台机器上。%n%n安装时会：%n  • 复制程序文件到您选定的目录%n  • 放行几个防火墙端口（TCP 8080 等）%n  • 注册一个每分钟检查一次的计划任务%n  • 启动服务，并等它应答%n%n整个过程不需要账户密码，也不会改动您的登录设置。
chinesesimp.SelectDirDesc=请选择安装位置。程序会装到该位置下的 cloud-stream 文件夹里。
chinesesimp.SelectDirLabel3=将安装到下面的位置（会自动创建 cloud-stream 子文件夹）：
chinesesimp.FinishedHeadingLabel=安装完成
chinesesimp.FinishedLabel=cloud-stream 已经装好并在运行。%n%n浏览器打开下面的地址就能看到画面。如果是第一次装，Windows 防火墙可能会问一次是否允许，选"允许"即可。

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; payload 就是运行根本身：2018 个文件，里面没有 cloud-setup.exe（构建时刻意
; 排除，否则会出现"exe 里嵌着包含自己的包"）。控制台那一份在下一行补进来。
Source: "{#PayloadDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#ConsoleExePath}"; DestDir: "{app}"; Flags: ignoreversion

[Dirs]
; 提权安装有个副作用：写进 {app} 的文件默认属主是管理员，普通用户改不了。
; 而 stream-config.toml 是文档里明确说"直接编辑它"的东西，logs 也要能写。
; 不显式授权，用户会撞上"拒绝访问"并且不知道为什么。
Name: "{app}"; Permissions: users-modify
Name: "{app}\logs"; Permissions: users-modify

[Icons]
Name: "{group}\{#AppName} 控制台"; Filename: "{app}\{#ConsoleExe}"
Name: "{group}\卸载 {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#ConsoleExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#ConsoleExe}"; Description: "打开 {#AppName} 控制台"; Flags: postinstall nowait skipifsilent

[UninstallRun]
; 先让 cloud-setup.exe 把服务收走（停任务、杀本目录进程、还录音设备、
; 删四条防火墙规则），再交给 Inno 删文件。
;
; `--keep` 是关键：这一步只卸"系统里的东西"，文件归 Inno 删 —— 两边都删
; 会互相踩（Inno 删到一半发现文件没了，或者我们删掉了 Inno 还要用的东西）。
; `--quiet` 同样关键：卸载页已经在告诉用户进度，这里再弹一个框就是两次。
Filename: "{app}\{#ConsoleExe}"; Parameters: "--uninstall --keep --yes --quiet --dir ""{app}"""; Flags: runhidden waituntilterminated; RunOnceId: "CSStopService"

[UninstallDelete]
; 运行时生成的东西不在安装清单里，Inno 卸载时不会碰它们。不列出来的话，
; 用户卸完会看到一个只剩 logs\*.log 的目录 —— 实机验过一次就是这样。
; 配置文件（stream-config.toml）在安装清单里，会跟着删，不用写在这里。
Type: filesandordirs; Name: "{app}\logs"
; 安装目录本身也要收掉。`dirifempty` 而不是 `filesandordirs`：装到别人
; 已有的目录里时（比如用户选了 D:\Tools），里面可能有他自己的东西，只删空目录。
; 只靠 Inno 自己的记录不够 —— 目录在安装之前就存在时，它不认为是自己建的。
Type: dirifempty; Name: "{app}"

[Code]
const
  ConsoleName = '{#ConsoleExe}';

/// 跑一条控制台命令并等它返回。
///
/// Filename 从 {app} 取而不是用相对路径：卸载阶段当前目录可能是 System32。
function RunConsole(const Params: String; var Code: Integer): Boolean;
var
  Exe: String;
begin
  Exe := ExpandConstant('{app}\' + ConsoleName);
  Result := FileExists(Exe) and
            Exec(Exe, Params, ExpandConstant('{app}'), SW_HIDE, ewWaitUntilTerminated, Code);
end;

/// 复制文件之前，先把可能还在跑的旧实例收走。
///
/// 不做的直接后果：升级时 stream-server.exe 正被计划任务跑着，Inno 复制到
/// 一半失败 —— 目录里留下新旧混杂的一堆文件，比"装失败"难查得多。
///
/// 失败就中止（返回非空字符串）：硬装下去只会得到更乱的状态。
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
  OldDir: String;
begin
  Result := '';

  // 旧版本（那个自带 payload 的 cloud-setup.exe）默认装在 %LOCALAPPDATA%。
  // 这里默认装到 Program Files，两份会抢同一个 8080，而计划任务是同一个
  // 名字 —— 后装的那份会把任务的动作路径改成自己，先装的那份从此没人管，
  // 但进程还活着。这种情况只能留一份，先问清楚。
  OldDir := ExpandConstant('{localappdata}\cloud-stream');
  if (CompareText(ExpandConstant('{app}'), OldDir) <> 0)
     and FileExists(OldDir + '\stream-server.exe') then
  begin
    Log('发现旧位置：' + OldDir);
    if not WizardSilent then
    begin
      if MsgBox('这台机器的 ' + OldDir + ' 里还有一份 cloud-stream。' + #13#10 + #13#10 +
                '两份会抢同一个端口（8080），计划任务也是同一个名字，只能留一份。' + #13#10 + #13#10 +
                '建议：先到那个目录双击 cloud-setup.exe 点"卸载"（会删掉那边的配置），' +
                '再把那边的 stream-config.toml 抄过来。' + #13#10 + #13#10 +
                '现在继续安装吗？', mbConfirmation, MB_YESNO) <> IDYES then
      begin
        Result := '已经有一份装在 ' + OldDir + '，没有继续。';
        Exit;
      end;
    end;
  end;

  // 全新安装：目标目录里没有控制台，就没有什么要收的。
  if not FileExists(ExpandConstant('{app}\' + ConsoleName)) then
    Exit;

  // 升级：先把服务停掉，让 Inno 能覆盖 stream-server.exe。
  //
  // 这里**刻意不调** cloud-setup.exe --uninstall。目标目录里那一份可能是
  // 旧版本，而旧版本的参数表里没有 --quiet —— 它会把不认识的开关忽略掉，
  // 然后按老规矩弹一个"卸载完成"框。向导背后（尤其静默安装）没有人去点
  // 那个确定，安装就永远停在这一步。**"用被升级的旧程序来完成升级"这件事
  // 本身就不成立**：它的行为是它那个版本的行为。
  //
  // 自己做停机只要两条命令，而且都是幂等的：防火墙规则由 --finish 里的
  // open_inbound 先删后建，任务由 /Create /F 覆盖，所以这里不必清规则。
  Log('停掉可能在跑的旧实例');
  Exec(ExpandConstant('{sys}\schtasks.exe'), '/End /TN {#TaskName}', '', SW_HIDE,
       ewWaitUntilTerminated, Code);
  // `taskkill` 找不到进程时退出码是 128，那是最常见的情况（本来就没跑），
  // 所以只看"起不起得来"，不看退出码。
  if not Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#AppExeName}', '', SW_HIDE,
              ewWaitUntilTerminated, Code) then
  begin
    Result := '停不掉正在运行的 {#AppExeName}。' + #13#10 + #13#10 +
              '先在任务管理器里结束它，再回来装。';
    Exit;
  end;
  // 给文件句柄一点时间真正释放。taskkill 返回时进程通常已经退出，但
  // 最后一两个句柄可能还挂在系统里。
  Sleep(600);
end;

/// 文件铺完之后，让 cloud-setup.exe 去做它那四步。
procedure CurStepChanged(CurStep: TSetupStep);
var
  Code: Integer;
begin
  if CurStep <> ssPostInstall then
    Exit;

  Log('文件已就位，开始系统集成（防火墙 / 计划任务 / 启动）');
  if not RunConsole('--finish --quiet --dir "' + ExpandConstant('{app}') + '"', Code) then
  begin
    Log('--finish 起不来');
    if not WizardSilent then
      MsgBox('文件都装好了，但"注册计划任务、放行端口、启动服务"这一步没能执行。' + #13#10 + #13#10 +
             '原因：' + ExpandConstant('{app}\' + ConsoleName) + ' 起不来。' + #13#10 +
             '到那个目录里双击它，点"安装 / 修复"可以补上。', mbError, MB_OK);
    Exit;
  end;
  if Code <> 0 then
  begin
    Log(Format('--finish 退出码 %d', [Code]));
    if not WizardSilent then
      MsgBox('文件已经装好，但服务没起来（退出码 ' + IntToStr(Code) + '）。' + #13#10 + #13#10 +
             '最常见的原因是这台机器缺 VC++ 运行库（vcruntime140.dll），' + #13#10 +
             '那会让 stream-server.exe 在写第一条日志之前就退出。' + #13#10 + #13#10 +
             '细节在 %ProgramData%\cloud-stream\last-install.txt，' + #13#10 +
             '补装一次运行库后，到程序目录双击控制台点"安装 / 修复"即可。', mbError, MB_OK);
    Exit;
  end;

  Log('系统集成完成');
end;
