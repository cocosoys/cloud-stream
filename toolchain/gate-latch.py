#!/usr/bin/env python3
"""#116 回归验证：队列排空一次之后，这条连接还能不能继续收帧。

现场症状是"服务端 FPS 60、客户端 FPS 从 60 一路跌到 0/1 并停在那里，重连才
恢复"。根因在下行循环的这一句：

    if dropped > 0 && need_key == false { need_key = true; }

`dropped` 是**整条连接的累计数**，但判断的却是"这一批丢没丢帧"。于是排空过
一次队列就等于永久上闸，此后只有每 `keyframe_secs`（默认 10 秒）一个的 IDR
能过 —— 60 帧/秒被削成 0.1 帧/秒，面板取整就是 0，偶尔落在窗口里就是 1。

这个脚本制造一次**真实的**排空：连上去正常读一会儿，然后两三秒不读（TCP
背压 → 服务端队列积压 → 下行循环里 `try_recv` 排空出多余帧），再恢复读，
数后面几秒还能收到多少帧。

判定：
    恢复后 ≥ 30 帧/秒 → 闸放下了（修复生效）
    恢复后 <  5 帧/秒 → 闸没放下，从此刻起只发关键帧（旧行为）

用法：
    python gate-latch.py [host] [port] [--stall 2.0] [--after 6]
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
STALL = 2.0
AFTER = 6.0
argv = sys.argv[3:]
for i, a in enumerate(argv):
    if a == "--stall":
        STALL = float(argv[i + 1])
    elif a == "--after":
        AFTER = float(argv[i + 1])

TYPE_FULL_FRAME = 0x04


def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        part = sock.recv(n - len(buf))
        if not part:
            raise EOFError("socket closed")
        buf += part
    return buf


def read_frame(sock):
    b0, b1 = recv_exact(sock, 2)
    opcode = b0 & 0x0F
    length = b1 & 0x7F
    if length == 126:
        length = struct.unpack(">H", recv_exact(sock, 2))[0]
    elif length == 127:
        length = struct.unpack(">Q", recv_exact(sock, 8))[0]
    return opcode, recv_exact(sock, length)


def send_text(sock, text):
    payload = text.encode()
    header = bytes([0x81])
    n = len(payload)
    if n < 126:
        header += bytes([0x80 | n])
    elif n < 65536:
        header += bytes([0x80 | 126]) + struct.pack(">H", n)
    else:
        header += bytes([0x80 | 127]) + struct.pack(">Q", n)
    mask = os.urandom(4)
    masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    sock.sendall(header + mask + masked)


def connect():
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    raw = socket.create_connection((HOST, PORT), timeout=10)
    sock = ctx.wrap_socket(raw, server_hostname=HOST)
    key = base64.b64encode(os.urandom(16)).decode()
    sock.sendall((
        f"GET /api/websockets HTTP/1.1\r\nHost: {HOST}:{PORT}\r\n"
        f"Upgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
        f"Sec-WebSocket-Protocol: binary\r\n\r\n"
    ).encode())
    buf = b""
    while b"\r\n\r\n" not in buf:
        buf += sock.recv(4096)
    return sock


def count_for(sock, seconds):
    """这几秒里收到的整帧数。"""
    end = time.monotonic() + seconds
    frames = 0
    sock.settimeout(0.5)
    while time.monotonic() < end:
        try:
            opcode, payload = read_frame(sock)
        except (socket.timeout, TimeoutError):
            continue
        except EOFError:
            break
        if opcode == 0x2 and payload and payload[0] == TYPE_FULL_FRAME:
            frames += 1
    return frames


def main():
    print(f"连 {HOST}:{PORT}/api/websockets，停读 {STALL:.1f}s 制造一次真实的队列排空")
    sock = connect()
    send_text(sock, "cr")

    base = count_for(sock, 2.0) / 2.0
    print(f" 基线            {base:5.1f} 帧/秒")

    # 停止读取：TCP 缓冲灌满之后服务端 `sink.send` 阻塞，下行循环不再 `try_recv`，
    # 4 深的队列被编码器的下一拍填满 —— 这正是现场"右击桌面、画面大改"时发生的事。
    time.sleep(STALL)
    print(f" 停读 {STALL:.1f}s（不 recv）")

    burst = count_for(sock, 1.0)
    print(f" 恢复第 1 秒     {burst:5.0f} 帧（积压 + 关键帧）")

    rows = []
    for i in range(int(AFTER)):
        n = count_for(sock, 1.0)
        rows.append(n)
        print(f" 恢复第 {i + 2} 秒    {n:5.0f} 帧/秒")

    tail = sum(rows[1:]) / max(1, len(rows[1:])) if len(rows) > 1 else rows[0]
    print("\n判定：")
    if base < 10:
        print(f" 基线就只有 {base:.1f} 帧/秒，这条连接一开始就没在收帧，先查别处。")
    elif tail >= 30:
        print(f" 停读一次之后仍有 {tail:.0f} 帧/秒 —— 闸放下了，串流没被永久降级。")
    elif tail < 5:
        print(f" 停读一次之后只剩 {tail:.1f} 帧/秒 —— 闸没放下：这条连接从此刻起")
        print(" 只收关键帧（10 秒一个），客户端 FPS 会显示 0/1，重连才能恢复。")
    else:
        print(f" 恢复后 {tail:.0f} 帧/秒，介于两者之间，值得再看一次日志。")
    sock.close()


if __name__ == "__main__":
    main()
