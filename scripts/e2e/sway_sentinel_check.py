#!/usr/bin/env python3
"""Reload-safe unbinds on a live Sway session (headless Sway 1.9 works).
argv: binary outdir mode [media_dir]

Modes:
- `noreload`: start a take with the chord while the `include`d file binds
  nothing; save `bindsym Escape exec …` to that file WITHOUT reloading.
  Escape must cancel the take (Sway runs `nop cantrip-cancel`, HUD hidden,
  staged WAV deleted, the receiver gets no Escape), then Cantrip's `nop` must
  be gone: idle and after Quit, Escape reaches the GTK receiver.
- `race-root` / `race-include`: start a take; save a user Escape and chord
  binding to the root config (`race-root`) or the included file
  (`race-include`); pause ONLY Cantrip's Sway listener thread
  (`thread_pause.py`, per-thread ptrace stop); `swaymsg reload`; the user's
  reloaded Escape fires; SIGTERM Cantrip (must exit 0) while the listener is
  still paused; after exit both user bindings must still fire.

Environment: as sway_check.py (SWAYSOCK, WAYLAND_DISPLAY, a session D-Bus
with a dummy key, the "Cantrip Test Mic" loopback, wtype, grim), plus
SWAY_CONFIG (the root config Sway was started with; it is rewritten as
SWAY_BASE_CONFIG + `include $INCLUDE_FILE`), INCLUDE_FILE and MARKER_DIR (the
user bindings append a line to $MARKER_DIR/escape or /chord). Uses the
loopback OpenRouter stub on port 18080, never a real provider; the takes are
cancelled or interrupted, so no request is expected. SHOT_PREFIX prefixes the
screenshot names. Prints one JSON report.
"""
import json, os, signal, socket, struct, subprocess, sys, threading, time

binary, outdir, mode = sys.argv[1], sys.argv[2], sys.argv[3]
media = sys.argv[4] if len(sys.argv) > 4 else outdir
assert mode in ("noreload", "race-root", "race-include"), mode
os.makedirs(outdir, exist_ok=True)
os.makedirs(media, exist_ok=True)
E2E = os.path.dirname(os.path.abspath(__file__))
env = dict(os.environ, CANTRIP_OPENROUTER_BASE="http://127.0.0.1:18080/api/v1")
env.pop("DISPLAY", None)
ROOT_CFG, BASE_CFG, INC = env["SWAY_CONFIG"], env["SWAY_BASE_CONFIG"], env["INCLUDE_FILE"]
MARK = env["MARKER_DIR"]
SHOT = env.get("SHOT_PREFIX", f"pass8-{mode}-")
data_home = env.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share")
dd = os.path.join(data_home, "cantrip/v2/dictations")
tag = f"{mode}-{os.path.basename(binary)}"
stub_log = f"{outdir}/{tag}-stub.jsonl"; open(stub_log, "w").close()
recv_log = f"{outdir}/{tag}-receiver.log"; open(recv_log, "w").close()
events_log = f"{outdir}/{tag}-receiver-events.log"; open(events_log, "w").close()
app_log_path = f"{outdir}/{tag}-cantrip.log"
USER_ESCAPE = f"bindsym Escape exec echo fired >> {MARK}/escape\n"
USER_CHORD = f"bindsym Control+Mod1+space exec echo fired >> {MARK}/chord\n"


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True, env=env, timeout=30).stdout


def swaymsg(*a):
    out = sh("swaymsg", "-r", *a)
    try:
        return json.loads(out)
    except ValueError:
        return out


def write(path, text):
    with open(path, "w") as f:
        f.write(text)


def marks(name):
    try:
        return sum(1 for _ in open(f"{MARK}/{name}"))
    except OSError:
        return 0


def staged():
    return len(os.listdir(dd)) if os.path.isdir(dd) else 0


def rx_keys():
    return [l.rstrip("\n").split("\t")[3] for l in open(events_log) if l.split("\t")[2] == "key"]


def modes():
    return swaymsg("-t", "get_binding_modes")


def sentinels():
    return [m for m in modes() if m.startswith("__cantrip_live_")]


def binding_state():
    return swaymsg("-t", "get_binding_state").get("name")


# --- Sway binding events (a separate subscriber), in arrival order ---
bind_events = []


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
            if t & 0x80000000:
                bind_events.append(json.loads(body).get("binding", {}).get("command"))
    except (EOFError, OSError):
        return


def ran_since(i):
    return bind_events[i:]


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


def hud_visible():
    return differs(base, grab(REGION))


shots = []


