#!/usr/bin/env python3
"""#115 复现工具：在串流页里真的"操作桌面"，同时盯住客户端的 FPS 计数。

用户报的动作是"右击桌面 → 刷新"，所以这里派发的就是同一条链路：CDP 往页面
视口里送**真实的 DOM 鼠标事件**，页面把它翻译成 `m,x,y` / 按键上行的同样的
字节，服务端注入被控机。这条路上没有一步是模拟的绕过 —— 只有手不是人的手。

参数：
    python desktop-act.py [--port 9456] [--seconds 18] [--act rightclick|move|none]

输出每秒一行 fps / chunks / divert / outAge，动作发生的那一秒标 `<<< ACT`。
"""

import json
import os
import subprocess
import sys
import time

sys.path.insert(0, __file__.rsplit("\\", 1)[0])
from page_probe_lib import CDP, close_all_targets, ensure_edge, open_target, version  # noqa: E402

PORT = 9456
SECONDS = 18
ACT = "rightclick"
URL = "https://192.168.59.148:8080/?transport=websockets"
PROFILE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "edge-cdp-act", "profile")

argv = sys.argv[1:]
for i, a in enumerate(argv):
    if a == "--port":
        PORT = int(argv[i + 1])
    elif a == "--seconds":
        SECONDS = float(argv[i + 1])
    elif a == "--act":
        ACT = argv[i + 1]
    elif a == "--url":
        URL = argv[i + 1]

SNAPSHOT = r"""(() => {
  const v = document.querySelector('video');
  const now = performance.now();
  return JSON.stringify({
    fps: window.fps,
    chunks: window.videoChunksReceived,
    divert: window.videoDivertOn,
    outAge: window.__lastOutAt ? Math.round(now - window.__lastOutAt) : null,
    size: v ? [v.videoWidth, v.videoHeight] : null,
    vis: document.visibilityState + (document.hidden ? '/hidden' : '/shown'),
    focus: document.hasFocus(),
    lastLog: (window.__logs || []).slice(-1)[0] || null,
    raw: window.__raw ? window.__raw.perSec : null,
  });
})()"""

HOOK = r"""(() => {
  if (window.__actHooked) return 'already';
  window.__actHooked = true;
  window.__lastOutAt = 0;
  window.__logs = [];
  const orig = console.log;
  console.log = function (...a) {
    try {
      window.__logs.push(a.map((x) => (typeof x === 'string' ? x : JSON.stringify(x))).join(' '));
      if (window.__logs.length > 120) window.__logs.shift();
    } catch (e) { /* never break the page */ }
    return orig.apply(console, a);
  };
  const v = document.querySelector('video');
  if (v && typeof v.requestVideoFrameCallback === 'function') {
    const tick = () => { window.__lastOutAt = performance.now(); v.requestVideoFrameCallback(tick); };
    v.requestVideoFrameCallback(tick);
  }
  if (window.__wantRawWs) {
    // 对照连接：页面自己的网络栈、自己的 TLS、自己的线程，但**不经过**核心的
    // socket worker 与分流逻辑。它每秒收到多少个 0x04，和核心 socket 的数字
    // 摆在一起，"浏览器收不到"与"核心没记"就分开了。
    window.__raw = { frames: 0, last: 0, perSec: 0, open: false };
    const proto = location.protocol === 'https:' ? 'wss://' : 'ws://';
    const raw = new WebSocket(proto + location.host + '/api/websockets');
    raw.binaryType = 'arraybuffer';
    raw.onopen = () => { window.__raw.open = true; };
    raw.onmessage = (e) => {
      if (e.data instanceof ArrayBuffer && new Uint8Array(e.data, 0, 1)[0] === 0x04) window.__raw.frames++;
    };
    setInterval(() => {
      window.__raw.perSec = window.__raw.frames - window.__raw.last;
      window.__raw.last = window.__raw.frames;
    }, 1000);
  }
  return 'ok';
})()"""


def find_page(port):
    import urllib.request
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list", timeout=8) as r:
        targets = json.load(r)
    for t in targets:
        if t.get("type") == "page" and "192.168" in (t.get("url") or ""):
            return t
    raise SystemExit("没有找到串流页 target（先跑 fps-watch.py --headed）")


def core_context(cdp):
    for cid in cdp.live_contexts():
        try:
            if cdp.evaluate("typeof window.videoChunksReceived", cid) == "number":
                return cid
        except Exception:
            continue
    return None


def mouse(cdp, kind, x, y, button="none", clicks=0):
    cdp.call("Input.dispatchMouseEvent", {
        "type": kind, "x": x, "y": y, "button": button,
        "clickCount": clicks, "pointerType": "mouse",
    })


def right_click(cdp, x, y):
    """一次真正的右键：移动 → 按下 → 抬起。页面据此发 `mb`/`m,` 的同一串字节。"""
    mouse(cdp, "mouseMoved", x, y)
    time.sleep(0.05)
    mouse(cdp, "mousePressed", x, y, "right", 1)
    time.sleep(0.08)
    mouse(cdp, "mouseReleased", x, y, "right", 1)


def press_escape(cdp):
    for kind in ("keyDown", "keyUp"):
        cdp.call("Input.dispatchKeyEvent", {
            "type": kind, "key": "Escape", "code": "Escape",
            "windowsVirtualKeyCode": 27, "nativeVirtualKeyCode": 27,
        })


def health():
    """服务端同一秒的自述：它编了多少帧、每帧多久、发了多少字节。

    页面计数说"我到手多少"，这一份说"服务端发出多少" —— 只有两份并排，
    "客户端 0 / 服务端 60" 才能被拆成"没发"与"发了没收"。
    """
    try:
        out = subprocess.run(
            ["ssh", "mc-admin", "curl.exe -sk https://127.0.0.1:8080/api/v1/health"],
            capture_output=True, text=True, timeout=15).stdout
        return json.loads(out)
    except Exception:
        return None


