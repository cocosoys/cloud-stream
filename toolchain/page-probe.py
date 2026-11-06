#!/usr/bin/env python3
"""#114 验证工具：用无头 Edge（CDP）读页面里的真实统计量。

为什么要有它：面板上的「客户端 FPS」不是服务端算的，而是页面自己算的
（`window.stream_stats.latest.fps`，由 `takeStreamSample` 每秒写一次）。
光看代码只能确定"应该"是多少，确定不了"实际"是多少 —— 而这个 bug 恰好
就藏在"应该"和"实际"之间：某条呈现路径没有给计数表记账，代码读起来一切
正常，跑起来恒为 0。

做法：起一个无头 Edge（独立 `--user-data-dir`，不会抢占用户正在用的窗口），
用 CDP 打开串流页，在**跑着串流核心的那个 frame** 里每秒取一次：

    window.fps                  ← 页面按"帧号变化"数出来的线上帧率
    window.videoChunksReceived  ← 真正到达页面的视频分片数（只增）
    window.videoDivertOn        ← 帧是否被分流给 decoder worker（绕开主线程）
    window.stream_stats.latest  ← 合并后的样本：encoded_fps(服务端) / fps(客户端)
    window.stream_client        ← 页面对自己解码链的描述（sink / decode_path）

拿到的组合直接指向病灶：
  * chunks 在涨、fps 恒 0   → 统计路径没记账（前端 bug）
  * chunks 不涨             → 帧没到页面（分流/过滤/网络）
  * latest.encoded_fps=60 而 latest.fps=0 → 面板上"服务端 60 / 客户端 0"的复现

用法：
    python page-probe.py [url] [采样秒数]
    python page-probe.py https://192.168.59.148:8080/ 8
"""

import base64
import json
import os
import socket
import struct
import subprocess
import sys
import time
import urllib.parse
import urllib.request

EDGE = r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"
PROFILE = os.path.join(os.environ.get("TEMP", r"C:\Windows\Temp"), "edge-cdp", "profile")
PORT = 9455
URL = sys.argv[1] if len(sys.argv) > 1 else "https://192.168.59.148:8080/"
SECONDS = float(sys.argv[2]) if len(sys.argv) > 2 else 8.0

SNAPSHOT_JS = r"""(() => {
  const s = window.stream_stats || {};
  const info = window.stream_info || null;
  return JSON.stringify({
    href: location.href,
    fps: window.fps,
    chunks: window.videoChunksReceived,
    divert: window.videoDivertOn,
    statsOpen: !!s.open,
    latest: s.latest || null,
    hist: (s.history || []).slice(-3),
    client: window.stream_client || null,
    info: info && info.data ? info.data : info,
    hidden: document.hidden,
    hasVideo: !!document.querySelector('video'),
    hasCanvas: !!document.querySelector('canvas'),
    videoPaused: (() => { const v = document.querySelector('video'); return v ? v.paused : null; })(),
    videoSize: (() => { const v = document.querySelector('video'); return v ? [v.videoWidth, v.videoHeight] : null; })(),
  });
})()"""

CORE_MARK = "typeof window.videoChunksReceived"


def http_json(path):
    with urllib.request.urlopen(f"http://127.0.0.1:{PORT}{path}", timeout=8) as r:
        return json.load(r)


