#!/usr/bin/env python3
"""Sway IPC shortcut checks on a live Sway session (headless Sway 1.9 works:
WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 sway -c CONFIG).
argv: binary outdir mode [N]

Same environment as sway_check.py (SWAYSOCK, WAYLAND_DISPLAY, a session
D-Bus with a dummy key, the "Cantrip Test Mic" loopback, wtype, grim); uses
the loopback OpenRouter stub on port 18080, never a real provider.

Modes:
- `latency N`: N tap-to-toggle takes with a GTK receiver focused. Each is
  timed from the Sway IPC `binding` event for `nop cantrip-hotkey-down` (a
  separate subscriber in this script) to the first `grim` capture of the
  HUD's screen region that shows the pill (an upper bound: it includes the
  capture itself). Each take is cancelled with plain Escape.
- `reload`: the chord works and Escape cancels; `swaymsg reload` (idle);
  after <= 1 s the chord works again with no restart, Escape cancels; a
  reload during a take keeps Escape cancelling; after a reload while idle,
  Escape reaches the receiver again.
- `include`: run with a Sway config whose `include`d file binds plain Escape
  and Control+Mod1+space to `exec touch <marker>` (see the evidence notes):
  both user bindings fire while Cantrip runs idle, a take started with
  `cantrip toggle` leaves Escape to the user binding, and both still fire
  after Cantrip quits.
Prints one JSON report.
"""
import json, os, signal, socket, struct, subprocess, sys, threading, time

binary, outdir, mode = sys.argv[1], sys.argv[2], sys.argv[3]
n_taps = int(sys.argv[4]) if len(sys.argv) > 4 else 15
os.makedirs(outdir, exist_ok=True)
E2E = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(E2E))
env = dict(os.environ, CANTRIP_OPENROUTER_BASE="http://127.0.0.1:18080/api/v1")
env.pop("DISPLAY", None)
data_home = env.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share")
dd = os.path.join(data_home, "cantrip/v2/dictations")
stub_log = f"{outdir}/{mode}-stub.jsonl"; open(stub_log, "w").close()
recv_log = f"{outdir}/{mode}-receiver.log"; open(recv_log, "w").close()
events_log = f"{outdir}/{mode}-receiver-events.log"; open(events_log, "w").close()
app_log_path = f"{outdir}/{mode}-cantrip.log"
MARK = os.environ.get("MARKER_DIR", "/tmp/sw/markers")


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True, env=env, timeout=30).stdout


def swaymsg(*a):
    out = sh("swaymsg", "-r", *a)
    try:
        return json.loads(out)
    except ValueError:
        return out


def staged():
    return len(os.listdir(dd)) if os.path.isdir(dd) else 0


def stub_reqs():
    return sum(1 for _ in open(stub_log))


def rx_keys():
    return [l.rstrip("\n").split("\t")[3] for l in open(events_log) if l.split("\t")[2] == "key"]


# --- Sway binding events, timestamped on CLOCK_MONOTONIC as they arrive ---
bind_events = []  # (monotonic s, command)


def subscriber():
    s = socket.socket(socket.AF_UNIX)
    s.connect(env["SWAYSOCK"])
    p = b'["binding"]'
    s.sendall(b"i3-ipc" + struct.pack("=II", len(p), 2) + p)

    def rd(n):
        b = b""
        while len(b) < n:
            c = s.recv(n - len(b))
            if not c:
                raise EOFError
            b += c
        return b
    try:
        while True:
            h = rd(14)
            ln, t = struct.unpack("=II", h[6:])
            body = rd(ln)
            now = time.monotonic()
            if t & 0x80000000:
                cmd = json.loads(body).get("binding", {}).get("command")
                bind_events.append((now, cmd))
    except (EOFError, OSError):
        return


threading.Thread(target=subscriber, daemon=True).start()


def wait_bind(cmd, after, timeout=2.0):
    t0 = time.monotonic()
    while time.monotonic() - t0 < timeout:
        for i, (t, c) in enumerate(bind_events):
            if i >= after and c == cmd:
                return t
        time.sleep(0.001)
    return None


# --- Screen: the HUD region (pill is bottom-centre, 72 px above the edge) ---
REGION = "440,600 400x140"


