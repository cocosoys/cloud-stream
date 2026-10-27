import socket, base64, os

HOST, PORT = "127.0.0.1", 8081

def http_get(path):
    s = socket.create_connection((HOST, PORT), timeout=5)
    s.sendall(f"GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\nConnection: close\r\n\r\n".encode())
    data = b""
    while True:
        chunk = s.recv(4096)
        if not chunk:
            break
        data += chunk
    s.close()
    return data.split(b"\r\n", 1)[0].decode(errors="replace"), len(data)

def ws_handshake(path):
    s = socket.create_connection((HOST, PORT), timeout=5)
    key = base64.b64encode(os.urandom(16)).decode()
    req = (f"GET {path} HTTP/1.1\r\n"
           f"Host: 127.0.0.1:{PORT}\r\n"
           f"Upgrade: websocket\r\n"
           f"Connection: Upgrade\r\n"
           f"Sec-WebSocket-Key: {key}\r\n"
           f"Sec-WebSocket-Version: 13\r\n\r\n").encode()
    s.sendall(req)
    data = b""
    try:
        while True:
            chunk = s.recv(4096)
            if not chunk:
                break
            data += chunk
            if b"\r\n\r\n" in data:
                break
    except socket.timeout:
        pass
    s.close()
    return data.split(b"\r\n", 1)[0].decode(errors="replace"), len(data)

print("GET / ->", http_get("/"))
print("GET /api/turn ->", http_get("/api/turn"))
print("WS /api/webrtc/signaling/ ->", ws_handshake("/api/webrtc/signaling/"))
print("WS /webrtc/api/webrtc/signaling/ ->", ws_handshake("/webrtc/api/webrtc/signaling/"))
print("WS /signaling/ ->", ws_handshake("/signaling/"))
print("GET /webrtc ->", http_get("/webrtc"))