def ensure_edge():
    try:
        http_json("/json/version")
        return False
    except Exception:
        pass
    os.makedirs(PROFILE, exist_ok=True)
    subprocess.Popen([
        EDGE, "--headless=new", f"--remote-debugging-port={PORT}",
        "--remote-allow-origins=*", f"--user-data-dir={PROFILE}",
        "--ignore-certificate-errors", "--no-first-run", "--no-default-browser-check",
        "--disable-gpu", "--autoplay-policy=no-user-gesture-required", "about:blank",
    ], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(60):
        time.sleep(0.25)
        try:
            http_json("/json/version")
            return True
        except Exception:
            continue
    print("RESULT fail 无头 Edge 起不来")
    sys.exit(1)


# ---------------------------------------------------------------- RFC 6455

def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        part = sock.recv(n - len(buf))
        if not part:
            raise EOFError("closed")
        buf += part
    return buf


class WS:
    def __init__(self, url):
        _, rest = url.split("://", 1)
        hostport, path = rest.split("/", 1)
        host, port = hostport.split(":")
        self.sock = socket.create_connection((host, int(port)), timeout=20)
        key = base64.b64encode(os.urandom(16)).decode()
        req = (
            f"GET /{path} HTTP/1.1\r\nHost: {hostport}\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
            f"Sec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(req.encode())
        head = b""
        while b"\r\n\r\n" not in head:
            head += self.sock.recv(1)
        if b"101" not in head.split(b"\r\n", 1)[0]:
            raise RuntimeError(head[:200])

    def send(self, text):
        payload = text.encode()
        mask = os.urandom(4)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        n = len(payload)
        if n < 126:
            header = bytes([0x81, 0x80 | n])
        elif n < 65536:
            header = bytes([0x81, 0x80 | 126]) + struct.pack(">H", n)
        else:
            header = bytes([0x81, 0x80 | 127]) + struct.pack(">Q", n)
        self.sock.sendall(header + mask + masked)

    def recv(self):
        b0, b1 = recv_exact(self.sock, 2)
        length = b1 & 0x7F
        if length == 126:
            length = struct.unpack(">H", recv_exact(self.sock, 2))[0]
        elif length == 127:
            length = struct.unpack(">Q", recv_exact(self.sock, 8))[0]
        return recv_exact(self.sock, length).decode(errors="replace")


class CDP:
    def __init__(self, ws_url):
        self.ws = WS(ws_url)
        self.next_id = 0
        self.contexts = {}   # executionContextId -> frameId
        self.dead = set()    # 已经被销毁的上下文（导航会重造，旧 id 必须剔除）

    def _pump(self, mid):
        while True:
            msg = json.loads(self.ws.recv())
            if msg.get("method") == "Runtime.executionContextCreated":
                ctx = msg["params"]["context"]
                self.contexts[ctx["id"]] = ctx.get("auxData", {}).get("frameId")
                self.dead.discard(ctx["id"])
                continue
            if msg.get("method") == "Runtime.executionContextDestroyed":
                self.dead.add(msg["params"]["executionContextId"])
                continue
            if msg.get("method") == "Runtime.executionContextsCleared":
                self.dead.update(self.contexts)
                continue
            if msg.get("id") == mid:
                if "error" in msg:
                    raise RuntimeError(f"{msg['error']}")
                return msg.get("result", {})

    def call(self, method, params=None):
        self.next_id += 1
        mid = self.next_id
        self.ws.send(json.dumps({"id": mid, "method": method, "params": params or {}}))
        return self._pump(mid)

    def evaluate(self, expression, context_id=None):
        params = {"expression": expression, "returnByValue": True, "awaitPromise": True}
        if context_id is not None:
            params["contextId"] = context_id
        result = self.call("Runtime.evaluate", params)
        if result.get("exceptionDetails"):
            raise RuntimeError(json.dumps(result["exceptionDetails"])[:200])
        return result["result"].get("value")

    def live_contexts(self):
        """活着的上下文，新造的排前面 —— 导航后旧的最先失效。"""
        return [cid for cid in reversed(list(self.contexts)) if cid not in self.dead]


def open_target(url):
    req = urllib.request.Request(
        f"http://127.0.0.1:{PORT}/json/new?" + urllib.parse.quote(url, safe=":/?=&"), method="PUT")
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.load(r)


def fmt(value):
    return "-" if value is None else str(value)


def main():
    ensure_edge()
    target = open_target(URL)
    print(f"target: {target.get('id')} -> {URL}")
    cdp = CDP(target["webSocketDebuggerUrl"])
    cdp.call("Runtime.enable")
    cdp.call("Page.enable")

    # 串流核心跑在哪个 frame 里？先等它有上下文，再逐个问"你认识 videoChunksReceived 吗"。
    core_ctx = None
    deadline = time.time() + 45
    while time.time() < deadline and core_ctx is None:
        time.sleep(1.0)
        for ctx in cdp.live_contexts():
            try:
                if cdp.evaluate(CORE_MARK, ctx) == "number":
                    core_ctx = ctx
                    break
            except Exception:
                continue
    if core_ctx is None:
        print("RESULT fail 没有找到跑着串流核心的 frame")
        for ctx in cdp.live_contexts():
            try:
                print(f"  ctx {ctx}: {cdp.evaluate('location.href', ctx)} "
                      f"fps={cdp.evaluate('typeof window.fps', ctx)}")
            except Exception:
                pass
        sys.exit(1)
    print(f"core context: {core_ctx} ({cdp.evaluate('location.href', core_ctx)})")

    # 等帧真的开始到：`videoChunksReceived` 只增。
    deadline = time.time() + 40
    last = None
    while time.time() < deadline:
        time.sleep(1.0)
        try:
            last = json.loads(cdp.evaluate(SNAPSHOT_JS, core_ctx))
        except Exception as exc:
            print(f"  (等待串流: {str(exc)[:80]})")
            continue
        if (last.get("chunks") or 0) > 0:
            break
    if not last:
        print("RESULT fail 页面没有产出任何可读的统计量")
        sys.exit(1)

    rows = []
    t0 = time.time()
    while time.time() - t0 < SECONDS:
        snap = json.loads(cdp.evaluate(SNAPSHOT_JS, core_ctx))
        rows.append((round(time.time() - t0, 1), snap))
        time.sleep(1.0)

    print("\n t     fps  chunks  divert  latest.fps  encoded_fps  statsOpen")
    for t, s in rows:
        latest = s.get("latest") or {}
        print(
            f"{t:4.1f}  {fmt(s.get('fps')):>4}  {fmt(s.get('chunks')):>6}  "
            f"{fmt(s.get('divert'))[:5]:>5}  {fmt(latest.get('fps')):>10}  "
            f"{fmt(latest.get('encoded_fps')):>11}  {s.get('statsOpen')}"
        )

    first, last_snap = rows[0][1], rows[-1][1]
    d_chunks = (last_snap.get("chunks") or 0) - (first.get("chunks") or 0)
    info = last_snap.get("info") or {}
    client = last_snap.get("client") or {}
    latest = last_snap.get("latest") or {}
    print(f"\n分片增量 {d_chunks} / {SECONDS:.0f}s，window.fps={fmt(last_snap.get('fps'))}")
    print(f"编码器={fmt(info.get('encoder'))} codec={fmt(info.get('codec'))} "
          f"sink={fmt(client.get('sink'))} path={fmt(client.get('decode_path'))}")
    print(f"面板: 客户端 fps={fmt(latest.get('fps'))} 服务端 encoded_fps={fmt(latest.get('encoded_fps'))}")
    print("原始快照:")
    print(json.dumps(last_snap, ensure_ascii=False, indent=1)[:2500])

    if d_chunks <= 0:
        print("\nRESULT fail 分片不涨 —— 帧没到页面")
        sys.exit(1)
    if (last_snap.get("fps") or 0) == 0:
        print("\nRESULT fail 帧到了页面，但 window.fps 恒为 0（计数没记账）")
        sys.exit(1)
    print("\nRESULT ok 客户端统计正常")
    sys.exit(0)


if __name__ == "__main__":
    main()
