#!/usr/bin/env python3
"""量一条 WS 连接上的 `server_settings` 回执次数 —— 验证 #111 的边沿触发。

以前服务端**每收到一批 SETTINGS 就回一条** `server_settings`，而页面对每条回执
都会回写一整批，于是两边合起来是一个 400~550 批/秒的正反馈环（日志 4 GB/h、
下行一半带宽是 JSON、主线程抢不到第一帧）。修法：回执只在内容变化时发，且
100 ms 内完全相同的批直接丢弃。

这个脚本就是那个环的最小复现：

    python tmp-echo-probe.py [host] [port]

* 阶段 1：连上，读完首同步，然后连发 N 批**逐字节相同**的 SETTINGS。
  修复前：收到 ≈N 条回执。修复后：0 条。
* 阶段 2：发一批**真的改了**帧率的。修复前后都该收到 1 条。
* 阶段 3：改回去，同样 1 条 —— 回执记的是"最后发出去的是什么"，不是"发过没有"。
"""

import asyncio
import json
import ssl
import sys
import time
import urllib.request

import websockets

HOST = sys.argv[1] if len(sys.argv) > 1 else "192.168.59.148"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8080
BURST = int(sys.argv[3]) if len(sys.argv) > 3 else 200

CTX = ssl._create_unverified_context()
BASE = f"https://{HOST}:{PORT}/api/v1"


def api(path, method="GET"):
    req = urllib.request.Request(BASE + path, method=method)
    with urllib.request.urlopen(req, context=CTX, timeout=10) as resp:
        return json.loads(resp.read().decode() or "null")


async def main():
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    echoes = []

    async with websockets.connect(
        f"wss://{HOST}:{PORT}/api/websockets", ssl=ctx, max_size=None
    ) as ws:
        async def drain():
            try:
                async for msg in ws:
                    if isinstance(msg, str) and msg.startswith("{"):
                        obj = json.loads(msg)
                        if obj.get("type") == "server_settings":
                            echoes.append((time.monotonic(), obj["settings"]))
            except Exception:  # noqa: BLE001
                pass

        reader = asyncio.create_task(drain())
        await asyncio.sleep(1.2)

        # 设置批里带程序参数，`exclusive` 下需要租约；先把自己认出来再要过来。
        clients = api("/health").get("clients", {}).get("items", [])
        mine = next((c for c in clients if c.get("client_type") == "websocket"), None)
        if mine:
            api(f"/control/session/take/{mine['id']}", "POST")
            await asyncio.sleep(0.3)

        baseline = len(echoes)
        print(f"connected; base echoes after hello = {baseline}")

        # 阶段 1：同一批，连发 BURST 次，10 ms 间隔 —— 正是那个环的形状。
        repeat = json.dumps(
            {
                "tabId": "echo-probe",
                "framerate": 60,
                "audio_bitrate": 64000,
                "rate_control_mode": "cbr",
                "video_crf": 30,
                "manual_resolution": False,
            },
            separators=(",", ":"),
        )

        def diff(before_settings, after_settings):
            """两条回执之间真正变了的键 —— 用来解释"为什么这一条必须发"。"""
            keys = set(before_settings) | set(after_settings)
            out = []
            for k in sorted(keys):
                a = json.dumps(before_settings.get(k), sort_keys=True)
                b = json.dumps(after_settings.get(k), sort_keys=True)
                if a != b:
                    out.append(f"{k}: {a} -> {b}")
            return out

        # 阶段 0：先对齐一次。这一批**可能真的改了点东西**（比如 `audio_bitrate`
        # 的出厂默认不是 64000），所以它理应得到一条回执；把这一条单独算清楚，
        # 后面的 burst 才有干净的基线。
        before = len(echoes)
        await ws.send("SETTINGS," + repeat)
        await asyncio.sleep(0.8)
        warm = len(echoes) - before
        print(f"stage 0: warming batch -> {warm} echo(es)")
        if warm:
            changed = diff(echoes[before - 1][1], echoes[before][1])
            for line in changed[:8]:
                print(f"         changed: {line}")

        # 阶段 1：同样的字节再发 BURST 次。修复前这里会是 ≈BURST 条。
        baseline = len(echoes)
        t0 = time.monotonic()
        for _ in range(BURST):
            await ws.send("SETTINGS," + repeat)
            await asyncio.sleep(0.01)
        await asyncio.sleep(0.8)
        stage1 = len(echoes) - baseline
        span = time.monotonic() - t0
        print(f"stage 1: {BURST} identical batches in {span:.1f}s -> {stage1} echo(es)")

        # 阶段 2：真的改一档。
        before = len(echoes)
        await ws.send("SETTINGS," + json.dumps({"framerate": 30}, separators=(",", ":")))
        await asyncio.sleep(0.8)
        stage2 = len(echoes) - before
        print(f"stage 2: framerate 60 -> 30 -> {stage2} echo(es)")

        # 阶段 3：改回去。
        before = len(echoes)
        await ws.send("SETTINGS," + json.dumps({"framerate": 60}, separators=(",", ":")))
        await asyncio.sleep(0.8)
        stage3 = len(echoes) - before
        print(f"stage 3: framerate 30 -> 60 -> {stage3} echo(es)")

        if echoes:
            last = echoes[-1][1]
            print(
                "last echo: framerate=%s encoder=%s use_cpu=%s"
                % (
                    json.dumps(last.get("framerate")),
                    json.dumps(last.get("encoder")),
                    json.dumps(last.get("use_cpu")),
                )
            )

        verdict = "PASS" if (stage1 == 0 and stage2 >= 1 and stage3 >= 1) else "FAIL"
        print(f"VERDICT={verdict}")
        reader.cancel()


asyncio.run(main())