def grab(geometry=None):
    g = ["-g", geometry] if geometry else []
    raw = subprocess.run(["grim", *g, "-t", "ppm", "-"], capture_output=True, env=env).stdout
    parts, i = [], 0
    while len(parts) < 4:
        j = raw.index(b"\n", i) if len(parts) == 3 else min(
            x for x in (raw.find(b" ", i), raw.find(b"\n", i)) if x >= 0)
        tok = raw[i:j].strip()
        if tok:
            parts.append(tok)
        i = j + 1
    return int(parts[1]), int(parts[2]), raw[i:]


def differs(base, cur, step=2):
    w, h, a = base
    _, _, b = cur
    n = 0
    for y in range(0, h, step):
        row = y * w * 3
        for x in range(0, w, step):
            k = row + x * 3
            if abs(a[k] - b[k]) + abs(a[k + 1] - b[k + 1]) + abs(a[k + 2] - b[k + 2]) > 30:
                n += 1
                if n > 40:
                    return True
    return False


def shot(name):
    subprocess.run(["grim", f"{outdir}/{name}"], env=env)


LEAD = 0.15


def wtype(*a):
    subprocess.run(["wtype", "-s", str(int(LEAD * 1000)), *a], env=env, check=True)


def chord_tap():
    wtype("-M", "ctrl", "-M", "alt", "-k", "space", "-m", "alt", "-m", "ctrl")


res = {"sway": swaymsg("-t", "get_version").get("human_readable"), "mode": mode}
procs = [subprocess.Popen(["python3", "-I", f"{E2E}/openrouter_stub.py", "18080", "ok", stub_log], env=env)]
procs.append(subprocess.Popen(["wtype", "-s", "360000000"], env=env))  # the seat's keyboard
procs.append(subprocess.Popen(["python3", "-I", f"{E2E}/receiver.py", "A", recv_log],
                              env=dict(env, RECEIVER_EVENTS=events_log, GDK_BACKEND="wayland")))
time.sleep(2.0)
swaymsg("workspace", "1"); time.sleep(0.3)
swaymsg('[title="^Receiver A$"] focus'); time.sleep(0.3)
base = grab(REGION)

app_log = open(app_log_path, "w")
app = subprocess.Popen([binary], env=env, stdout=app_log, stderr=subprocess.STDOUT)
for _ in range(100):
    if "shortcut" in open(app_log_path).read():
        break
    time.sleep(0.1)
time.sleep(2.0)
res["cantrip_log_start"] = [l.strip() for l in open(app_log_path) if l.startswith("[cantrip]")]


def take_and_cancel(label):
    """Tap the chord; time binding event → HUD; Escape cancels."""
    mark = len(bind_events)
    keys0 = len(rx_keys())
    t_tap = time.monotonic() + LEAD
    threading.Thread(target=chord_tap).start()
    t_ev = wait_bind("nop cantrip-hotkey-down", mark)
    t_hud = None
    t_end = time.monotonic() + 2.0
    while time.monotonic() < t_end:
        if differs(base, grab(REGION)):
            t_hud = time.monotonic()
            break
    time.sleep(0.4)
    r = {"label": label,
         "binding_event_seen": t_ev is not None,
         "binding_event_to_hud_visible_ms": round((t_hud - t_ev) * 1000, 1) if (t_ev and t_hud) else None,
         "tap_to_binding_event_ms": round((t_ev - t_tap) * 1000, 1) if t_ev else None,
         "staged_while_listening": staged(),
         "focused": walk_focus()}
    mark2 = len(bind_events)
    t1 = time.monotonic() + LEAD
    wtype("-k", "Escape")
    t_cancel = wait_bind("nop cantrip-cancel", mark2)
    gone = deleted = None
    while time.monotonic() - t1 < 2.0 and not (gone and deleted):
        if deleted is None and staged() == 0:
            deleted = time.monotonic()
        if gone is None and not differs(base, grab(REGION)):
            gone = time.monotonic()
    r["escape_binding_event_seen"] = t_cancel is not None
    r["escape_to_hud_gone_ms"] = round((gone - t1) * 1000) if gone else None
    r["escape_to_audio_deleted_ms"] = round((deleted - t1) * 1000) if deleted else None
    r["receiver_keys_during_take"] = rx_keys()[keys0:]
    return r


