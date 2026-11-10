#!/usr/bin/env python3
"""临时：抓 WS 下行的真实帧字节，落成 IVF 后用 ffmpeg 独立解码。

只看两件事：
  1. 服务端标的 `key` 位与 VP9 码流头里的 frame_type 是否一致；
  2. 把这些 payload 拼成 IVF 后，ffmpeg 能不能解出画面（能解 = 码流合法，
     问题在浏览器侧；不能解 = 服务端发出去的就不是合法 VP9）。
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
OUT = sys.argv[3] if len(sys.argv) > 3 else "C:/Users/user/Desktop/cloud-stream/target/vp9-probe.ivf"
SECONDS = float(sys.argv[4]) if len(sys.argv) > 4 else 6.0

WIRE = {1: "h264", 2: "vp8", 3: "vp9", 4: "av1", 5: "h265"}


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
    length = b1 & 0x7F
    if length == 126:
        length = struct.unpack(">H", recv_exact(sock, 2))[0]
    elif length == 127:
        length = struct.unpack(">Q", recv_exact(sock, 8))[0]
    payload = recv_exact(sock, length) if length else b""
    return opcode, payload


def send_text(sock, text):
    p = text.encode()
    mask = os.urandom(4)
    m = bytes(b ^ mask[i % 4] for i, b in enumerate(p))
    sock.sendall(bytes([0x81, 0x80 | len(p)]) + mask + m)


ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE
raw = socket.create_connection((HOST, PORT), timeout=10)
sock = ctx.wrap_socket(raw, server_hostname=HOST)
sock.settimeout(10)

key = base64.b64encode(os.urandom(16)).decode()
sock.sendall((
    f"GET /api/websockets HTTP/1.1\r\nHost: {HOST}:{PORT}\r\n"
    f"Upgrade: websocket\r\nConnection: Upgrade\r\n"
    f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
).encode())
head = b""
while b"\r\n\r\n" not in head:
    head += sock.recv(1)
print("handshake:", head.split(b"\r\n", 1)[0].decode(errors="replace"))

send_text(sock, "cr")

deadline = time.time() + SECONDS
frames = []
seen = 0
codecs = set()
keys = 0
while time.time() < deadline:
    try:
        opcode, payload = read_frame(sock)
    except (socket.timeout, EOFError):
        break
    if opcode == 0x1:
        line = payload.decode(errors="replace")
        if line.startswith("{"):
            continue
        if "settings" in line:
            # 只挑 encoder 那一段，整行太长。
            i = line.find('"encoder"')
            print("settings.encoder:", line[i:i + 60] if i >= 0 else "(no encoder key)")
        else:
            print("text:", line[:100])
        continue
    if opcode != 0x2 or len(payload) < 12:
        continue
    if payload[0] != 0x04:
        continue
    seen += 1
    codec = WIRE.get(payload[1] >> 4, f"?{payload[1] >> 4}")
    is_key = bool(payload[1] & 0x01)
    fid, y, w, h, ref = struct.unpack(">HHHHH", payload[2:12])
    body = payload[12:]
    codecs.add(codec)
    if is_key:
        keys += 1
    if seen <= 3 or is_key:
        # VP9 头：b0[7:6]=marker(0b10) b0[2]=frame_type(0=key)
        bits = ""
        if body:
            b0 = body[0]
            bits = f"marker={b0 >> 6:02b} profile={(b0 >> 4) & 3} show_existing={(b0 >> 3) & 1} type={(b0 >> 2) & 1} b0=0x{b0:02x}"
        print(f"frame #{seen} {codec} key={is_key} id={fid} y={y} {w}x{h} ref={ref} {len(body)}B | {bits}")
    frames.append((fid, w, h, is_key, body))

sock.close()
print(f"collected {len(frames)} video frames, codecs={codecs}, server-flagged keys={keys}")
if not frames:
    print("NO FRAMES")
    sys.exit(1)

# 从第一个关键帧开始，否则前面全是残差帧，解不出来是必然的。
start = next((i for i, f in enumerate(frames) if f[3]), None)
if start is None:
    print("NO KEYFRAME in the window; cannot build a decodable IVF")
    sys.exit(2)
print(f"starting at keyframe index {start}")

w = frames[start][1] or 1920
h = frames[start][2] or 1080
chunk = frames[start:]
with open(OUT, "wb") as fp:
    fp.write(b"DKIF" + struct.pack("<HH", 0, 32) + b"VP90" +
             struct.pack("<HH", w, h) + struct.pack("<II", 30, 1) +
             struct.pack("<I", len(chunk)) + struct.pack("<I", 0))
    for i, (_fid, _w, _h, _k, body) in enumerate(chunk):
        fp.write(struct.pack("<IQ", len(body), i))
        fp.write(body)
print(f"wrote {OUT} ({len(chunk)} frames, {w}x{h})")
