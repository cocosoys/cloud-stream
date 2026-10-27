"""列出关键进程的父子链与启动时间，用于确认「谁启动的 stream-server」。

用法：C:\\Python311\\python.exe ps-tree.py
"""

import ctypes
import ctypes.wintypes as wt
import time

k32 = ctypes.WinDLL("kernel32", use_last_error=True)
PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
TH32CS_SNAPPROCESS = 2


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
    rows = []
    ok = k32.Process32FirstW(snap, ctypes.byref(entry))
    while ok:
        rows.append((entry.th32ProcessID, entry.th32ParentProcessID,
                     entry.szExeFile, entry.cntThreads))
        ok = k32.Process32NextW(snap, ctypes.byref(entry))
    k32.CloseHandle(snap)
    return rows


def started(pid):
    h = k32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    if not h:
        return "?"
    c, e, k, u = FILETIME(), FILETIME(), FILETIME(), FILETIME()
    ok = k32.GetProcessTimes(h, ctypes.byref(c), ctypes.byref(e),
                            ctypes.byref(k), ctypes.byref(u))
    k32.CloseHandle(h)
    if not ok:
        return "?"
    secs = c.to_int() / 1e7 - 11644473600
    return time.strftime("%H:%M:%S", time.localtime(secs))


def main():
    rows = snapshot()
    by_pid = {r[0]: r for r in rows}
    targets = [r for r in rows if r[2].lower() in ("stream-server.exe",)]
    for pid, ppid, name, thr in targets:
        print("%s pid=%d ppid=%d threads=%d started=%s" % (name, pid, ppid, thr, started(pid)))
        p = by_pid.get(ppid)
        seen = 0
        while p and seen < 5:
            print("   <- %s pid=%d threads=%d started=%s" % (p[2], p[0], p[3], started(p[0])))
            p = by_pid.get(p[1])
            seen += 1
    print("--- cmd.exe / wscript / python 相关 ---")
    for pid, ppid, name, thr in rows:
        if name.lower() in ("cmd.exe", "conhost.exe", "wscript.exe", "wt.exe"):
            print("%-14s pid=%-6d ppid=%-6d started=%s" % (name, pid, ppid, started(pid)))


if __name__ == "__main__":
    main()
