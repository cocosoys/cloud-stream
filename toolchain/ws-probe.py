#!/usr/bin/env python3
"""#106 验证工具：对 /api/websockets 做一次真实的 WebSocket 会话，逐帧解析 12 字节头。

为什么手写而不是装 websockets 库：这条链路要验的正好是**线上格式**，中间夹一层
库只会把"库帮我拼对了"混进结论里。这里只做三件事——TLS 连上、发 Upgrade、按
RFC 6455 拆帧——剩下的字节原样看。

用法：
    python ws-probe.py [host] [port] [帧数] [目标屏]
    python ws-probe.py 192.168.59.148 8080 5            # 只看默认屏
    python ws-probe.py 192.168.59.148 8080 5 display2   # 切到第二块屏再看

给了「目标屏」时，探针会像页面那样发一条 `SETTINGS,{"displayId":…}`，然后**只统计
`PIPELINE_RESETTING` 之后**的帧 —— 那之前的帧属于旧屏，混进来会把"换屏没生效"验成
通过。

多屏机上应当看到 `PIPELINE_RESETTING display2` + 之后每一帧都是第二块屏的尺寸；
单屏机上应当看到同一行文本、然后**没有帧**（服务端如实失败，不会把主屏错发过来）。
输出每一帧的 head 解析结果，最后给出结论行 `RESULT ok|fail <原因>`。
"""

import base64
import hashlib
import os
import socket
import ssl
import struct
import sys

HOST = sys.argv[1] if len(sys.argv) > 1 else "192.168.59.148"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8080
WANT_FRAMES = int(sys.argv[3]) if len(sys.argv) > 3 else 5
# `primary` / `display2` / … — 与页面 hash 里的关键字同一套。
TARGET_DISPLAY = sys.argv[4] if len(sys.argv) > 4 else None

WIRE_CODECS = {1: "h264", 2: "vp8", 3: "vp9", 4: "av1", 5: "h265"}

# 探针自己的进度，只为了让**超时**这条退出路径能说人话：换屏后没有帧是最需要解释的
# 一种失败（单屏机上它是预期结果，多屏机上它是 bug），而 `read_frame` 只能抛一个
# `TimeoutError: timed out`。
STATE = {"armed": False, "frames": 0}


def fail(reason):
    print(f"RESULT fail {reason}")
    sys.exit(1)


def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        part = sock.recv(n - len(buf))
        if not part:
            raise EOFError("socket closed")
        buf += part
    return buf


def read_frame(sock):
    """RFC 6455 一帧。服务端不掩码，所以 payload 直接可读。"""
    b0, b1 = recv_exact(sock, 2)
    fin = b0 & 0x80
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
    return fin, opcode, payload


def send_text(sock, text):
    """一个客户端必须掩码的文本帧（RFC 6455 5.3）。只用于短命令。"""
    payload = text.encode()
    mask = os.urandom(4)
    masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    if len(payload) < 126:
        header = bytes([0x81, 0x80 | len(payload)])
    else:
        header = bytes([0x81, 0x80 | 126]) + struct.pack(">H", len(payload))
    sock.sendall(header + mask + masked)


