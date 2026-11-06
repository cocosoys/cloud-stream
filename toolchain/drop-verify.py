#!/usr/bin/env python3
"""#115：一次"读慢两秒"会不会把这条连接永久钉在只发关键帧上。

`ws_media.rs` 里 `dropped` 是**整条连接的累计数**，却拿来决定"下一帧要不要
等关键帧"：

    while let Ok(next) = rx.try_recv() { latest = next; dropped += 1; }
    if dropped > 0 && need_key == false { need_key = true; }

一旦这条连接排空过一次队列（页面卡一下、网络抖一下、帧忽然变大），
`dropped` 就再也不回到 0，于是此后**每一拍**都会把 `need_key` 重新置真，
只有关键帧能过闸。周期关键帧是 10 s 一个 —— 客户端于是恰好看到
"0 和 1 之间浮动"，而服务端编码器照旧 60 fps。

这个脚本用一条裸连接复现：正常收 3 s → **故意停读 2 s**（让服务端排空队列）
→ 恢复读 12 s，数收到多少帧、其中多少是关键帧。

用法：
    python drop-verify.py [host] [port]
"""

import base64
import os
import socket
import ssl
import struct
import sys
import time

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


def window(sock, seconds):
    """收一段时间，返回 (帧数, 关键帧数, 出现的帧号列表)。"""
    end = time.monotonic() + seconds
    frames = keys = 0
    ids = []
    while time.monotonic() < end:
        try:
            opcode, payload = read_frame(sock)
        except (socket.timeout, TimeoutError):
            continue
        except EOFError:
            break
        if opcode != 0x2 or not payload or payload[0] != 0x04 or len(payload) < 12:
            continue
        frames += 1
        if payload[1] & 0x01:
            keys += 1
        ids.append(struct.unpack(">H", payload[2:4])[0])
    return frames, keys, ids


def main():
    sock = connect()
    send_text(sock, "cr")
    send_text(sock, "START_VIDEO")
    base, basek, baseids = window(sock, 3.0)
    print(f"基线 3s: {base} 帧（关键帧 {basek}）"
          + (f"，帧号 {baseids[0]}->{baseids[-1]}" if baseids else ""))
    if base < 30:
        print("基线不到 30 帧 —— 这条连接本来就没在发，先查别的。")
        return

    print("故意停读 2s（让服务端把队列里积压的帧排空丢弃）…")
    time.sleep(2.0)

    after, afterk, afterids = window(sock, 12.0)
    print(f"恢复读写 12s: {after} 帧（关键帧 {afterk}）"
          + (f"，帧号 {afterids[0]}->{afterids[-1]}" if afterids else ""))
    rate = after / 12.0
    print(f"  平均 {rate:.1f} 帧/秒")

    print("\n判定：")
    if rate >= 30:
        print("  读慢两秒之后照样满速 —— 这条连接没有掉进'只发关键帧'。")
    elif rate <= 2:
        print("  复现了：一次排空丢弃之后，这条连接只剩关键帧能过闸。")
        print("  这就是'服务端 60 fps、客户端 0/1 浮动'的机制 ——")
        print("  周期关键帧 10 s 一个，客户端因此每秒看到 0~1 帧。")
    else:
        print(f"  降到了 {rate:.1f} 帧/秒 但没到底 —— 介于两者之间，值得再看一次。")


if __name__ == "__main__":
    main()
