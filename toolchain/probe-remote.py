"""被控端（Windows）诊断探针：只读，不修改任何状态。

用途：在 192.168.59.148 上抓 stream-server 的进程级事实，用于 fps 归因：
  * 进程是否存在、可执行文件路径、启动时间（判断跑的是哪个构建）
  * 线程数、工作集 / 提交内存
  * 进程 CPU 占用（占单核百分比 + 占全机百分比）
  * 全机 CPU 忙闲（含 idle 拆分）
  * 每隔 interval 打印一次采样，便于看抖动

设计：不依赖 psutil / wmic / PowerShell，只用 ctypes 调 kernel32 + psapi，
避免新版 Windows 移除 wmic 带来的不可用。GBK 控制台输出统一用 ASCII。

用法：
    C:\\Python311\\python.exe probe-remote.py [秒数] [采样间隔]
"""

import ctypes
import ctypes.wintypes as wt
import sys
import time

k32 = ctypes.WinDLL("kernel32", use_last_error=True)
psapi = ctypes.WinDLL("psapi", use_last_error=True)

TH32CS_SNAPPROCESS = 0x00000002
PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
PROCESS_QUERY_INFORMATION = 0x0400
PROCESS_VM_READ = 0x0010


class PROCESSENTRY32W(ctypes.Structure):
    _fields_ = [
        ("dwSize", wt.DWORD),
        ("cntUsage", wt.DWORD),
        ("th32ProcessID", wt.DWORD),
        ("th32DefaultHeapID", ctypes.c_void_p),
        ("th32ModuleID", wt.DWORD),
        ("cntThreads", wt.DWORD),
        ("th32ParentProcessID", wt.DWORD),
        ("pcPriClassBase", ctypes.c_long),
        ("dwFlags", wt.DWORD),
        ("szExeFile", wt.WCHAR * 260),
    ]


class FILETIME(ctypes.Structure):
    _fields_ = [("dwLowDateTime", wt.DWORD), ("dwHighDateTime", wt.DWORD)]

    def to_int(self):
        return (self.dwHighDateTime << 32) | self.dwLowDateTime


class PROCESS_MEMORY_COUNTERS(ctypes.Structure):
    _fields_ = [
        ("cb", wt.DWORD),
        ("PageFaultCount", wt.DWORD),
        ("PeakWorkingSetSize", ctypes.c_size_t),
        ("WorkingSetSize", ctypes.c_size_t),
        ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
        ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
        ("PagefileUsage", ctypes.c_size_t),
        ("PeakPagefileUsage", ctypes.c_size_t),
    ]


def find_pids(exe_name):
    snap = k32.CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
    if snap == ctypes.c_void_p(-1).value or snap == -1:
        raise OSError("CreateToolhelp32Snapshot failed")
    entry = PROCESSENTRY32W()
    entry.dwSize = ctypes.sizeof(PROCESSENTRY32W)
    out = []
    ok = k32.Process32FirstW(snap, ctypes.byref(entry))
    while ok:
        if entry.szExeFile.lower() == exe_name.lower():
            out.append((entry.th32ProcessID, entry.cntThreads, entry.th32ParentProcessID))
        ok = k32.Process32NextW(snap, ctypes.byref(entry))
    k32.CloseHandle(snap)
    return out


def proc_times(handle):
    creation, exit_t, kernel, user = FILETIME(), FILETIME(), FILETIME(), FILETIME()
    ok = k32.GetProcessTimes(
        handle,
        ctypes.byref(creation),
        ctypes.byref(exit_t),
        ctypes.byref(kernel),
        ctypes.byref(user),
    )
    if not ok:
        raise ctypes.WinError(ctypes.get_last_error())
    return creation, kernel.to_int() + user.to_int()


def sys_times():
    idle, kernel, user = FILETIME(), FILETIME(), FILETIME()
    k32.GetSystemTimes(ctypes.byref(idle), ctypes.byref(kernel), ctypes.byref(user))
    return idle.to_int(), kernel.to_int(), user.to_int()


def image_path(handle):
    buf = ctypes.create_unicode_buffer(32768)
    size = wt.DWORD(32768)
    if k32.QueryFullProcessImageNameW(handle, 0, buf, ctypes.byref(size)):
        return buf.value
    return "<unavailable>"


def mem_info(handle):
    pmc = PROCESS_MEMORY_COUNTERS()
    pmc.cb = ctypes.sizeof(PROCESS_MEMORY_COUNTERS)
    if psapi.GetProcessMemoryInfo(handle, ctypes.byref(pmc), pmc.cb):
        return pmc.WorkingSetSize, pmc.PagefileUsage
    return 0, 0


def filetime_to_local(ft_int):
    # 100ns since 1601-01-01 UTC
    secs = ft_int / 1e7 - 11644473600
    return time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(secs))


def main():
    duration = float(sys.argv[1]) if len(sys.argv) > 1 else 12.0
    interval = float(sys.argv[2]) if len(sys.argv) > 2 else 2.0

    ncpu = k32.GetActiveProcessorCount(0xFFFF) or 1
    print("logical_cpus=%d" % ncpu)

    procs = find_pids("stream-server.exe")
    if not procs:
        print("stream-server.exe: NOT RUNNING")
        return
    for pid, threads, ppid in procs:
        h = k32.OpenProcess(
            PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, False, pid
        )
        print("pid=%d threads=%d ppid=%d" % (pid, threads, ppid))
        if not h:
            print("  open failed err=%d" % ctypes.get_last_error())
            continue
        print("  image=%s" % image_path(h))
        creation, cpu0 = proc_times(h)
        print("  started=%s" % filetime_to_local(creation.to_int()))
        ws, commit = mem_info(h)
        print("  working_set=%.1fMB commit=%.1fMB" % (ws / 1048576.0, commit / 1048576.0))
        print("  cpu_seconds_total=%.2f" % (cpu0 / 1e7))

        idle0, k0, u0 = sys_times()
        t0 = time.time()
        print("")
        print("  t  proc_core%%  proc_machine%%  machine_busy%%  threads  ws_MB")
        steps = int(duration / interval)
        for i in range(steps):
            time.sleep(interval)
            _, cpu1 = proc_times(h)
            idle1, k1, u1 = sys_times()
            t1 = time.time()
            wall = (t1 - t0)
            # 100ns ticks -> seconds
            pcpu = (cpu1 - cpu0) / 1e7
            proc_core = pcpu / wall * 100.0
            proc_machine = proc_core / ncpu
            # NOTE: GetSystemTimes' kernel time INCLUDES idle, so machine busy is
            # 1 - idle/(wall*ncpu); (kernel+user) would always read ~100%.
            busy = 100.0 - (idle1 - idle0) / 1e7 / wall / ncpu * 100.0
            ws, _ = mem_info(h)
            thr = find_pids("stream-server.exe")
            threads_now = thr[0][1] if thr else -1
            print(
                "  %2d  %8.1f  %10.1f  %12.1f  %7d  %5.1f"
                % (i + 1, proc_core, proc_machine, busy, threads_now, ws / 1048576.0)
            )
        k32.CloseHandle(h)


if __name__ == "__main__":
    main()