def shot(name, caption):
    """Full-output screenshot; `caption` (the step and Sway's state) is shown
    in the receiver's panel."""
    write(f"{MARK}/caption", caption)
    time.sleep(0.35)  # let the receiver's counter panel repaint
    subprocess.run(["grim", f"{media}/{SHOT}{name}"], env=env)
    shots.append(f"{SHOT}{name}")


LEAD = 0.15


def wtype(*a):
    subprocess.run(["wtype", "-s", str(int(LEAD * 1000)), *a], env=env, check=True)


def chord_tap():
    wtype("-M", "ctrl", "-M", "alt", "-k", "space", "-m", "alt", "-m", "ctrl")


def escape():
    """Press Escape; report what Sway ran and what reached the receiver."""
    i, k0, e0 = len(bind_events), len(rx_keys()), marks("escape")
    wtype("-k", "Escape"); time.sleep(0.5)
    return {"sway_ran": ran_since(i), "receiver_keys": rx_keys()[k0:],
            "user_escape_fired": marks("escape") - e0}


def chord():
    i, c0, s0 = len(bind_events), marks("chord"), staged()
    chord_tap(); time.sleep(0.6)
    return {"sway_ran": ran_since(i), "user_chord_fired": marks("chord") - c0,
            "staged_before": s0, "staged_after": staged()}


# --- A clean starting config: nothing user-bound, then a reload ---
os.makedirs(MARK, exist_ok=True)
for n in ("escape", "chord", "caption"):
    write(f"{MARK}/{n}", "")
base_text = open(BASE_CFG).read()
write(ROOT_CFG, base_text + f"\ninclude {INC}\n")
write(INC, "# nothing bound yet\n")
res = {"sway": swaymsg("-t", "get_version").get("human_readable"), "mode": mode,
       "binary": binary, "initial_reload": swaymsg("reload")}
time.sleep(0.5)
threading.Thread(target=subscriber, daemon=True).start()
procs = [subprocess.Popen(["python3", "-I", f"{E2E}/openrouter_stub.py", "18080", "ok", stub_log], env=env)]
procs.append(subprocess.Popen(["wtype", "-s", "360000000"], env=env))  # the seat's keyboard
procs.append(subprocess.Popen(
    ["python3", "-I", f"{E2E}/receiver.py", "A", recv_log],
    env=dict(env, RECEIVER_EVENTS=events_log, RECEIVER_COUNTER=MARK, GDK_BACKEND="wayland")))
time.sleep(2.0)
swaymsg("workspace", "1"); time.sleep(0.3)
swaymsg('[title="^Receiver A$"] focus'); time.sleep(0.3)
base = grab(REGION)
res["sentinels_before_cantrip"] = sentinels()

app_log = open(app_log_path, "w")
app = subprocess.Popen([binary], env=env, stdout=app_log, stderr=subprocess.STDOUT)
for _ in range(100):
    if "shortcut" in open(app_log_path).read():
        break
    time.sleep(0.1)
time.sleep(2.0)
res["sentinels_idle"] = sentinels()
res["binding_state_idle"] = binding_state()

# 1. Start a take with the chord.
res["take_start"] = chord()
res["listening"] = staged() > 0
res["hud_visible_recording"] = hud_visible()
res["sentinels_recording"] = sentinels()
res["binding_state_recording"] = binding_state()
shot("1-recording.png",
     f"[{mode}, {os.path.basename(binary)}] Step 1: Ctrl+Alt+Space started a take (Sway ran "
     f"{', '.join(res['take_start']['sway_ran'])}). HUD visible: {res['hud_visible_recording']}, "
     f"staged WAVs: {staged()}.\nSentinel modes: {res['sentinels_recording']}; "
     f"active mode: {res['binding_state_recording']}")

if mode == "noreload":
    # 2. Save a user Escape to the included file; no reload.
    write(INC, USER_ESCAPE)
    t1 = time.monotonic()
    i, k0 = len(bind_events), len(rx_keys())
    wtype("-k", "Escape")
    gone = deleted = None
    while time.monotonic() - t1 < 2.5 and not (gone and deleted):
        if deleted is None and staged() == 0:
            deleted = time.monotonic()
        if gone is None and not hud_visible():
            gone = time.monotonic()
    time.sleep(0.3)
    res["cancel"] = {"sway_ran": ran_since(i), "receiver_keys": rx_keys()[k0:],
                     "user_escape_fired": marks("escape"),
                     "escape_to_hud_gone_ms": round((gone - t1 - LEAD) * 1000) if gone else None,
                     "escape_to_wav_deleted_ms": round((deleted - t1 - LEAD) * 1000) if deleted else None,
                     "staged_after": staged()}
    res["sentinels_after_cancel"] = sentinels()
    shot("2-cancelled.png",
         f"[{mode}] Step 2: user Escape saved to {INC} WITHOUT reload; Escape pressed.\n"
         f"Sway ran {res['cancel']['sway_ran']}; HUD gone in {res['cancel']['escape_to_hud_gone_ms']} ms, "
         f"WAV deleted in {res['cancel']['escape_to_wav_deleted_ms']} ms (staged now {staged()}).\n"
         f"Sentinel still live: {res['sentinels_after_cancel']} -> Cantrip ran unbindsym Escape on its own nop")
    # 3. Idle: Escape is the receiver's (Cantrip's `nop` is gone).
    res["idle_escape"] = escape()
    res["idle_hud_visible"] = hud_visible()
    shot("3-idle-escape.png",
         f"[{mode}] Step 3: idle Escape -> Sway ran {res['idle_escape']['sway_ran']}, "
         f"receiver got {res['idle_escape']['receiver_keys']}. No nop cantrip-cancel left bound.")