def send_binary(sock, payload):
    """掩码的二进制帧（opcode 0x2）——麦克风/摄像头上行走的就是这条。"""
    mask = os.urandom(4)
    masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    if len(payload) < 126:
        header = bytes([0x82, 0x80 | len(payload)])
    elif len(payload) < 65536:
        header = bytes([0x82, 0x80 | 126]) + struct.pack(">H", len(payload))
    else:
        header = bytes([0x82, 0x80 | 127]) + struct.pack(">Q", len(payload))
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
        fail(f"upgrade refused: {first}")

    # RFC 6455 要求回一个由 key 派生的 Accept；这里只核对它出现了，不复算哈希。
    expect = base64.b64encode(
        hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()
    ).decode()
    if f"sec-websocket-accept: {expect}".lower() not in head.decode(errors="replace").lower():
        fail("accept header mismatch")

    # 连接建立时的只读剪贴板请求。服务端必须回 `clipboard_reply,cr` —— 页面的
    # `taggedClipboardFetch` 靠它结束等待；不回只是"剪贴板永远空着"，不报错。
    send_text(sock, "cr")

    # #110: 麦克风上行服务端没有接收端，必须**明确回绝**（`MICROPHONE_DISABLED`），
    # 否则页面会一直采集、电平条一直跳，而对面永远听不到。这里发一帧假的 `0x02`，
    # 断言这句回绝真的回来了 —— 这是"广告了没货"那条最直接的证据。
    send_binary(sock, b"\x02" + b"\x00" * 8)

    # #109: 像页面一样选屏。`displayId` 是页面自己的字段名（signaling 的 hello
    # 用 `display_id`，这是两套写法，别混），服务端从这一批里读出目标屏。
    reset_line = None
    if TARGET_DISPLAY:
        payload = '{"displayId":"%s","tabId":"ws-probe"}' % TARGET_DISPLAY
        send_text(sock, f"SETTINGS,{payload}")
        reset_line = f"PIPELINE_RESETTING {TARGET_DISPLAY}"

    text_seen = []
    frames = 0
    last_id = None
    ids_ok = True
    keyed = 0
    audio = 0
    # 换屏前属于旧屏的帧要丢掉：把它们算进去，"换了屏但画面没换"会验成通过。
    armed = not TARGET_DISPLAY
    pre_switch_frames = 0
    first_after_switch = None
    while frames < WANT_FRAMES:
        fin, opcode, payload = read_frame(sock)
        if opcode == 0x1:
            line = payload.decode(errors="replace")
            # 断言看全文，打印才截断：`server_settings` 是一整行设置表，只存前
            # 120 个字符会让"有没有这条消息"的检查变成一个假阴性 —— 第一次跑
            # 就是这么误报的。
            text_seen.append(line)
            print(f"text: {line[:160]}")
            if reset_line and line.strip() == reset_line:
                armed = True
                STATE["armed"] = True
                last_id = None  # 新程序从新的帧号开始
            continue
        if opcode == 0x8:
            if TARGET_DISPLAY and not armed:
                fail(
                    f"server closed before answering the display switch "
                    f"({TARGET_DISPLAY}): {payload!r}"
                )
            fail(f"server closed before any frame: {payload!r}")
        if opcode != 0x2:
            continue

        ftype = payload[0]
        # 0x01 是音频（`[0x01, 0x00] + opus`）：同一个 socket 上混着走，不能
        # 因为"长度不足 12 字节的头"就判失败。
        if ftype == 0x01:
            audio += 1
            if len(payload) < 3:
                fail(f"audio frame without an Opus packet: {len(payload)} B")
            continue
        if not armed:
            pre_switch_frames += 1
            continue
        if len(payload) < 12:
            fail(f"video frame shorter than the 12-byte header: {len(payload)}")
        type_byte = payload[1]
        codec = WIRE_CODECS.get(type_byte >> 4, f"?{type_byte >> 4}")
        is_key = (type_byte & 0x0F) == 0x01
        frame_id = struct.unpack(">H", payload[2:4])[0]
        y = struct.unpack(">H", payload[4:6])[0]
        w = struct.unpack(">H", payload[6:8])[0]
        h = struct.unpack(">H", payload[8:10])[0]
        ref = struct.unpack(">H", payload[10:12])[0]
        body = len(payload) - 12

        if last_id is not None and frame_id != (last_id + 1) & 0xFFFF:
            ids_ok = False
        last_id = frame_id
        if first_after_switch is None:
            first_after_switch = (w, h)
        if is_key:
            keyed += 1

        print(
            f"frame#{frames}: type=0x{ftype:02x} codec={codec} key={int(is_key)} "
            f"id={frame_id} y={y} {w}x{h} ref={ref} payload={body}B fin={int(bool(fin))}"
        )
        if ftype != 0x04:
            fail(f"unexpected video frame type 0x{ftype:02x}, expected 0x04 (full frame)")
        if codec not in ("vp8", "vp9"):
            fail(f"unexpected codec {codec}")
        if y != 0:
            fail(f"full frame must have y=0, got {y}")
        if w == 0 or h == 0:
            fail(f"zero dimension {w}x{h}")
        if body == 0:
            fail("empty payload")
        frames += 1
        STATE["frames"] = frames

    sock.sendall(b"\x88\x82" + b"\x00\x00\x00\x00" + struct.pack(">H", 1000))

    if "MODE websockets" not in text_seen:
        fail("no MODE websockets before the frames")
    if not any('"server_settings"' in t for t in text_seen):
        fail("no server_settings payload")
    if "clipboard_reply,cr" not in text_seen:
        fail("no clipboard_reply,cr answer to the connect-time `cr`")
    if "MICROPHONE_DISABLED" not in text_seen:
        fail("no MICROPHONE_DISABLED answer to a 0x02 uplink: the page would keep capturing")
    if reset_line and reset_line not in text_seen:
        fail(f"no `{reset_line}` after SETTINGS displayId={TARGET_DISPLAY}")
    if not ids_ok:
        fail("frame ids are not contiguous")
    if keyed == 0:
        fail("no keyframe in the window — first frame must be a decode entry point")

    size = f" {first_after_switch[0]}x{first_after_switch[1]}" if first_after_switch else ""
    switched = f" display={TARGET_DISPLAY}" if TARGET_DISPLAY else ""
    print(
        f"RESULT ok frames={frames} keyframes={keyed} audio={audio} text={len(text_seen)}"
        f"{switched}{size} pre_switch={pre_switch_frames}"
    )
    sock.close()


if __name__ == "__main__":
    try:
        main()
    except Exception as exc:  # noqa: BLE001 - a probe prints, it does not raise
        if isinstance(exc, (TimeoutError, socket.timeout)) and TARGET_DISPLAY:
            if STATE["armed"] and STATE["frames"] == 0:
                fail(
                    f"no frame after switching to {TARGET_DISPLAY}: the capture could not "
                    f"open that display. On a single-monitor host this is the CORRECT "
                    f"answer (the server must not serve the primary screen instead) — "
                    f"check the server log for `display N does not exist`"
                )
            if not STATE["armed"]:
                fail(f"no PIPELINE_RESETTING {TARGET_DISPLAY} within the timeout")
        fail(f"{type(exc).__name__}: {exc}")
