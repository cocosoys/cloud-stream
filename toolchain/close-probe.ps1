param([int]$TargetPid)

# 关闭路径实验：先激活再发 WM_CLOSE（模拟用户点标题栏 X 时的状态）。
#
# 上一次实测的结论值得记下来：给 LWJGL 窗口发 WM_CLOSE，**窗口消失了但 javaw
# 没退出**（进程停在 71 MB）。也就是说 MC 1.12.2 把 WM_CLOSE 当"销毁窗口"，
# 而不是"退出游戏"。这个探针要回答的是：先 SetForegroundWindow 再发，行为会不会
# 不同（玩家点 X 时窗口是激活的）。
#
# 必须跑在 console 会话：ssh 落在 session 0，EnumWindows 在那里看不到用户桌面。
Start-Transcript -Path 'C:\mc\close-probe.out' -Force | Out-Null

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public class WC {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] static extern int GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] static extern bool IsWindow(IntPtr h);
    [DllImport("user32.dll")] static extern bool IsIconic(IntPtr h);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] static extern bool ShowWindow(IntPtr h, int cmd);
    [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] static extern bool BringWindowToTop(IntPtr h);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern bool PostMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [StructLayout(LayoutKind.Sequential)] struct RECT { public int L, T, R, B; }
    public static string Dump(uint pid) {
        StringBuilder sb = new StringBuilder();
        EnumWindows(delegate(IntPtr h, IntPtr l) {
            uint p; GetWindowThreadProcessId(h, out p);
            if (p == pid) {
                StringBuilder c = new StringBuilder(128);
                GetClassNameW(h, c, 128);
                RECT r; GetWindowRect(h, out r);
                sb.AppendLine(string.Format("  hwnd=0x{0:X} class={1} {2}x{3}@{4},{5} visible={6} iconic={7}",
                    h.ToInt64(), c, r.R - r.L, r.B - r.T, r.L, r.T, IsWindowVisible(h), IsIconic(h)));
            }
            return true;
        }, IntPtr.Zero);
        return sb.ToString();
    }
    public static IntPtr Lwjgl(uint pid) {
        IntPtr found = IntPtr.Zero;
        EnumWindows(delegate(IntPtr h, IntPtr l) {
            uint p; GetWindowThreadProcessId(h, out p);
            if (p == pid) {
                StringBuilder c = new StringBuilder(128);
                GetClassNameW(h, c, 128);
                if (c.ToString() == "LWJGL") { found = h; return false; }
            }
            return true;
        }, IntPtr.Zero);
        return found;
    }
    public static bool Fore(IntPtr h) {
        ShowWindow(h, 9);
        BringWindowToTop(h);
        return SetForegroundWindow(h);
    }
    public static bool IsAlive(IntPtr h) { return IsWindow(h); }
    public static bool Send(IntPtr h, uint msg, int w) { return PostMessageW(h, msg, (IntPtr)w, IntPtr.Zero); }
}
'@
$p = Get-Process -Id $TargetPid -ErrorAction SilentlyContinue
if (-not $p) { "pid $TargetPid not running"; Stop-Transcript | Out-Null; exit }
"target pid=$TargetPid"
"windows:"
[WC]::Dump([uint32]$TargetPid)
$h = [WC]::Lwjgl([uint32]$TargetPid)
if ($h -eq [IntPtr]::Zero) { "no LWJGL window"; Stop-Transcript | Out-Null; exit }
"chosen LWJGL hwnd=0x{0:X}" -f $h.ToInt64()
"set foreground = " + [WC]::Fore($h)
Start-Sleep -Milliseconds 600

"1) WM_CLOSE = " + [WC]::Send($h, 0x0010, 0)
Start-Sleep -Seconds 5
$alive = [bool](Get-Process -Id $TargetPid -ErrorAction SilentlyContinue)
"   process alive = $alive ; window alive = $([WC]::IsAlive($h))"

if ($alive -and [WC]::IsAlive($h)) {
    "2) WM_SYSCOMMAND(SC_CLOSE) = " + [WC]::Send($h, 0x0112, 0xF060)
    Start-Sleep -Seconds 5
    $alive = [bool](Get-Process -Id $TargetPid -ErrorAction SilentlyContinue)
    "   process alive = $alive ; window alive = $([WC]::IsAlive($h))"
}
Stop-Transcript | Out-Null
