#!/usr/bin/env python3
"""像页面那样往 /api/websockets 推 0x02 麦克风帧，用来验虚拟麦克风链路。

把一段 440 Hz 正弦编成 24 kHz 单声道 Opus（20 ms 一帧），按页面的节奏
`[0x02] + packet` 发上去，同时把服务端回的文本读出来。配合被控机上的
`mic-probe.exe`：一边推、一边从 CABLE Output 读，才能证明"声音真的进了线缆"。

    python mic-push.py [host] [port] [秒数]

默认 `exclusive` 输入策略下，若已有页签持租约，这条新连接是 viewer，`0x02` 会被回
`MICROPHONE_DISABLED`。所以脚本连上后先用 `POST /api/v1/control/session/take/<id>`
把自己变成 controller，测完再把租约还给原来那条连接 —— 借租约期间被控机的键鼠是
属于这个脚本的，别在别人正在操作时跑。
"""

import asyncio
import json
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.request

import websockets

HOST = sys.argv[1] if len(sys.argv) > 1 else "192.168.59.148"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8080
SECONDS = float(sys.argv[3]) if len(sys.argv) > 3 else 8.0

CTX = ssl._create_unverified_context()
BASE = f"https://{HOST}:{PORT}/api/v1"


def api(path, method="GET"):
    req = urllib.request.Request(BASE + path, method=method)
    with urllib.request.urlopen(req, context=CTX, timeout=10) as resp:
        return json.loads(resp.read().decode() or "null")


def take_lease(session_id):
    """把输入租约交给这条会话。exclusive 策略下这是"能不能开麦"的前提。"""
    try:
        print(f"take lease -> session {session_id}: {api(f'/control/session/take/{session_id}', 'POST')}")
    except Exception as exc:  # noqa: BLE001
        print(f"take lease failed: {exc}")


def parse_ogg_packets(path):
    """把一个 Ogg 文件拆回 Opus 包（跳过 OpusHead / OpusTags）。"""
    data = open(path, "rb").read()
    packets, i = [], 0
    while True:
        j = data.find(b"OggS", i)
        if j < 0:
            break
        nsegs = data[j + 26]
        seg_table = data[j + 27 : j + 27 + nsegs]
        body = j + 27 + nsegs
        cur = b""
        for lace in seg_table:
            cur += data[body : body + lace]
            body += lace
            if lace < 255:
                packets.append(cur)
                cur = b""
        i = body
    return [p for p in packets if not p.startswith((b"OpusHead", b"OpusTags"))]


async def main():
    tmp = tempfile.NamedTemporaryFile(suffix=".ogg", delete=False).name
    subprocess.run(
        [
            "ffmpeg", "-v", "quiet", "-y",
            "-f", "lavfi",
            "-i", f"sine=frequency=440:sample_rate=24000:duration={SECONDS}",
            "-ac", "1", "-c:a", "libopus", "-b:a", "32k",
            "-frame_duration", "20", "-f", "ogg", tmp,
        ],
        check=True,
    )
    packets = parse_ogg_packets(tmp)
    print(f"encoded {len(packets)} opus packets ({len(packets) * 20} ms)")

    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    uri = f"wss://{HOST}:{PORT}/api/websockets"
    async with websockets.connect(uri, ssl=ctx, max_size=None) as ws:
        print("connected")

        async def drain():
            try:
                async for msg in ws:
                    if isinstance(msg, str):
                        print(f"  <- {msg[:160]}")
            except Exception as exc:  # noqa: BLE001
                print(f"  drain stopped: {exc}")

        drainer = asyncio.create_task(drain())
        await asyncio.sleep(1.5)

        # 这条连接还没拿到租约时，服务端会回 MICROPHONE_DISABLED。先把自己找出来
        # （health 里 client_type=websocket 的那条），把租约要过来，测完再还回去。
        clients = api("/health").get("clients", {}).get("items", [])
        mine = next((c for c in clients if c.get("client_type") == "websocket"), None)
        others = [c for c in clients if c is not mine and c.get("role") == "controller"]
        if mine:
            take_lease(mine["id"])
            await asyncio.sleep(0.3)

        sent = 0
        first_ts = None
        for packet in packets:
            await ws.send(b"\x02" + packet)
            if first_ts is None:
                first_ts = time.time()
                print(f"T0={first_ts:.3f} first mic frame")
            sent += 1
            await asyncio.sleep(0.02)
        print(f"T1={time.time():.3f} sent {sent} mic frames")
        await asyncio.sleep(1.0)

        # 还给原来那个页签：机器上的键盘鼠标在这几秒里是被我借走的。
        if others:
            take_lease(others[0]["id"])
            await asyncio.sleep(0.2)
        drainer.cancel()
    print("RESULT sent")


asyncio.run(main())
