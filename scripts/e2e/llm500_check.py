#!/usr/bin/env python3
"""Real-binary check: rewrite HTTP 500 on a take whose target is uncertain
(nothing focused). argv: display binary label outdir focus(none|receiver-moved)."""
import os, sys, time, json, subprocess, signal
disp, binary, label, outdir, focus = sys.argv[1:6]
env = dict(os.environ, DISPLAY=disp, CANTRIP_OPENROUTER_BASE="http://127.0.0.1:18080/api/v1")
E2E = os.path.dirname(os.path.abspath(__file__))
stub_log = f"{outdir}/{label}-stub.jsonl"; open(stub_log, "w").close()
def sh(*a, inp=None):
    if inp is not None:  # xclip stays as selection owner: never wait on its stdout
        subprocess.run(a, text=True, env=env, input=inp, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return ""
    return subprocess.run(a, capture_output=True, text=True, env=env, timeout=10).stdout
def stub_lines(): return [json.loads(l) for l in open(stub_log)]
def shot(name): subprocess.run(["import", "-display", disp, "-window", "root", f"{outdir}/{name}.png"], env=env)
def hud_text():
    """The HUD's accessible description through AT-SPI (AccessKit), if exposed."""
    try:
        r = subprocess.run(["python3", "-I", os.path.join(E2E, "hud_atspi.py")], capture_output=True, text=True, env=env, timeout=8)
        return r.stdout.strip() or f"(not exposed; exit {r.returncode})"
    except Exception as e:
        return f"(atspi probe failed: {type(e).__name__})"
procs = [subprocess.Popen(["python3", "-I", f"{E2E}/openrouter_stub.py", "18080", "llm500", stub_log], env=env)]
recv_log = f"{outdir}/{label}-receiver.log"; open(recv_log, "w").close()
if focus == "receiver-moved":
    procs.append(subprocess.Popen(["python3", "-I", f"{E2E}/receiver.py", "A", recv_log], env=env))
    procs.append(subprocess.Popen(["python3", "-I", f"{E2E}/receiver.py", "B", recv_log], env=env))
    time.sleep(1.5)
    wa = sh("xdotool", "search", "--name", "^Receiver A$").split()[0]
    wb = sh("xdotool", "search", "--name", "^Receiver B$").split()[0]
    sh("xdotool", "windowmove", wb, "300", "150")
    sh("xdotool", "windowactivate", "--sync", wa); sh("xdotool", "mousemove", "--window", wa, "100", "40", "click", "1")
sh("xclip", "-selection", "clipboard", inp="user's earlier clipboard")
app = subprocess.Popen([binary], env=env, stdout=open(f"{outdir}/{label}-cantrip.log", "w"), stderr=subprocess.STDOUT)
procs.append(app)
for _ in range(100):
    if sh("xdotool", "search", "--name", "^Cantrip HUD$").split(): break
    time.sleep(0.1)
time.sleep(1.5)
res = {"label": label, "focus": focus, "active_window_at_start": sh("xprop", "-root", "_NET_ACTIVE_WINDOW").strip()}
sh("xdotool", "key", "ctrl+alt+space")      # tap: start
time.sleep(1.2)
if focus == "receiver-moved":
    sh("xdotool", "windowactivate", "--sync", wb)  # focus moves to B mid-take
sh("xdotool", "key", "ctrl+alt+space")      # tap: stop
t0 = time.monotonic()
while time.monotonic() - t0 < 8 and len(stub_lines()) < 2: time.sleep(0.02)
time.sleep(0.25)
shot(f"{label}-hud-both")
res["hud_atspi_text"] = hud_text()
res["clipboard_after_take"] = sh("xclip", "-o", "-selection", "clipboard")
sh("xclip", "-selection", "clipboard", inp="a foreign copy")   # another app copies
time.sleep(0.35)
shot(f"{label}-hud-claim-dropped")
res["hud_atspi_text_after_foreign_copy"] = hud_text()
res["stub_requests"] = [(r["path"], r["model"], (r.get("provider") or {}).get("data_collection")) for r in stub_lines()]
res["receiver_changes"] = sum(1 for _ in open(recv_log))
time.sleep(2.5)
res["staged_after"] = len(os.listdir(os.path.expanduser("~/.local/share/cantrip/v2/dictations")))
for p in reversed(procs):
    p.send_signal(signal.SIGTERM)
    try: p.wait(5)
    except Exception: p.kill()
print(json.dumps(res, indent=1, ensure_ascii=False))
