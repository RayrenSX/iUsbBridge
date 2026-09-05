import json
import struct
import subprocess
import sys
import time

udid = sys.argv[1]
exe = r"C:\Users\Ray\Documents\iUsbBridge\dist\iUsbBridge\iUsbBridge.exe"
proc = subprocess.Popen([exe, "--usb", "--udid", udid], stdin=subprocess.PIPE,
                        stdout=subprocess.PIPE, stderr=subprocess.STDOUT)

def read_event(timeout=180):
    deadline = time.time() + timeout
    while time.time() < deadline:
        line = proc.stdout.readline()
        if line:
            text = line.decode("utf-8", "replace").rstrip()
            print(text, flush=True)
            try:
                return json.loads(text)
            except ValueError:
                continue
        if proc.poll() is not None:
            return None
    return None

ready = False
while True:
    event = read_event()
    if not event:
        break
    if event.get("event") == "ready":
        ready = True
        break
    if event.get("event") == "error":
        break

if ready:
    points = [{"pointerId": i, "action": "down", "normalizedX": 0.15 + i * 0.17,
               "normalizedY": 0.5} for i in range(5)]
    for action, y in (("down", 0.5), ("move", 0.55), ("up", 0.55)):
        payload_points = [dict(point, action=action, normalizedY=y) for point in points]
        payload = json.dumps({"schema": "iphoneMirror.touch.v2", "kind": "touch_batch",
                              "seq": time.time_ns() & 0x7fffffff, "timestampNs": time.time_ns(),
                              "points": payload_points}).encode()
        proc.stdin.write(struct.pack("<I", len(payload)) + payload)
        proc.stdin.flush()
        time.sleep(0.4)
    proc.stdin.close()
    time.sleep(1)
proc.terminate()
proc.wait(timeout=10)
if not ready:
    raise SystemExit("bridge did not become ready")
