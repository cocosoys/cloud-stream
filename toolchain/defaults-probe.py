"""#117 回归：一个**从没打开过这个站点**的浏览器，首屏应当落在
WebSockets + VP9 上。

为什么要单独一个探针：`--mode` / `--video-codec` 只是服务端的起点，
`main.jsx` 会把它写进 localStorage，面板再从 `server_settings` 把编码器
回显成**正在跑**的那个。三处任意一处写错，界面上都只表现为"默认变了"
或者"默认没变"，看不出是哪一层。这里一次把三层都读出来：

* `/api/status` → `current_mode`（服务端默认）
* `localStorage['<ns>_stream_mode']`（页面 seeding 的结果）
* `#streamModeSelect` / `#encoderSelect` 的 value（面板真正显示给用户的值）

用法：
    python defaults-probe.py [--port 9470] [--url https://192.168.59.148:8080/]
"""

import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from page_probe_lib import CDP, ensure_edge, open_target, version  # noqa: E402

PORT = 9470
URL = "https://192.168.59.148:8080/"

argv = sys.argv[1:]
for i, a in enumerate(argv):
    if a == "--port":
        PORT = int(argv[i + 1])
    elif a == "--url":
        URL = argv[i + 1]

# 带时间戳的一次性 profile：上一次跑剩的 localStorage 会把"默认"变成
# "上次的选择"，那样量到的就不是默认值了。
PROFILE = os.path.join(
    os.environ.get("TEMP", r"C:\Users\user\AppData\Local\Temp"),
    f"selkies-defaults-{int(time.time())}",
)

SNAPSHOT = r"""(() => {
  const pick = (sel) => {
    const el = document.querySelector(sel);
    return el ? el.value : null;
  };
  const mode = Object.keys(localStorage).find((k) => k.endsWith('_stream_mode'));
  const enc = Object.keys(localStorage).find((k) => k.endsWith('_encoder'));
  return JSON.stringify({
    storedMode: mode ? localStorage.getItem(mode) : null,
    storedEncoder: enc ? localStorage.getItem(enc) : null,
    panelMode: pick('#streamModeSelect'),
    panelEncoder: pick('#encoderSelect'),
    transport: window.selkiesTransport ? window.selkiesTransport.readyState : null,
    fps: window.fps,
  });
})()"""

# 面板默认值只有把侧边栏和"视频设置"分组展开之后才在 DOM 里。这一步等的是
# 界面自己渲染，不是改状态 —— 读到什么就说明用户点开时会看到什么。
OPEN_PANEL = r"""(() => {
  if (document.querySelector('#settings-content')) return 'ready';
  const header = document.querySelector('[aria-controls="settings-content"]');
  if (header) { header.click(); return 'opened-section'; }
  const handle = document.querySelector('.toggle-handle');
  if (handle) { handle.click(); return 'opened-sidebar'; }
  return 'waiting';
})()"""


def main():
    ensure_edge(PORT, PROFILE, headed=False)
    target = open_target(PORT, URL)
    print(f"edge headless profile={PROFILE}")
    print(f"target={target.get('id')} url={URL}")
    cdp = CDP(target["webSocketDebuggerUrl"])
    cdp.call("Runtime.enable")

    deadline = time.time() + 40
    seen = None
    step = None
    while time.time() < deadline:
        time.sleep(1.0)
        try:
            # 不带 contextId：CDP 在主 frame 的默认上下文里求值，而 localStorage
            # 与面板的两个 <select> 都在主文档上。
            step = cdp.evaluate(OPEN_PANEL)
            raw = cdp.evaluate(SNAPSHOT)
        except Exception as err:  # 页面还在导航时 context 会短暂失效
            print(f"  (retry: {err})")
            continue
        if not raw:
            continue
        seen = json.loads(raw)
        if seen["panelMode"] and seen["panelEncoder"]:
            break
    print(f"panel step: {step}")
    print(json.dumps(seen or {}, ensure_ascii=False, indent=2))

    if not seen:
        raise SystemExit("页面没给出快照")
    verdict = []
    verdict.append(("storedMode", seen["storedMode"], "websockets"))
    verdict.append(("panelMode", seen["panelMode"], "websockets"))
    verdict.append(("panelEncoder", seen["panelEncoder"], "vp9enc"))
    ok = True
    for name, got, want in verdict:
        mark = "OK " if got == want else "BAD"
        if got != want:
            ok = False
        print(f"  {mark} {name} = {got!r} (want {want!r})")
    print("默认值：" + ("符合 #117" if ok else "不符合 #117"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
