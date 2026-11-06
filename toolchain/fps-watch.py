#!/usr/bin/env python3
"""#115 复现探针：盯着一个真实/无头的串流页，每秒打印客户端侧的全部计数。

为什么不用 page-probe.py：那个脚本只回答"现在是不是正常"，而这次要回答的是
**变化过程** —— 用户报的是"操作桌面之后 FPS 从 60 一路跌到 0，然后在 0/1 抖动"。
一路下跌这个形状本身就是证据：它只能来自"分子停止增长、分母继续变大"，也就是
某个计数在某时刻停止记账，而面板还在按秒重算。要看到它，就得在动作前后连续采样。

采的字段（都来自页面，不是服务端）：
    fps       window.fps —— 页面按"帧号变化"数出来的线上帧率
    chunks    window.videoChunksReceived —— 到达页面的 0x03/0x04 条数（只增）
    divert    window.videoDivertOn —— 帧是否绕开主线程直接进 decoder worker
    frameMs   距离上一帧到达页面多久（ms），判"帧停了"还是"计数停了"
    outMs     距离上一帧被呈现多久（ms）—— 由 pageShown/videoWatch 记账
    size      <video> 的 videoWidth x videoHeight

用法：
    python fps-watch.py [url] [秒数] [--headed]
    python fps-watch.py "https://192.168.59.148:8080/?transport=websockets" 20
"""

import json
import os
import subprocess
import sys
import time
import urllib.parse
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from page_probe_lib import CDP, ensure_edge, open_target  # noqa: E402  兼容下划线命名

EDGE = r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"
PORT = 9456
PROFILE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "edge-cdp-fps", "profile")

args = [a for a in sys.argv[1:] if not a.startswith("--")]
HEADED = "--headed" in sys.argv
URL = args[0] if len(args) > 0 else "https://192.168.59.148:8080/?transport=websockets"
SECONDS = float(args[1]) if len(args) > 1 else 20.0

SNAPSHOT = r"""(() => {
  const v = document.querySelector('video');
  const now = performance.now();
  return JSON.stringify({
    fps: window.fps,
    chunks: window.videoChunksReceived,
    divert: window.videoDivertOn,
    rows: window.videoStripeRows,
    outAge: window.__lastOutAt ? Math.round(now - window.__lastOutAt) : null,
    mode: window.currentEncoderMode || null,
    sink: (window.stream_client || {}).sink || null,
    paused: v ? v.paused : null,
    size: v ? [v.videoWidth, v.videoHeight] : null,
    hidden: document.hidden,
  });
})()"""


def main():
    # outAge 需要"最后一帧被呈现"的时刻。核心把它放在闭包里（lastVideoOutputAt），
    # 页面拿不到；这里用 `<video>` 的 requestVideoFrameCallback 自己记一份等价物 ——
    # 它只在**真的有一帧进入合成器**时回调，正好能区分"帧到了但没呈现"与"帧没到"。
    hook = r"""(() => {
  if (window.__fpsWatchHooked) return 'already';
  window.__fpsWatchHooked = true;
  window.__lastOutAt = 0;
  const v = document.querySelector('video');
  if (v && typeof v.requestVideoFrameCallback === 'function') {
    const tick = () => { window.__lastOutAt = performance.now(); v.requestVideoFrameCallback(tick); };
    v.requestVideoFrameCallback(tick);
  }
  return 'ok';
})()"""

    started = ensure_edge(PORT, PROFILE, EDGE, headed=HEADED,
                          extra=["--window-size=1920,1080"] if HEADED else None)
    target = open_target(PORT, URL)
    print(f"edge {'headed' if HEADED else 'headless'} (spawned={started}) target={target.get('id')}")
    cdp = CDP(target["webSocketDebuggerUrl"])
    cdp.call("Runtime.enable")
    cdp.call("Page.enable")
    # 视口固定成 1080p：用户看到的就是这个尺寸，分辨率一变服务端就重开编码器，
    # 那会把"静止时会不会掉到 0"变成"换分辨率那一秒发生了什么"。
    try:
        cdp.call("Emulation.setDeviceMetricsOverride",
                 {"width": 1920, "height": 1080, "deviceScaleFactor": 1, "mobile": False})
    except Exception as exc:
        print(f"(视口覆盖失败: {str(exc)[:60]})")

    core = None
    deadline = time.time() + 45
    while time.time() < deadline and core is None:
        time.sleep(1.0)
        for cid in cdp.live_contexts():
            try:
                if cdp.evaluate("typeof window.videoChunksReceived", cid) == "number":
                    core = cid
                    break
            except Exception:
                continue
    if core is None:
        print("RESULT fail 找不到串流核心 frame")
        sys.exit(1)
    print(f"core ctx {core} url={cdp.evaluate('location.href', core)}")
    print(cdp.evaluate(hook, core))

    time.sleep(12)  # 等首帧、等统计定时器起来
    print("\n   t   fps  chunks  divert  outAge  size      note")
    t0 = time.time()
    prev_chunks = None
    while time.time() - t0 < SECONDS:
        try:
            s = json.loads(cdp.evaluate(SNAPSHOT, core))
        except Exception as exc:
            print(f"  (上下文失效: {str(exc)[:60]})")
            break
        d = "-" if prev_chunks is None else s["chunks"] - prev_chunks
        prev_chunks = s["chunks"]
        print(f"{time.time() - t0:5.1f} {str(s['fps']):>5} {s['chunks']:>7}  "
              f"{str(s['divert'])[:5]:>5} {str(s['outAge']):>6}  "
              f"{str(s['size']):>11}  +{d} mode={s['mode']} sink={s['sink']}")
        time.sleep(1.0)


if __name__ == "__main__":
    main()
