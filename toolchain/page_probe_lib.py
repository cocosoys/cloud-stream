#!/usr/bin/env python3
"""#115：无头/窗口化 Edge 的 CDP 小工具，给探针脚本共用。

page-probe.py 和 fps-watch.py 需要同一套东西：起一个和用户正在用的浏览器
**互不干扰**的 Edge（独立 user-data-dir + 独立调试端口）、开一个 target、用
裸 RFC 6455 说 CDP 的 WebSocket 协议、在"跑着串流核心的那个 frame"里求值。

抽出来是因为两份实现迟早会分叉：一边修好了"导航后旧上下文失效"的判断，
另一边还留着旧逻辑，于是同一个 bug 在两个探针里给出两种答案。
"""

import base64
import json
import os
import socket
import struct
import subprocess
import time
import urllib.parse
import urllib.request

DEFAULT_EDGE = r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"


def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        part = sock.recv(n - len(buf))
        if not part:
            raise EOFError("closed")
        buf += part
    return buf


class WS:
    """最小 WebSocket 客户端：只够说 CDP（文本帧、掩码上行、不分片）。"""

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
        self.contexts = {}
        self.dead = set()
        # 事件流。探针只关心"有没有"，所以这里只留方法名和字节数，不保留载荷 ——
        # 60 fps 的 1080p 帧全留在内存里会把探针自己变成瓶颈。
        self.events = []

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
            if msg.get("method"):
                p = msg.get("params") or {}
                resp = p.get("response") or {}
                self.events.append((msg["method"], resp.get("payloadData") or p.get("payloadData") or ""))
                continue
            if msg.get("id") == mid:
                if "error" in msg:
                    raise RuntimeError(f"{msg['error']}")
                return msg.get("result", {})

    def take_events(self, method):
        """取走某类事件，返回 [(method, payload)]。"""
        kept, out = [], []
        for ev in self.events:
            (out if ev[0] == method else kept).append(ev)
        self.events = kept
        return out

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
        return [cid for cid in reversed(list(self.contexts)) if cid not in self.dead]


def version(port):
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version", timeout=8) as r:
        return json.load(r)


def ensure_edge(port, profile, edge=DEFAULT_EDGE, headed=False, extra=None):
    """返回 True 表示这次新起了一个浏览器。窗口化时带 GPU，与用户手上的一致。"""
    try:
        version(port)
        return False
    except Exception:
        pass
    os.makedirs(profile, exist_ok=True)
    flags = [
        edge, f"--remote-debugging-port={port}",
        "--remote-allow-origins=*", f"--user-data-dir={profile}",
        "--ignore-certificate-errors", "--no-first-run", "--no-default-browser-check",
        "--autoplay-policy=no-user-gesture-required",
    ]
    if not headed:
        flags += ["--headless=new", "--disable-gpu"]
    flags += list(extra or [])
    flags.append("about:blank")
    subprocess.Popen(flags, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(80):
        time.sleep(0.25)
        try:
            version(port)
            return True
        except Exception:
            continue
    raise RuntimeError("无头 Edge 起不来")


def open_target(port, url):
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/json/new?" + urllib.parse.quote(url, safe=":/?=&"),
        method="PUT")
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.load(r)


def close_all_targets(port):
    """关掉本端口下所有 page 目标，免得上一轮的页面还在后台抢帧/抢租约。"""
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list", timeout=8) as r:
        targets = json.load(r)
    for t in targets:
        if t.get("type") != "page":
            continue
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/json/close/{t['id']}", timeout=5).read()
        except Exception:
            pass