def main():
    fresh = False
    try:
        version(PORT)
    except Exception:
        # 没有现成的浏览器：自己起一个**窗口化**的（无头环境验证过一切正常，
        # 而用户手里是带 GPU 的真实窗口 —— 差异只能在这里复现）。
        ensure_edge(PORT, PROFILE, headed=True, extra=["--window-size=1920,1080"])
        fresh = True
    # 总是新开一个页面：上一轮的页面可能已经被 STOP_VIDEO 停在原地，复用它
    # 等于从一个"已经被判过死刑"的会话上读数字。
    close_all_targets(PORT)
    open_target(PORT, URL)
    time.sleep(2)
    target = find_page(PORT)
    cdp = CDP(target["webSocketDebuggerUrl"])
    cdp.call("Runtime.enable")
    if "--emulate" in sys.argv:
        try:
            cdp.call("Emulation.setDeviceMetricsOverride",
                     {"width": 1920, "height": 1080, "deviceScaleFactor": 1, "mobile": False})
        except Exception:
            pass
    core = None
    deadline = time.time() + 45
    while time.time() < deadline and core is None:
        time.sleep(1.0)
        core = core_context(cdp)
    if core is None:
        raise SystemExit("找不到串流核心 frame")
    print(f"target={target['id']} ctx={core} act={ACT} {SECONDS:.0f}s")
    if "--raw" in sys.argv:
        cdp.evaluate("window.__wantRawWs = true", core)
    print(cdp.evaluate(HOOK, core))
    # 网络层证据：CDP 报告的 WebSocket 下行帧。它与页面计数是两个独立的口径 ——
    # 一个说"浏览器收到了"，一个说"页面记了账"。两者分开才能定位是谁掉了。
    try:
        cdp.call("Network.enable")
    except Exception as exc:
        print(f"(Network 域不可用: {str(exc)[:60]})")
    try:
        cdp.call("Page.bringToFront")
    except Exception:
        pass
    time.sleep(8)  # 让首帧、统计定时器、分辨率都稳定下来再开始记

    print("\n   t   fps  chunks  srv  enc_ms  raw  note")
    t0 = time.time()
    acted = False
    prev = None
    prev_srv = None
    prev_at = None
    while time.time() - t0 < SECONDS:
        elapsed = time.time() - t0
        s = json.loads(cdp.evaluate(SNAPSHOT, core))
        h = health()
        now = time.time()
        srv_fps, enc_ms = "-", "-"
        if h and h.get("stream"):
            st = h["stream"]
            ft = st.get("frames_total") or {}
            total = (ft.get("new") or 0) + (ft.get("repeat") or 0)
            if prev_srv is not None and prev_at and now > prev_at:
                srv_fps = f"{(total - prev_srv) / (now - prev_at):.0f}"
            prev_srv, prev_at = total, now
            enc_ms = ((st.get("timing") or {}).get("encode_ms")) or "-"
        d = "-" if prev is None else s["chunks"] - prev
        prev = s["chunks"]
        note = f"+{d}"
        if not acted and elapsed >= 5.0:
            acted = True
            # 视口正中偏下：桌面右键菜单一定弹得出来的地方。
            w, h = s.get("size") or [1920, 1080]
            if ACT == "drag":
                mouse(cdp, "mouseMoved", 200, 200)
                mouse(cdp, "mousePressed", 200, 200, "left", 1)
                note += "   <<< DRAG START"
            else:
                right_click(cdp, w / 2, h * 0.72)
                note += "   <<< RIGHT CLICK"
            if ACT == "wiggle":
                # 把右键菜单收掉，否则后面那些移动只是在菜单项上打转。
                press_escape(cdp)
                time.sleep(0.3)
        print(f"{elapsed:5.1f} {str(s['fps']):>5} {s['chunks']:>7} {str(srv_fps):>4} "
              f"{str(enc_ms):>7} {str(s.get('raw')):>4}  {note}")
        if s.get("lastLog"):
            print(f"        log: {s['lastLog'][:140]}")
        if ACT == "drag" and acted:
            # 拖着走：桌面上的框选矩形每移动一次就重画一大片，采集到的每一拍
            # 都是**大面积**变化，而不是光标那种几十像素的小补丁。这是"在桌面
            # 上操作"里对编码器最贵的一种动作。
            w, h = s.get("size") or [1920, 1080]
            step = int(elapsed * 10)
            x = 200 + (step * 37) % int(w * 0.6)
            y = 200 + (step * 23) % int(h * 0.5)
            mouse(cdp, "mouseMoved", x, y)
            time.sleep(0.1)
        elif ACT == "wiggle" and acted:
            # 一秒里挪五次：每次移动都让被控机的光标换位置，于是采集到的每一拍
            # 都是"新帧"，而不是能几毫秒编完的 repeat。
            w, h = s.get("size") or [1920, 1080]
            for k in range(5):
                x = w / 2 + (90 if (k + int(elapsed)) % 2 else -90)
                y = h * 0.6 + (70 if k % 2 else -70)
                mouse(cdp, "mouseMoved", x, y)
                time.sleep(0.12)
        else:
            time.sleep(1.0)

    if ACT == "drag":
        # 松开左键。脚本被 timeout 打断时这一句不会执行，但被控机的按钮状态
        # 会在下一条连接的第一次点击里重新同步，不会把桌面留在按下状态。
        try:
            mouse(cdp, "mouseReleased", 200, 200, "left", 1)
        except Exception:
            pass


if __name__ == "__main__":
    main()
