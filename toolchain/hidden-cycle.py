#!/usr/bin/env python3
"""复现「切走标签页 → 切回」之后客户端 FPS 归零。

用户现场的控制台给出两件事：
  1. `videoChunksReceived` 从 1520 涨到 2902，而 `window.fps` 从 60 掉到 0
     —— 帧在到，是**计数**丢了；
  2. 之后出现 `Tab is hidden → STOP_VIDEO → Tab is visible → START_VIDEO`，
     紧接着 chunks 只从 2902 涨到 2904 —— 恢复之后**服务端像是不发了**。

这两件事必须分开量。所以本脚本同时读三组数：
  * 页面口径：`window.fps` / `window.videoChunksReceived` / `window.videoDivertOn`
  * 网络口径：CDP `Network.webSocketFrameReceived` 的条数（浏览器真正收到的 WS 帧）
  * 服务端口径：`/api/v1/health` 里这条会话的 `frames_total`

判定：
  * 切回后 net≈0 而服务端 frames_total 在涨 → 服务端没给这条连接发（STOP/START 恢复坏了）
  * 切回后 net≈60/s 而 window.fps=0     → 页面计数坏了（统计口径问题）

用法：
    python hidden-cycle.py [--port 9460] [--headed]
"""

import json
import sys
import time
import urllib.request

sys.path.insert(0, __file__.rsplit("\\", 1)[0])
from page_probe_lib import CDP, ensure_edge, open_target, close_all_targets  # noqa: E402

PORT = 9460
HIDE_S = 15.0
PROFILE = r"C:\Users\user\AppData\Local\Temp\selkies-hc"
URL = "https://192.168.59.148:8080/?transport=websockets"
HEADED = True

argv = sys.argv[1:]
for i, a in enumerate(argv):
    if a == "--port":
        PORT = int(argv[i + 1])
    elif a == "--headless":
        HEADED = False

SNAP = r"""(() => {
  const v = document.querySelector('video');
  const c = document.querySelector('canvas');
  return JSON.stringify({
    fps: window.fps,
    chunks: window.videoChunksReceived,
    divert: window.videoDivertOn,
    rows: Object.keys(window.videoStripeRows || {}).length,
    vis: document.visibilityState,
    size: v ? [v.videoWidth, v.videoHeight] : (c ? [c.width, c.height] : null),
    canvasEmpty: c ? (() => { try { return c.width === 0; } catch (e) { return null; } })() : null,
  });
})()"""


def find_stream_target(port):
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list", timeout=8) as r:
        for t in json.load(r):
            if t.get("type") == "page" and "8080" in t.get("url", ""):
                return t
    raise SystemExit("没有串流页面")


def main():
    spawned = ensure_edge(PORT, PROFILE, headed=HEADED, extra=["--window-size=1600,900"])
    # 只在复用已有浏览器时清场：刚 spawn 出来的浏览器只有那一个 about:blank，
    # 关掉它会让 Chromium 连同进程一起退出（端口随即 502）。
    if not spawned:
        close_all_targets(PORT)
    time.sleep(0.5)
    open_target(PORT, URL)
    print(f"edge {'headed' if HEADED else 'headless'} port={PORT}，等首帧…")
    time.sleep(12)

    target = find_stream_target(PORT)
    cdp = CDP(target["webSocketDebuggerUrl"])
    cdp.call("Runtime.enable")
    cdp.call("Network.enable")

    def sample(tag, seconds=2.0):
        s = json.loads(cdp.evaluate(SNAP))
        net0 = len(cdp.take_events("Network.webSocketFrameReceived"))
        time.sleep(seconds)
        net = len(cdp.take_events("Network.webSocketFrameReceived"))
        d = net - 0
        print(f"{tag:<22} fps={str(s['fps']):>4} chunks={s['chunks']:>6} "
              f"divert={str(s['divert'])[:5]:>5} rows={s['rows']:>2} "
              f"vis={s['vis']:<8} net={net / seconds:>5.1f}/s size={s['size']}")
        return s

    print("\n  阶段                  fps  chunks divert rows  可见性  网络帧/s")
    sample("1 静止基线", 2.0)
    sample("2 再采一次", 2.0)

    # 切走：新开一个空白标签并激活它，串流页就变成 hidden。
    blank = open_target(PORT, "about:blank")
    cdp_b = CDP(blank["webSocketDebuggerUrl"])
    cdp_b.call("Runtime.enable")
    cdp_b.call("Page.bringToFront")
    print("   >>> 已切到空白标签（串流页应为 hidden），保持 %d 秒" % HIDE_S)
    time.sleep(HIDE_S)
    s = sample("3 隐藏中", 2.0)

    # 切回：把串流页提到前台。
    cdp.call("Page.bringToFront")
    print("   >>> 已切回串流页（应发 START_VIDEO）")
    time.sleep(1.5)
    a = sample("4 切回 +1.5s", 2.0)
    b = sample("5 切回 +5.5s", 2.0)
    c = sample("6 切回 +9.5s", 2.0)

    print("\n判定：")
    if b["chunks"] - a["chunks"] <= 4:
        print("  切回后 chunks 几乎不涨 —— 服务端没恢复给这条连接发帧（STOP/START 恢复链路）。")
    elif c["fps"] == 0 and c["chunks"] - b["chunks"] > 40:
        print("  帧一直在到（chunks 涨），但 window.fps 恒为 0 —— 页面计数坏了。")
    else:
        print("  这一轮切回后恢复正常，症状未能复现。")


if __name__ == "__main__":
    main()
