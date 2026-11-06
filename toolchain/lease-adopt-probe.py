#!/usr/bin/env python3
"""#113 验证工具：换传输之后，新的 WebSocket 连接能不能拿到输入租约。

为什么需要它：租约在 `exclusive` 下一度是**永久所有权** —— claim-if-free、
没人能挤掉、只在收尾时归还。页面从 WebRTC 切到 WebSocket 之后，上一次那条
WebRTC 会话还挂在注册表里（PeerConnection 没拆、下行 sink 还在收字节），租约
归它；新的 media 连接每一次认领都撞车，于是**每一条 `m,` 都被门禁丢掉**。

页面上的表现很有欺骗性：画面在流，指针跟着鼠标动（那是页面自己画的），桌面
一动不动 —— 悬停没有反馈、点击没有反应。用户的原话是"指针像是再次渲染上去
的"。服务端日志的形状是：

    ws media: … grant=viewer operates=yes lease=no
    ws media: … attached as session 3 (…)
    pipeline 0: input lease Some(2) -> Some(3)      ← 只有 HTTP session/take 才动得了
    ws media: … lease=no                            ← 又来一条，还是拿不到

这个探针复现的是那个形状的最简版本：**A 连上、认领租约、一条输入都不投；
B 随后连上，必须能把租约接过来**。A 保持连接（这才是关键 —— 悬空租约的
难点正是持有者还活着）。

用法：
    python lease-adopt-probe.py [host] [port]
    python lease-adopt-probe.py 192.168.59.148 8080

判据（全部来自 /api/v1/health，不猜日志格式）：
  1. A 落座后是 `controller`（租约空着 → 认领，或从一条没人用的租约接管）；
  2. B 落座后是 `controller`，而 A 退成 `viewer` —— 这一步在修复前恒失败；
  3. 关掉 B 之后租约落回 A（活着、有下行的那条），而不是留在一个死 id 上；
  4. A 持租约发 30 条 `m,`，`denied_inputs` 必须一动不动 —— 门禁和注入器
     不能各说各话。

退出码：0 = 通过；1 = 失败；2 = 环境不适用（没连上、策略是 shared 等）。
"""

import base64
import hashlib
import json
import os
import socket
import ssl
import struct
import sys
import time

HOST = sys.argv[1] if len(sys.argv) > 1 else "192.168.59.148"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8080
# 探针自己的页签名。用固定前缀，收尾时能确定哪几条是"我的"，绝不误伤真页面。
TAG = "lease-probe"

# 探针经常被 `timeout` 杀掉（失败时最需要看它跑到哪一步了），所以关掉块缓冲：
# 被 SIGTERM 打断时缓冲区里的行会一起消失，留下的现象就是"什么都没打印"。
try:
    sys.stdout.reconfigure(line_buffering=True)
except (AttributeError, ValueError):
    pass


def fail(reason):
    print(f"RESULT fail {reason}")
    sys.exit(1)


def skip(reason):
    print(f"RESULT skip {reason}")
    sys.exit(2)


def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        part = sock.recv(n - len(buf))
        if not part:
            raise EOFError("socket closed")
        buf += part
    return buf


def read_frame(sock):
    """RFC 6455 一帧。服务端不掩码，payload 直接可读。"""
    b0, b1 = recv_exact(sock, 2)
    opcode = b0 & 0x0F
    masked = b1 & 0x80
    length = b1 & 0x7F
    if length == 126:
        length = struct.unpack(">H", recv_exact(sock, 2))[0]
    elif length == 127:
        length = struct.unpack(">Q", recv_exact(sock, 8))[0]
    mask = recv_exact(sock, 4) if masked else None
    payload = recv_exact(sock, length) if length else b""
    if mask:
        payload = bytes(payload[i] ^ mask[i % 4] for i in range(len(payload)))
    return opcode, payload


def send_text(sock, text):
    payload = text.encode()
    mask = os.urandom(4)
    masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    if len(payload) < 126:
        header = bytes([0x81, 0x80 | len(payload)])
    else:
        header = bytes([0x81, 0x80 | 126]) + struct.pack(">H", len(payload))
    sock.sendall(header + mask + masked)


def send_close(sock):
    """正常关闭帧。半开的 socket 会让服务端的"持有者还在"判定拖到超时。"""
    mask = os.urandom(4)
    body = struct.pack(">H", 1000)
    masked = bytes(b ^ mask[i % 4] for i, b in enumerate(body))
    try:
        sock.sendall(bytes([0x88, 0x80 | len(body)]) + mask + masked)
    except OSError:
        pass


def connect():
    """一条 /api/websockets 连接。返回值里的 sock 已经过了 101。"""
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    raw = socket.create_connection((HOST, PORT), timeout=10)
    sock = ctx.wrap_socket(raw, server_hostname=HOST)
    key = base64.b64encode(os.urandom(16)).decode()
    request = (
        f"GET /api/websockets HTTP/1.1\r\n"
        f"Host: {HOST}:{PORT}\r\n"
        f"Upgrade: websocket\r\n"
        f"Connection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        f"Sec-WebSocket-Version: 13\r\n"
        f"\r\n"
    )
    sock.sendall(request.encode())
    head = b""
    while b"\r\n\r\n" not in head:
        head += sock.recv(1)
    first = head.split(b"\r\n", 1)[0].decode(errors="replace")
    if "101" not in first:
        fail(f"upgrade refused: {first}")
    expect = base64.b64encode(
        hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()
    ).decode()
    if f"sec-websocket-accept: {expect}".lower() not in head.decode(errors="replace").lower():
        fail("accept header mismatch")
    return sock


