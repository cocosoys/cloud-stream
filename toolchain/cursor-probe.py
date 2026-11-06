#!/usr/bin/env python3
"""量一条 WS 连接上到底有没有 `cursor,` —— 验证 #111 的光标下行。

诊断：WS 这一版从做出来就**一个管线事件都没订**，于是被控机的指针形状永远到
不了页面。页面的 `updateServerCursor` 因此从没被调用过，指针只会是浏览器默认
箭头（或干脆没有）—— 而 WebRTC 那条线一直是有的，所以这是"只有 WS 模式"的
独有缺口。

    python toolchain/cursor-probe.py [host] [port] [seconds]

判据：
* 收到至少一条 `cursor,`，且 JSON 里有 `curdata/handle/width/height/hotx/hoty`；
* 同一条连接上还有二进制帧（证明只是光标缺，不是整条视频不通）。
"""

import asyncio
import json
import os
import ssl
import sys
import time

import websockets

HOST = sys.argv[1] if len(sys.argv) > 1 else "192.168.59.148"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8080
SECONDS = float(sys.argv[3]) if len(sys.argv) > 3 else 8.0
# 本地调试服不带 TLS（`--tls-cert` 缺省就是明文），一键切换。
PLAIN = os.environ.get("WSPLAIN") == "1"
SCHEME = "ws" if PLAIN else "wss"


async def main():
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE

    cursors = []
    texts = []
    binary = 0
    binary_bytes = 0

    async with websockets.connect(
        f"{SCHEME}://{HOST}:{PORT}/api/websockets",
        ssl=None if PLAIN else ctx,
        max_size=None,
    ) as ws:
        async def drain():
            nonlocal binary, binary_bytes
            try:
                async for msg in ws:
                    if isinstance(msg, str):
                        if msg.startswith("cursor,"):
                            cursors.append((time.monotonic(), msg[7:]))
                        else:
                            texts.append(msg[:40])
                    else:
                        binary += 1
                        binary_bytes += len(msg)
            except Exception:  # noqa: BLE001
                pass

        reader = asyncio.create_task(drain())
        await asyncio.sleep(1.0)
        hello = list(texts)
        print(f"hello: {hello}")

        # 页面进来就是这条：先开会话的流，再看光标。
        await ws.send("START_VIDEO")
        t0 = time.monotonic()
        while time.monotonic() - t0 < SECONDS:
            await asyncio.sleep(0.25)

        print(f"binary frames in {SECONDS:.0f}s: {binary} ({binary_bytes} B)")
        print(f"cursor lines: {len(cursors)}")
        if cursors:
            first = json.loads(cursors[0][1])
            print(
                "first cursor: handle=%s %sx%s hot=(%s,%s) png=%d b64 chars"
                % (
                    first.get("handle"),
                    first.get("width"),
                    first.get("height"),
                    first.get("hotx"),
                    first.get("hoty"),
                    len(first.get("curdata") or ""),
                )
            )
            keys = sorted(first)
            print(f"keys: {keys}")
        others = [t for t in texts if not t.startswith("stream_stats")]
        print(f"other text ({len(others)}): {others[:12]}")

        shape_ok = bool(cursors) and all(
            k in json.loads(cursors[0][1])
            for k in ("curdata", "handle", "width", "height", "hotx", "hoty")
        )
        verdict = "PASS" if (shape_ok and binary > 0) else "FAIL"
        print(f"VERDICT={verdict}")
        reader.cancel()


asyncio.run(main())
