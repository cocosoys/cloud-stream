"""被控端（Windows）CPU 占用排行探针：只读。

修正点：GetSystemTimes 的 kernel 时间**包含** idle，所以机器忙闲必须用
1 - idle/(wall*ncpu) 计算，不能用 (kernel+user)。

用法：
    C:\\Python311\\python.exe probe-top.py [采样秒] [topN]
"""

import ctypes
import ctypes.wintypes as wt
import sys
import time

k32 = ctypes.WinDLL("kernel32", use_last_error=True)

TH32CS_SNAPPROCESS = 0x00000002
PROCESS_QUERY_LIMITED_INFORMATION = 0x1000


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


def snapshot():
    snap = k32.CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
    entry = PROCESSENTRY32W()
    entry.dwSize = ctypes.sizeof(PROCESSENTRY32W)
    out = []
    ok = k32.Process32FirstW(snap, ctypes.byref(entry))
    while ok:
        out.append((entry.th32ProcessID, entry.szExeFile, entry.cntThreads))
        ok = k32.Process32NextW(snap, ctypes.byref(entry))
    k32.CloseHandle(snap)
    return out


def cpu_ticks(pid):
    h = k32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    if not h:
        return None
    creation, exit_t, kernel, user = FILETIME(), FILETIME(), FILETIME(), FILETIME()
    ok = k32.GetProcessTimes(
        h,
        ctypes.byref(creation),
        ctypes.byref(exit_t),
        ctypes.byref(kernel),
        ctypes.byref(user),
    )
    k32.CloseHandle(h)
    if not ok:
        return None
    return kernel.to_int() + user.to_int()


def sys_idle():
    idle, kernel, user = FILETIME(), FILETIME(), FILETIME()
    k32.GetSystemTimes(ctypes.byref(idle), ctypes.byref(kernel), ctypes.byref(user))
    return idle.to_int()


def main():
    win = float(sys.argv[1]) if len(sys.argv) > 1 else 3.0
    topn = int(sys.argv[2]) if len(sys.argv) > 2 else 10
    ncpu = k32.GetActiveProcessorCount(0xFFFF) or 1

    first = {}
    names = {}
    threads = {}
    for pid, name, thr in snapshot():
        t = cpu_ticks(pid)
        if t is not None:
            first[pid] = t
            names[pid] = name
            threads[pid] = thr
    idle0 = sys_idle()
    t0 = time.time()
    time.sleep(win)
    idle1 = sys_idle()
    t1 = time.time()

    wall = t1 - t0
    busy = 100.0 - (idle1 - idle0) / 1e7 / wall / ncpu * 100.0
    print("logical_cpus=%d sample=%.1fs machine_busy=%.1f%%" % (ncpu, wall, busy))

    rows = []
    for pid, name, thr in snapshot():
        if pid not in first:
            continue
        t = cpu_ticks(pid)
        if t is None:
            continue
        core = (t - first[pid]) / 1e7 / wall * 100.0
        if core <= 0.1:
            continue
        rows.append((core, core / ncpu, pid, name, thr))
    rows.sort(reverse=True)
    print("%-32s %6s %9s %8s %6s" % ("exe", "pid", "core%%", "machine%%", "thr"))
    for core, machine, pid, name, thr in rows[:topn]:
        print("%-32s %6d %9.1f %8.1f %6d" % (name[:32], pid, core, machine, thr))

    print("")
    print("sum(core%%)=%.1f over %d cpus -> %.1f%% of machine in sampled processes"
          % (sum(r[0] for r in rows), ncpu, sum(r[1] for r in rows)))


if __name__ == "__main__":
    main()
