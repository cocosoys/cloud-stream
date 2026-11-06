"""#112 端到端验证：WS 收尾必须把输入租约交出去。

复现线上形状（换传输那几十秒就是这个形状）：
  A 持租约 → B 连上来（认领失败）→ A 掉线 → 租约必须落到 B 手里。

判据（看服务端日志）：
  `input lease handed to session <B> (holder left)`
另外从 B 发 `m,` 之后，B 的 `denied_inputs` 必须还是 0（真的注进去了）。
"""
import asyncio
import json
import ssl
import sys
import urllib.request

import websockets

HOST = sys.argv[1] if len(sys.argv) > 1 else "192.168.59.148"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8080
BASE = f"https://{HOST}:{PORT}"
WS = f"wss://{HOST}:{PORT}/api/websockets"

CTX = ssl.create_default_context()
CTX.check_hostname = False
CTX.verify_mode = ssl.CERT_NONE


def health():
    with urllib.request.urlopen(f"{BASE}/api/v1/health", context=CTX, timeout=8) as r:
        return json.load(r)


def take(session_id):
    req = urllib.request.Request(
        f"{BASE}/api/v1/control/session/take/{session_id}", method="POST", data=b""
    )
    try:
        with urllib.request.urlopen(req, context=CTX, timeout=8) as r:
            return json.load(r)
    except Exception as e:  # noqa: BLE001
        return {"error": str(e)}


def sockets():
    return [c for c in health()["clients"]["items"] if c["client_type"] == "websocket"]


async def main():
    before = {c["id"] for c in sockets()}
    holder_page = [c["id"] for c in health()["clients"]["items"] if c["client_type"] != "websocket"]

    a = await websockets.connect(WS, ssl=CTX, max_size=None)
    await asyncio.sleep(0.6)
    await a.send("START_VIDEO")
    await asyncio.sleep(0.6)
    id_a = max(c["id"] for c in sockets() if c["id"] not in before)
    print(f"A = session {id_a}")
    print("take A ->", take(id_a))
    await asyncio.sleep(0.5)

    b = await websockets.connect(WS, ssl=CTX, max_size=None)
    await asyncio.sleep(0.6)
    await b.send("START_VIDEO")
    await asyncio.sleep(0.6)
    id_b = max(c["id"] for c in sockets() if c["id"] not in before and c["id"] != id_a)
    print(f"B = session {id_b}")

    row = [c for c in health()["clients"]["items"] if c["id"] == id_b][0]
    print(f"B before A leaves: role={row['role']} denied={row['denied_inputs']}")

    # A 掉线（不是优雅关闭：半开的 socket 正是线上换传输的形状）
    await a.close()
    await asyncio.sleep(1.2)

    # B 发 40 条绝对指针移动 —— 门禁如果还是死的，这些会被记进 denied
    for i in range(40):
        await b.send(f"m,{600 + i * 10},400,0,0")
        await asyncio.sleep(0.01)
    await asyncio.sleep(0.8)

    row = [c for c in health()["clients"]["items"] if c["id"] == id_b][0]
    print(f"B after A leaves: role={row['role']} denied={row['denied_inputs']}")

    await b.close()
    # 把租约还给页面（它本来就有）
    for sid in holder_page:
        print(f"take {sid} (give it back to the page) ->", take(sid))

    ok = row["role"] == "controller" and row["denied_inputs"] == 0
    print(f"VERDICT={'PASS' if ok else 'FAIL'}")


asyncio.run(main())
