#!/usr/bin/env python3
"""Real-binary Escape check: launch BINARY on DISPLAY with a loopback stub,
focus a GTK receiver, XTEST-tap Ctrl+Alt+Space, then press plain Escape.
Reports HUD state, staged audio and stub request counts. argv: display binary
label outdir [check_next_press]."""
import os, sys, time, json, subprocess, signal
disp, binary, label, outdir = sys.argv[1:5]
check_next = len(sys.argv) > 5
env = dict(os.environ, DISPLAY=disp, CANTRIP_OPENROUTER_BASE="http://127.0.0.1:18080/api/v1")
E2E = os.path.dirname(os.path.abspath(__file__))
stub_log = f"{outdir}/{label}-stub.jsonl"; open(stub_log, "w").close()
recv_log = f"{outdir}/{label}-receiver.log"; open(recv_log, "w").close()
dd = os.path.expanduser("~/.local/share/cantrip/v2/dictations")
def sh(*a): return subprocess.run(a, capture_output=True, text=True, env=env).stdout
def staged(): return len(os.listdir(dd)) if os.path.isdir(dd) else 0
def stub_reqs(): return sum(1 for _ in open(stub_log))
procs = []
procs.append(subprocess.Popen(["python3", "-I", f"{E2E}/openrouter_stub.py", "18080", "ok", stub_log], env=env))
procs.append(subprocess.Popen(["python3", "-I", f"{E2E}/receiver.py", "A", recv_log], env=env))
time.sleep(1.5)
win = sh("xdotool", "search", "--name", "^Receiver A$").split()[0]
sh("xdotool", "windowactivate", "--sync", win); sh("xdotool", "mousemove", "--window", win, "100", "40", "click", "1")
app_log = open(f"{outdir}/{label}-cantrip.log", "w")
app = subprocess.Popen([binary], env=env, stdout=app_log, stderr=subprocess.STDOUT)
procs.append(app)
hud = None
for _ in range(100):
    out = sh("xdotool", "search", "--name", "^Cantrip HUD$").split()
    if out: hud = out[0]; break
    time.sleep(0.1)
time.sleep(1.5)
def hud_x():
    g = sh("xdotool", "getwindowgeometry", hud)
    return int(g.split("Position: ")[1].split(",")[0])
def shot(name):
    subprocess.run(["import", "-display", disp, "-window", "root", f"{outdir}/{name}.png"], env=env)
res = {"label": label, "display": disp, "hud_window": bool(hud)}
sh("xdotool", "key", "ctrl+alt+space")   # a tap: tap-to-toggle keeps listening
time.sleep(0.8)
res["hud_onscreen_after_tap"] = hud_x() > -1000
res["staged_items_while_listening"] = staged()
shot(f"{label}-1-listening")
t1 = time.monotonic(); sh("xdotool", "key", "Escape")
gone = deleted = None
while time.monotonic() - t1 < 2.0:
    if gone is None and hud_x() < -1000: gone = time.monotonic()
    if deleted is None and staged() == 0: deleted = time.monotonic()
    if gone and deleted: break
    time.sleep(0.005)
res["escape_to_hud_hidden_ms"] = round((gone - t1) * 1000) if gone else None
res["escape_to_audio_deleted_ms"] = round((deleted - t1) * 1000) if deleted else None
time.sleep(0.3)
shot(f"{label}-2-after-escape")
time.sleep(3.0)
res["hud_onscreen_3s_after_escape"] = hud_x() > -1000
res["staged_items_3s_after_escape"] = staged()
res["stub_requests_after_escape"] = stub_reqs()
if check_next:
    sh("xdotool", "key", "ctrl+alt+space")   # next shortcut press
    time.sleep(4.0)
    res["stub_requests_after_next_press"] = stub_reqs()
    res["receiver_lines_after_next_press"] = sum(1 for _ in open(recv_log))
    shot(f"{label}-3-after-next-press")
grabs = [l.strip() for l in open(f"{outdir}/{label}-cantrip.log") if "escape" in l.lower()]
res["cantrip_log_escape_lines"] = grabs
for p in reversed(procs):
    p.send_signal(signal.SIGTERM)
    try: p.wait(5)
    except Exception: p.kill()
print(json.dumps(res, indent=1))
