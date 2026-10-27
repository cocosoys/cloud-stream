"""短跑一个 stream-server 诊断实例（独立端口 8090），验证日志与启动行为。

只读诊断用：不碰 8080 上的生产实例，3 秒后 terminate。
"""

import os
import subprocess
import time

EXE = r"C:\dev\stream\stream-server.exe"
LOG = r"C:\dev\stream\logs\diag.log"

args = [
    EXE,
    "--host", "127.0.0.1",
    "--port", "8090",
    "--fps", "60",
    "--no-hdr",
    "--log", LOG,
]

if os.path.exists(LOG):
    os.remove(LOG)

p = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
time.sleep(3)
p.terminate()
try:
    out, err = p.communicate(timeout=10)
except Exception:
    p.kill()
    out, err = p.communicate()

print("rc=%s" % p.returncode)
print("STDOUT=%r" % out[:2000])
print("STDERR=%r" % err[:2000])
print("log_exists=%s size=%s"
      % (os.path.exists(LOG), os.path.getsize(LOG) if os.path.exists(LOG) else -1))
