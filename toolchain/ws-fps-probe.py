#!/usr/bin/env python3
"""#114 验证工具：用页面的口径重算 WS 的客户端 FPS。

为什么需要它：面板上的「客户端 FPS」只有一个来源 —— 页面的 `window.fps`，
而它只数**帧号的变化**（`selkies-ws-core.js:8195` 的
`uniqueStripedFrameIdsThisPeriod.add(...)`，以及 worker 侧的
`if (frameId !== wireLastId) wireFrames++`）。所以「服务端 60 fps、客户端 0」
只有三种可能：

  1. 服务端发出去的帧号不递增（同一秒里全是同一个 id）→ 页面数出来就是 0/1；
  2. 帧根本没到页面（分流到 worker 但 worker 没回报，或者被 `wireExpect` 滤掉）；
  3. 统计函数没跑（`isSharedMode` 为真 / 定时器没起来）。

这个探针回答第 1 条，用**字节层面的事实**：连上去、按 RFC 6455 收帧、解析
12 字节头里的 `frameId`，再按页面那套「每秒去重计数」算一遍。如果探针按页面
口径算出来是 60，那问题就不在帧号，而在第 2/3 条。

它**不改任何设置**（不发 encoder / displayId）：线上可能正有人在看，切编码器
会把会话踢掉。想验特定编码器，先在页面上切好，再跑这个探针。

用法：
    python ws-fps-probe.py [host] [port] [秒数]
    python ws-fps-probe.py 192.168.59.148 8080 6
"""

import base64
import hashlib
import os
import socket
import ssl
import struct
import sys
import time

HOST = sys.argv[1] if len(sys.argv) > 1 else "192.168.59.148"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8080
SECONDS = float(sys.argv[3]) if len(sys.argv) > 3 else 6.0

WIRE_CODECS = {1: "h264", 2: "vp8", 3: "vp9", 4: "av1", 5: "h265"}
TYPE_AUDIO = 0x01
TYPE_JPEG_STRIPE = 0x03
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
    """客户端必须掩码（RFC 6455 5.3）。"""
    payload = text.encode()
    mask = os.urandom(4)
    masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    if len(payload) < 126:
        header = bytes([0x81, 0x80 | len(payload)])
    else:
        header = bytes([0x81, 0x80 | 126]) + struct.pack(">H", len(payload))
    sock.sendall(header + mask + masked)


def main():
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
    print(f"handshake: {first}")
    if "101" not in first:
        print("RESULT fail upgrade refused")
        sys.exit(1)

    expect = base64.b64encode(
        hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()
    ).decode()
    if f"sec-websocket-accept: {expect}".lower() not in head.decode(errors="replace").lower():
        print("RESULT fail accept header mismatch")
        sys.exit(1)

    # 只读：要一次剪贴板。不发 SETTINGS —— 任何设置都可能重建管线、踢掉真人会话。
    send_text(sock, "cr")

    # 页面的口令：`Yi.size + Ua`，每秒清一次。这里逐秒复刻。
    buckets = []          # [(unique_ids, frames)]
    cur_ids = set()
    cur_frames = 0
    t_start = time.monotonic()
    sec_deadline = t_start + 1.0
    total = 0
    audio = 0
    stripes = 0
    first_id = None
    last_id = None
    codec_seen = None
    ids_nonzero = 0
    id_hist = {}

    sock.settimeout(2.0)
    while time.monotonic() - t_start < SECONDS:
        try:
            opcode, payload = read_frame(sock)
        except (socket.timeout, TimeoutError):
            # 空闲一秒不是错误：静止桌面也可能一帧都没有，正好用来判"是否间歇"。
            if time.monotonic() >= sec_deadline:
                buckets.append((len(cur_ids), cur_frames))
                cur_ids = set()
                cur_frames = 0
                sec_deadline += 1.0
            continue
        except EOFError:
            print("server closed the connection")
            break
        if opcode == 0x1:
            line = payload.decode(errors="replace")
            print(f"text: {line[:160]}")
            continue
        if opcode == 0x8:
            print("close frame received")
            break
        if opcode != 0x2:
            continue

        ftype = payload[0]
        if ftype == TYPE_AUDIO:
            audio += 1
        elif ftype == TYPE_JPEG_STRIPE:
            stripes += 1
        elif ftype == TYPE_FULL_FRAME:
            if len(payload) < 12:
                print(f"short full frame: {len(payload)} B")
                continue
            frame_type = payload[1]
            fid = struct.unpack(">H", payload[2:4])[0]
            y = struct.unpack(">H", payload[4:6])[0]
            w = struct.unpack(">H", payload[6:8])[0]
            h = struct.unpack(">H", payload[8:10])[0]
            if first_id is None:
                first_id = fid
                codec_seen = WIRE_CODECS.get(frame_type & 0x07, f"type{frame_type}")
                print(f"first frame: codec={codec_seen} id={fid} {w}x{h} {len(payload)} B")
            last_id = fid
            if fid:
                ids_nonzero += 1
            id_hist[fid] = id_hist.get(fid, 0) + 1
            cur_ids.add(fid)
            cur_frames += 1
            total += 1

        now = time.monotonic()
        while now >= sec_deadline:
            buckets.append((len(cur_ids), cur_frames))
            cur_ids = set()
            cur_frames = 0
            sec_deadline += 1.0

    # 收尾：把最后不满一秒的一块也记下来（只用于诊断，不参与结论）。
    buckets.append((len(cur_ids), cur_frames))

    print("\n每秒（页面口径 = 去重帧号数；实际收到 = 帧数）:")
    for i, (uniq, frames) in enumerate(buckets):
        print(f"  t={i}s  unique_ids={uniq:3d}  frames={frames:3d}")

    full_buckets = [b for b in buckets[:-1]]
    if not full_buckets:
        print("\nRESULT fail 没收到完整一秒的数据，无法判定")
        sys.exit(1)

    page_fps = sorted(b[0] for b in full_buckets)
    median_unique = page_fps[len(page_fps) // 2]
    avg_frames = sum(b[1] for b in full_buckets) / len(full_buckets)
    print(
        f"\n汇总: 帧 {total} 条 / {SECONDS:.0f}s，"
        f"平均 {avg_frames:.1f} 帧/秒，"
        f"去重帧号中位数 {median_unique}/秒"
    )
    print(f"  音频 {audio} 条，JPEG 条纹 {stripes} 条")
    if first_id is not None:
        print(f"  帧号 {first_id} -> {last_id}（非零帧号 {ids_nonzero}/{total}）")
        dup = sum(1 for k, v in id_hist.items() if v > 1)
        if dup:
            print(f"  重复帧号 {dup} 个（同一 id 被发了多次）")

    if codec_seen == "vp9" and median_unique <= 1:
        print("RESULT fail 服务端每秒只发出一个不同的帧号 —— 页面必然数出 0/1")
        sys.exit(1)
    if median_unique > 0:
        print(f"RESULT ok 页面口径应显示约 {median_unique} fps")
        sys.exit(0)
    print("RESULT fail 这一秒没有帧号 —— 帧没到页面，或统计函数没跑")
    sys.exit(1)


if __name__ == "__main__":
    main()