def walk_focus():
    out = []
    def walk(n):
        if n.get("focused") and n.get("type") in ("con", "floating_con"):
            out.append(n.get("name"))
        for c in n.get("nodes", []) + n.get("floating_nodes", []):
            walk(c)
    walk(swaymsg("-t", "get_tree"))
    return out


def idle_escape_reaches_receiver():
    k0 = len(rx_keys())
    wtype("-k", "Escape"); time.sleep(0.3)
    return rx_keys()[k0:] == ["Escape"]


if mode == "latency":
    taps = []
    for i in range(n_taps):
        taps.append(take_and_cancel(f"tap {i + 1}"))
        time.sleep(0.4)
    lat = [t["binding_event_to_hud_visible_ms"] for t in taps]
    res["taps"] = taps
    res["latencies_ms"] = lat
    res["within_100ms"] = f"{sum(1 for x in lat if x is not None and x <= 100)}/{len(lat)}"
    res["max_ms"] = max((x for x in lat if x is not None), default=None)
    res["stub_requests"] = stub_reqs()
elif mode == "reload":
    res["before_reload"] = take_and_cancel("before reload")
    res["idle_escape_before_reload"] = idle_escape_reaches_receiver()
    res["reload_reply"] = swaymsg("reload")
    time.sleep(1.0)  # the brief's "<= 1 s"
    res["after_reload"] = take_and_cancel("after idle reload (1 s)")
    # Reload in the middle of a take: Escape must still cancel.
    chord_tap(); time.sleep(0.6)
    res["listening_before_mid_take_reload"] = staged() > 0
    swaymsg("reload"); time.sleep(1.0)
    shot("pass6-sway-reload-mid-take.png")
    mark = len(bind_events)
    wtype("-k", "Escape"); time.sleep(0.6)
    res["mid_take_reload_escape_cancels"] = wait_bind("nop cantrip-cancel", mark, 0.5) is not None and staged() == 0
    swaymsg("reload"); time.sleep(1.0)
    res["idle_escape_after_reloads"] = idle_escape_reaches_receiver()
    res["after_third_reload"] = take_and_cancel("after third reload")
    res["stub_requests"] = stub_reqs()
elif mode == "include":
    os.makedirs(MARK, exist_ok=True)

    def fired(name, act):
        p = f"{MARK}/{name}"
        if os.path.exists(p):
            os.unlink(p)
        act(); time.sleep(0.5)
        return os.path.exists(p)
    res["idle_user_escape_fires"] = fired("escape", lambda: wtype("-k", "Escape"))
    res["idle_user_chord_fires"] = fired("chord", chord_tap)
    res["staged_after_user_chord"] = staged()
    # A take from `cantrip toggle`: Escape stays the user's.
    subprocess.run([binary, "toggle"], env=env); time.sleep(0.8)
    res["toggle_take_listening"] = staged() > 0
    shot("pass6-sway-include-take.png")
    res["take_user_escape_fires"] = fired("escape", lambda: wtype("-k", "Escape"))
    res["take_still_listening_after_escape"] = staged() > 0
    subprocess.run([binary, "toggle"], env=env)
    t = time.monotonic()
    while time.monotonic() - t < 10 and "Tuesday" not in open(recv_log).read():
        time.sleep(0.05)
    res["toggle_take_delivered"] = "Tuesday" in open(recv_log).read()
    time.sleep(1.0)
    res["after_take_user_escape_fires"] = fired("escape", lambda: wtype("-k", "Escape"))
    res["stub_requests"] = [json.loads(l)["path"] for l in open(stub_log)]

app.send_signal(signal.SIGTERM)
try:
    res["app_exit_code"] = app.wait(8)
except subprocess.TimeoutExpired:
    app.kill(); res["app_exit_code"] = "killed after 8 s"
if mode == "include":
    res["after_quit_user_escape_fires"] = fired("escape", lambda: wtype("-k", "Escape"))
    res["after_quit_user_chord_fires"] = fired("chord", chord_tap)
else:
    res["after_quit_idle_escape_reaches_receiver"] = idle_escape_reaches_receiver()
res["cantrip_log"] = [l.strip() for l in open(app_log_path) if l.startswith("[cantrip]")]
for p in reversed(procs):
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(5)
    except Exception:
        p.kill()
print(json.dumps(res, indent=1))
