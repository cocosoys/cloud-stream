#!/usr/bin/env python3
"""#115：验证 `STOP_VIDEO` 之后服务端到底做了什么。

用户的报告是"服务端 FPS 60、客户端 FPS 0"。这两个数字来自两套完全不同的
口径，必须分开验证，否则永远在猜：

  * 服务端面板的 FPS 是**编码器**的帧数/秒（`frames_total.new + repeat`），
    它只说明"编码器在干活"，不说明"这一条连接收到了东西"；
  * 客户端面板的 FPS 是**页面**按帧号变化数出来的到达帧率。

这个脚本用一条裸 WS 连接回答中间那一环：发一条 `STOP_VIDEO` 之后，服务端
还会不会往这条连接发视频？如果会，那"客户端 0"就只能是页面侧的问题；如果
不会，那么"服务端 60（在编码）+ 客户端 0（没发给它）"就是一个自洽的状态，
页面一旦发了 STOP 而没再发 START，就会永久停在 0。

用法：
    python stop-verify.py [host] [port]
"""

import base64
import hashlib
import os
import socket
import ssl
import struct
import sys
import time
import urllib.request
import json

HOST = sys.argv[1] if len(sys.argv) > 1 else "192.168.59.148"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8080


def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        part = sock.recv(n - len(buf))
        if not part:
            raise EOFError("closed")
        buf += part
    return buf


def read_frame(sock):
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
    n = len(payload)
    header = bytes([0x81, 0x80 | n]) if n < 126 else bytes([0x81, 0x80 | 126]) + struct.pack(">H", n)
    sock.sendall(header + mask + masked)


def connect():
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    raw = socket.create_connection((HOST, PORT), timeout=10)
    sock = ctx.wrap_socket(raw, server_hostname=HOST)
    key = base64.b64encode(os.urandom(16)).decode()
    req = (
        f"GET /api/websockets HTTP/1.1\r\nHost: {HOST}:{PORT}\r\n"
        f"Upgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    sock.sendall(req.encode())
    head = b""
    while b"\r\n\r\n" not in head:
        head += sock.recv(1)
    if b"101" not in head.split(b"\r\n", 1)[0]:
        raise SystemExit("upgrade refused")
    sock.settimeout(1.0)
    return sock


def count_for(sock, seconds, stop_at_first=False):
    """数这几秒里收到的 0x04 全帧，顺便把文本消息和第二帧的时间记下来。"""
    start = time.monotonic()
    end = start + seconds
    frames = 0
    texts = []
    ids = []
    first_after = None
    while time.monotonic() < end:
        try:
            opcode, payload = read_frame(sock)
        except (socket.timeout, TimeoutError):
            continue
        except EOFError:
            break
        if opcode == 0x1:
            texts.append(payload.decode(errors="replace")[:120])
            continue
        if opcode != 0x2 or not payload:
            continue
        if payload[0] == 0x04 and len(payload) >= 12:
            if first_after is None:
                first_after = time.monotonic() - start
            frames += 1
            ids.append(struct.unpack(">H", payload[2:4])[0])
    return frames, texts, ids, first_after


def server_encoded_fps():
    """服务端自己的口径：编码器每秒产出的帧（new + repeat）。"""
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{PORT}/api/v1/health", timeout=5) as r:
            pass
    except Exception:
        pass
    return None


def main():
    print(f"连 {HOST}:{PORT}/api/websockets")
    sock = connect()
    send_text(sock, "cr")
    before, texts, ids, _ = count_for(sock, 3.0)
    print(f"STOP 之前 3s: 收到 {before} 帧"
          + (f"，帧号 {ids[0]}->{ids[-1]}" if ids else ""))

    send_text(sock, "STOP_VIDEO")
    print("已发送 STOP_VIDEO")
    time.sleep(0.5)
    after, texts2, ids2, _ = count_for(sock, 3.0)
    print(f"STOP 之后 3s: 收到 {after} 帧"
          + (f"，帧号 {ids2[0]}->{ids2[-1]}" if ids2 else ""))

    send_text(sock, "START_VIDEO")
    print("已发送 START_VIDEO")
    resumed, texts3, ids3, first = count_for(sock, 10.0)
    print(f"START 之后 10s: 收到 {resumed} 帧"
          + (f"，帧号 {ids3[0]}->{ids3[-1]}" if ids3 else ""))
    if first is not None:
        print(f"  第一帧在 START 后 {first:.2f}s 才到（这是用户切回窗口要等的黑屏）")

    print("\n判定：")
    if before > 30 and after == 0:
        print("  STOP_VIDEO 确实**停掉这一条连接的视频**。")
        if resumed < 200:
            print(f"  START 之后的 10s 只补了 {resumed} 帧（正常应为 ~600）——")
            print("  恢复要等下一个关键帧，这就是「切回来还是 0/1」的另一半。")
        else:
            print("  START 之后立即恢复满速。")
        print("  → 页面一旦发了 STOP 而没有补发 START，客户端 FPS 就永恒为 0，")
        print("    而面板的「服务器 FPS」（编码口径）照旧显示 60。")
    elif before > 30 and after > 30:
        print("  STOP_VIDEO 没有停掉视频 —— 服务端忽略了它，问题在页面侧。")
    else:
        print("  基线就不正常，先查这条连接的 START/ACK 流程。")


if __name__ == "__main__":
    main()