else:
    # 2. Save user Escape + chord bindings, pause the listener, reload.
    if mode == "race-root":
        write(ROOT_CFG, base_text + f"\ninclude {INC}\n" + USER_ESCAPE + USER_CHORD)
    else:
        write(INC, USER_ESCAPE + USER_CHORD)
    pause = subprocess.Popen(
        ["sudo", "-n", "python3", "-I", f"{E2E}/thread_pause.py", str(app.pid), "cantrip-sway-events"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    res["listener_pause"] = pause.stdout.readline().strip()
    log_before = open(app_log_path).read().count("restored after")
    res["reload_reply"] = swaymsg("reload")
    time.sleep(0.8)
    res["sentinels_after_reload"] = sentinels()
    res["listener_reasserted_while_paused"] = open(app_log_path).read().count("restored after") > log_before
    # The user's reloaded Escape fires (Cantrip's nop is gone with the reload).
    res["reloaded_escape_during_take"] = escape()
    res["still_listening"] = staged() > 0
    res["hud_visible_after_reload"] = hud_visible()
    shot("2-reloaded-user-escape.png",
         f"[{mode}, {os.path.basename(binary)}] Step 2: user Escape+chord saved to "
         f"{'root config' if mode == 'race-root' else INC}; listener thread paused ({res['listener_pause']});\n"
         f"swaymsg reload; sentinel modes now {res['sentinels_after_reload']} (wiped). "
         f"Escape -> Sway ran {res['reloaded_escape_during_take']['sway_ran']}.\n"
         f"Take still listening (listener paused): {res['still_listening']}, HUD visible: {res['hud_visible_after_reload']}")

# 4. Quit (SIGTERM) and probe again.
t = time.monotonic()
app.send_signal(signal.SIGTERM)
try:
    res["app_exit_code"] = app.wait(10)
except subprocess.TimeoutExpired:
    app.kill(); res["app_exit_code"] = "killed after 10 s"
res["exit_ms"] = round((time.monotonic() - t) * 1000)
if mode != "noreload":
    pause.stdin.close()
    res["listener_pause_end"] = pause.stdout.readline().strip()
    pause.wait(5)
time.sleep(0.5)
res["post_quit_escape"] = escape()
res["post_quit_hud_visible"] = hud_visible()
if mode != "noreload":
    res["post_quit_chord"] = chord()
shot("3-post-quit.png" if mode != "noreload" else "4-post-quit.png",
     f"[{mode}, {os.path.basename(binary)}] After SIGTERM (exit code {res['app_exit_code']}): "
     f"Escape -> Sway ran {res['post_quit_escape']['sway_ran']}, receiver got "
     f"{res['post_quit_escape']['receiver_keys']}"
     + (f";\nchord -> Sway ran {res['post_quit_chord']['sway_ran']}" if mode != "noreload" else "")
     + f".\nHUD visible: {res['post_quit_hud_visible']}")
res["sentinels_post_quit"] = sentinels()
res["binding_state_post_quit"] = binding_state()
if mode == "noreload":
    # The saved user binding takes over at the next reload.
    swaymsg("reload"); time.sleep(0.5)
    res["after_reload_escape"] = escape()
res["stub_requests"] = sum(1 for _ in open(stub_log))
res["cantrip_log"] = [l.strip() for l in open(app_log_path) if l.startswith("[cantrip]")]
res["screenshots"] = shots
for p in reversed(procs):
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(5)
    except Exception:
        p.kill()
# Restore the clean config for the next run.
write(ROOT_CFG, base_text + f"\ninclude {INC}\n")
write(INC, "# nothing bound yet\n")
swaymsg("reload")
print(json.dumps(res, indent=1))