def health():
    """一次 /api/v1/health。用裸 socket 而不是 requests：这台机器上不一定有它。"""
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    raw = socket.create_connection((HOST, PORT), timeout=10)
    sock = ctx.wrap_socket(raw, server_hostname=HOST)
    sock.sendall(
        f"GET /api/v1/health HTTP/1.1\r\nHost: {HOST}:{PORT}\r\nConnection: close\r\n\r\n".encode()
    )
    data = b""
    while True:
        part = sock.recv(65536)
        if not part:
            break
        data += part
    sock.close()
    body = data.split(b"\r\n\r\n", 1)[1]
    return json.loads(body.decode(errors="replace"))


def drain(sock, seconds=0.6):
    """把服务端主动推来的帧读掉。

    不能用"读到超时"当循环条件：这条连接同时在下发 60 fps 的二进制帧，永远
    不会超时，于是 `drain` 变成死循环（第一版就是这么卡住的）。按**截止时间**
    停，帧照读，只是时间到了就走。
    """
    deadline = time.monotonic() + seconds
    sock.settimeout(0.2)
    try:
        while time.monotonic() < deadline:
            try:
                read_frame(sock)
            except (socket.timeout, TimeoutError):
                continue
    finally:
        sock.settimeout(10)


def client_with_tag(state, tag):
    for item in state.get("clients", {}).get("items", []):
        if item.get("tab_id") == tag:
            return item
    return None


def main():
    state = health()
    if not state.get("ok"):
        fail("/api/v1/health did not report ok")
    policy = state.get("server", {}).get("input_policy")
    print(f"server: pid={state['server'].get('pid')} input_policy={policy}")
    if policy != "exclusive":
        # `shared` 下人人都能注入，这条路径根本不存在 —— 不是失败，是前提不对。
        skip(f"input_policy is {policy}; the lease only gates input under exclusive")

    # 环境里已经有别人的连接时仍然可以跑：探针只认自己页签名的记录，而租约
    # 接管本来就是"谁在投输入谁说了算"，跑一次不会把真用户的指针永久拿走
    # （他下一条输入就会接回去）。
    others = [
        c for c in state["clients"]["items"] if not str(c.get("tab_id", "")).startswith(TAG)
    ]
    for c in others:
        print(
            f"  note: live client id={c['id']} role={c['role']} tab={c.get('tab_id')!r} "
            f"type={c.get('client_type')}"
        )

    tag_a = f"{TAG}-a"
    tag_b = f"{TAG}-b"
    sock_a = connect()
    send_text(sock_a, 'SETTINGS,{"tabId":"%s"}' % tag_a)
    drain(sock_a)

    state = health()
    a = client_with_tag(state, tag_a)
    if not a:
        fail("session A did not register (no client with our tab id in health)")
    print(f"A: id={a['id']} role={a['role']} denied={a.get('denied_inputs')}")
    if a["role"] != "controller":
        holder = [c for c in state["clients"]["items"] if c["role"] == "controller"]
        fail(
            "session A did not get the lease even though it is the newest connection; "
            f"holder(s)={[(c['id'], c.get('tab_id')) for c in holder]}"
        )

    # A 一条输入都不投 —— 这正是线上那条路：持有者活着，但页面已经不在了。
    time.sleep(0.3)

    sock_b = connect()
    send_text(sock_b, 'SETTINGS,{"tabId":"%s"}' % tag_b)
    drain(sock_b)

    state = health()
    a = client_with_tag(state, tag_a)
    b = client_with_tag(state, tag_b)
    if not b:
        fail("session B did not register")
    print(f"B: id={b['id']} role={b['role']} denied={b.get('denied_inputs')}")
    print(f"A after B: role={a['role'] if a else 'gone'}")

    if b["role"] != "controller":
        fail(
            "session B could not take a lease its holder never injected through — "
            "this is the transport-switch bug (#113)"
        )
    if a and a["role"] == "controller":
        fail("two sessions report `controller` at once; health would be unreadable")

    # 收尾：先关 B（它握着租约），再关 A。关掉 B 时租约应当交给 A —— 活着、
    # 有下行的会话 —— 而不是留给一条已经不存在的 id。
    send_close(sock_b)
    sock_b.close()
    time.sleep(0.5)
    state = health()
    b = client_with_tag(state, tag_b)
    a = client_with_tag(state, tag_a)
    print(f"after closing B: A role={a['role'] if a else 'gone'} B={'gone' if not b else b['role']}")
    if a and a["role"] != "controller":
        print("  note: the lease did not fall back to A; the next connection will adopt it instead")

    # 第二步：拿到租约之后，输入必须真的注进去。
    #
    # `role == controller` 只证明门禁放行，不证明 `m,` 走到了注入器 —— 这两件事
    # 以前同时坏过一次（门禁放行、`handle_text` 把 `m,` 归到了别的分支）。判据用
    # `denied_inputs` 不变 + 服务端日志里下一个 5 s 窗口的 `m=` 非零（那条要去
    # 被控机上看，探针只负责把输入发出去并把 denied 记下来）。
    if a and a["role"] == "controller":
        before = a.get("denied_inputs", 0)
        for i in range(30):
            # 画面中央的一小段弧线：只移动、不带按键，不会点到任何东西。
            send_text(sock_a, f"m,{900 + i * 4},{540 + i * 2},0,0")
            time.sleep(0.01)
        drain(sock_a, 0.4)
        state = health()
        a = client_with_tag(state, tag_a)
        after = a.get("denied_inputs", 0) if a else -1
        print(f"A input: denied {before} -> {after}")
        if after != before:
            fail("input with a held lease was refused; the gate and the injector disagree")
    else:
        print("  note: skipped the input step (A is not the holder any more)")

    send_close(sock_a)
    sock_a.close()
    print("RESULT ok transport-switch lease handover works")
    sys.exit(0)


if __name__ == "__main__":
    main()
