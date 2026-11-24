#!/usr/bin/env python3
"""Real-binary check on a live Sway session (e.g. headless Sway 1.9:
WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 sway). argv: binary outdir
[media_dir].

Needs, in the environment: SWAYSOCK and WAYLAND_DISPLAY of that session, a
session D-Bus with an unlocked Secret Service holding a (dummy) key, PipeWire
with the loopback "Cantrip Test Mic" (sink node `cantrip_test_mic_in`, see
README.md), `wtype` and `grim`, and optionally VPTR=<path of the vptr helper
in ./vptr> for the click-through check. Uses the loopback OpenRouter stub on
port 18080; never a real provider. A headless seat has no keyboard or pointer
device: the script keeps one virtual keyboard alive for the whole run (as a
real seat always has one; toolkits report a focused, active window only while
the seat has a keyboard) and leads each `wtype` call with a short pause (a
fresh virtual keymap otherwise loses its first key in GTK clients).

Drives the real shortcut with `wtype` (virtual keyboard through Sway's
bindings) while a GTK receiver is focused, and measures: the shortcut backend;
HUD placement (pixel diff against a no-HUD baseline) on workspace 1 and 2;
that the receiver keeps focus, gets pointer clicks through the HUD and never
sees the cancelling Escape; Escape → HUD hidden / staged audio deleted; zero
stub requests after a cancel; Escape reaching apps again when idle; a full
hold-to-talk and a tap-to-toggle take delivered through the stub; and that no
Sway binding is left after the app quits.
"""
import json, os, signal, subprocess, sys, time

binary, outdir = sys.argv[1], sys.argv[2]
media = sys.argv[3] if len(sys.argv) > 3 else outdir
os.makedirs(outdir, exist_ok=True)
E2E = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(E2E))
env = dict(os.environ, CANTRIP_OPENROUTER_BASE="http://127.0.0.1:18080/api/v1")
env.pop("DISPLAY", None)
data_home = env.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share")
dd = os.path.join(data_home, "cantrip/v2/dictations")
stub_log = f"{outdir}/sway-stub.jsonl"; open(stub_log, "w").close()
recv_log = f"{outdir}/sway-receiver.log"; open(recv_log, "w").close()
events_log = f"{outdir}/sway-receiver-events.log"; open(events_log, "w").close()
app_log_path = f"{outdir}/sway-cantrip.log"


def sh(*a, check=False):
    r = subprocess.run(a, capture_output=True, text=True, env=env, timeout=30)
    if check and r.returncode != 0:
        raise SystemExit(f"{a}: {r.stderr}")
    return r.stdout


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


def events():
    return [l.rstrip("\n").split("\t")[2:] for l in open(events_log)]


def tree_windows():
    out = []
    def walk(n, ws=None):
        if n.get("type") == "workspace":
            ws = n["name"]
        if n.get("type") in ("con", "floating_con") and n.get("app_id"):
            out.append({"workspace": ws, "title": n.get("name"), "app_id": n["app_id"], "focused": n["focused"],
                        "rect": n["rect"]})
        for c in n.get("nodes", []) + n.get("floating_nodes", []):
            walk(c, ws)
    walk(swaymsg("-t", "get_tree"))
    return out


def focused_title():
    f = [w["title"] for w in tree_windows() if w["focused"]]
    return f[0] if f else None


STRIP = "0,640 1280x160"  # bottom strip polled for timing (fast to grab)


def grab(geometry=None):
    """Screenshot (full output or `geometry`) as (w, h, rgb bytes)."""
    g = ["-g", geometry] if geometry else []
    raw = subprocess.run(["grim", *g, "-t", "ppm", "-"], capture_output=True, env=env).stdout
    parts, i = [], 0
    while len(parts) < 4:  # P6 w h maxval
        j = raw.index(b"\n", i) if len(parts) == 3 else min(
            x for x in (raw.find(b" ", i), raw.find(b"\n", i)) if x >= 0)
        tok = raw[i:j].strip()
        if tok:
            parts.append(tok)
        i = j + 1
    return int(parts[1]), int(parts[2]), raw[i:]


def bbox(base, cur, y_from=500, step=1):
    """Bounding box of pixels that differ from the baseline (below y_from:
    the bar's clock and the entry's caret are above it)."""
    w, h, a = base
    _, _, b = cur
    xs, ys = [], []
    for y in range(y_from, h, step):
        row = y * w * 3
        for x in range(0, w, step):
            k = row + x * 3
            if abs(a[k] - b[k]) + abs(a[k + 1] - b[k + 1]) + abs(a[k + 2] - b[k + 2]) > 30:
                xs.append(x); ys.append(y)
    if not xs:
        return None
    return {"x0": min(xs), "y0": min(ys), "x1": max(xs) + 1, "y1": max(ys) + 1,
            "center_x": (min(xs) + max(xs) + 1) / 2, "screen_w": w, "screen_h": h,
            "gap_to_bottom": h - (max(ys) + 1)}


def shot(name):
    subprocess.run(["grim", f"{media}/{name}"], env=env)


SHOT = os.environ.get("SHOT_PREFIX", "")  # screenshot name prefix
LEAD = 0.15  # wtype lead-in before the first key (s); excluded from timings


def wtype(*a):
    subprocess.run(["wtype", "-s", str(int(LEAD * 1000)), *a], env=env, check=True)


def chord_tap():
    wtype("-M", "ctrl", "-M", "alt", "-k", "space", "-m", "alt", "-m", "ctrl")


def unbind_probe(spec):
    """`unbindsym` succeeds only if the binding exists: an error proves none
    is left. (If one were left, the probe removes it and reports it.)"""
    r = swaymsg(*(["--"] if spec.startswith("--") else []), "unbindsym", *spec.split())
    ok = isinstance(r, list) and r and r[0].get("success")
    return "LEFT BOUND (removed by probe)" if ok else "not bound"


res = {"sway": swaymsg("-t", "get_version").get("human_readable")}
procs = [subprocess.Popen(["python3", "-I", f"{E2E}/openrouter_stub.py", "18080", "ok", stub_log], env=env)]
# The seat's persistent keyboard (headless Sway has none of its own).
procs.append(subprocess.Popen(["wtype", "-s", "360000000"], env=env))
procs.append(subprocess.Popen(["python3", "-I", f"{E2E}/receiver.py", "A", recv_log],
                              env=dict(env, RECEIVER_EVENTS=events_log, GDK_BACKEND="wayland")))
time.sleep(2.0)
# Baselines without any HUD: workspace 2 (empty) and workspace 1 (receiver).
swaymsg("workspace", "2"); time.sleep(0.4); base_ws2 = grab()
swaymsg("workspace", "1"); time.sleep(0.4)
swaymsg('[title="^Receiver A$"] focus'); time.sleep(0.3)
base_ws1 = grab()
base_strip = grab(STRIP)

app_log = open(app_log_path, "w")
app = subprocess.Popen([binary], env=env, stdout=app_log, stderr=subprocess.STDOUT)
for _ in range(100):
    if "shortcut" in open(app_log_path).read():
        break
    time.sleep(0.1)
time.sleep(2.0)
res["cantrip_log"] = [l.strip() for l in open(app_log_path) if l.startswith("[cantrip]")]
res["windows_at_start"] = tree_windows()
res["focused_before_take"] = focused_title()
res["hud_after_startup_bbox"] = bbox(base_ws1, grab())

# 1. Tap: tap-to-toggle keeps listening; HUD bottom-centre, receiver focused.
t0 = time.monotonic() + LEAD; chord_tap()
seen_audio = seen_hud = None
while time.monotonic() - t0 < 2.0 and not (seen_audio and seen_hud):
    if seen_audio is None and staged() > 0:
        seen_audio = time.monotonic()
    if seen_hud is None and bbox(base_strip, grab(STRIP), y_from=0, step=2):
        seen_hud = time.monotonic()
res["tap_to_staged_audio_ms"] = round((seen_audio - t0) * 1000) if seen_audio else None
res["tap_to_hud_visible_ms_upper_bound_incl_grim"] = round((seen_hud - t0) * 1000) if seen_hud else None
time.sleep(0.6)
res["ws1_hud_bbox"] = bbox(base_ws1, grab())
res["focused_while_recording"] = focused_title()
shot(f"{SHOT}sway-ws1-bottom.png")
# Pointer click on the pill reaches the receiver beneath it (click-through).
hb = res["ws1_hud_bbox"]
if hb:
    cx, cy = int(hb["center_x"]), (hb["y0"] + hb["y1"]) // 2
    vptr = os.environ.get("VPTR")
    if vptr:
        subprocess.run([vptr, str(cx), str(cy), str(hb["screen_w"]), str(hb["screen_h"])], env=env)
        time.sleep(0.3)
        res["click_on_pill_at"] = [cx, cy]
        res["receiver_pointer_events"] = [e for e in events() if e[0] == "button"]
        res["focused_after_click"] = focused_title()
        res["hud_bbox_after_click"] = bbox(base_ws1, grab())
    else:
        res["click_on_pill_at"] = "skipped (set VPTR)"

# 2. Workspace 2 while recording.
swaymsg("workspace", "2"); time.sleep(0.5)
res["ws2_hud_bbox"] = bbox(base_ws2, grab())
shot(f"{SHOT}sway-ws2-visible.png")
swaymsg("workspace", "1"); time.sleep(0.5)
res["back_on_ws1_hud_bbox"] = bbox(base_ws1, grab())
res["focused_back_on_ws1"] = focused_title()
res["staged_items_while_listening"] = staged()

# 3. Plain Escape cancels: HUD gone, audio deleted, receiver never sees it.
keys_before = [e for e in events() if e[0] == "key"]
t1 = time.monotonic() + LEAD; wtype("-k", "Escape")
gone = deleted = None
while time.monotonic() - t1 < 2.0 and not (gone and deleted):
    if deleted is None and staged() == 0:
        deleted = time.monotonic()
    if gone is None and bbox(base_strip, grab(STRIP), y_from=0, step=2) is None:
        gone = time.monotonic()
res["escape_to_audio_deleted_ms"] = round((deleted - t1) * 1000) if deleted else None
res["escape_to_hud_gone_ms_upper_bound_incl_grim"] = round((gone - t1) * 1000) if gone else None
time.sleep(0.3)
shot(f"{SHOT}sway-after-escape.png")
res["receiver_keys_during_cancel"] = [e for e in events() if e[0] == "key"][len(keys_before):]
time.sleep(3.0)
res["stub_requests_3s_after_escape"] = stub_reqs()
res["staged_items_3s_after_escape"] = staged()
res["hud_bbox_3s_after_escape"] = bbox(base_ws1, grab())
res["escape_binding_when_idle"] = unbind_probe("Escape")
wtype("-k", "Escape"); time.sleep(0.3)
res["idle_escape_reaches_receiver"] = ["key", "Escape"] in events()

# 4. Hold-to-talk: speech into the virtual mic while the chord is held.
play = subprocess.Popen(["pw-play", "--target", "cantrip_test_mic_in", f"{ROOT}/samples/jfk.wav"], env=env)
time.sleep(0.2)
wtype("-M", "ctrl", "-M", "alt", "-P", "space", "-s", "4000", "-p", "space", "-m", "alt", "-m", "ctrl")
t2 = time.monotonic()  # released
while time.monotonic() - t2 < 15 and "Tuesday" not in open(recv_log).read():
    time.sleep(0.05)
res["hold_release_to_delivered_ms"] = round((time.monotonic() - t2) * 1000)
play.wait()
time.sleep(0.4)
shot(f"{SHOT}sway-hold-delivered.png")
res["receiver_text_after_hold"] = open(recv_log).read().strip().split("\n")[-1:]
res["stub_requests_after_hold"] = [json.loads(l)["path"] for l in open(stub_log)]
res["stub_data_collection_deny"] = all((json.loads(l).get("provider") or {}).get("data_collection") == "deny"
                                       for l in open(stub_log) if "chat" in json.loads(l)["path"])
res["staged_items_after_hold"] = staged()
time.sleep(4.0)

# 5. Tap-to-toggle: tap, speak, tap.
n_before = sum(1 for _ in open(recv_log))
chord_tap(); time.sleep(0.3)
play = subprocess.Popen(["pw-play", "--target", "cantrip_test_mic_in", f"{ROOT}/samples/jfk.wav"], env=env)
play.wait(); time.sleep(0.3)
res["tap_toggle_still_listening"] = staged() > 0
chord_tap()
t3 = time.monotonic()
while time.monotonic() - t3 < 15 and sum(1 for _ in open(recv_log)) == n_before:
    time.sleep(0.05)
time.sleep(0.5)
res["receiver_lines_after_toggle_take"] = sum(1 for _ in open(recv_log)) - n_before
res["stub_requests_total"] = stub_reqs()
res["escape_binding_when_idle_after_takes"] = unbind_probe("Escape")

# 6. Rebind in Settings (Record, then the new chord): the old chord's press
# and release bindings go, the new chord works; then rebind back.
vptr = os.environ.get("VPTR")
def record(chord_args):
    subprocess.run([binary, "settings"], env=env); time.sleep(1.5)
    win = [w for w in tree_windows() if w["title"] == "Cantrip Settings"]
    if not win:
        return False
    r = win[0]["rect"]  # Record button: right of the Shortcut field
    subprocess.run([vptr, str(r["x"] + 364), str(r["y"] + 33), "1280", "800"], env=env)
    time.sleep(0.4); wtype(*chord_args); time.sleep(1.0)
    return True
if vptr and record(["-M", "ctrl", "-M", "alt", "-k", "d", "-m", "alt", "-m", "ctrl"]):
    res["rebind_log"] = [l.strip() for l in open(app_log_path) if "shortcut" in l][-1:]
    res["rebind_settings_notices"] = [l.strip() for l in open(app_log_path) if "not saved" in l]
    shot(f"{SHOT}sway-settings-rebound.png")
    swaymsg('[title="^Receiver A$"] focus'); time.sleep(0.3)
    chord_tap(); time.sleep(0.6)
    res["old_chord_starts_take_after_rebind"] = staged() > 0
    wtype("-M", "ctrl", "-M", "alt", "-k", "d", "-m", "alt", "-m", "ctrl"); time.sleep(0.6)
    res["new_chord_starts_take"] = staged() > 0
    wtype("-k", "Escape"); time.sleep(0.6)
    res["new_chord_take_cancelled_by_escape"] = staged() == 0
    res["old_chord_after_rebind"] = {s: unbind_probe(s) for s in
                                     ["Control+Mod1+space", "--release Control+Mod1+space"]}
    record(["-M", "ctrl", "-M", "alt", "-k", "space", "-m", "alt", "-m", "ctrl"])
    res["rebind_back_log"] = [l.strip() for l in open(app_log_path) if "shortcut" in l][-1:]
    swaymsg('[title="^Cantrip Settings$"] kill'); time.sleep(0.5)
    swaymsg('[title="^Receiver A$"] focus'); time.sleep(0.3)
else:
    res["rebind"] = "skipped (set VPTR)"

# 7. Quit (SIGTERM is a graceful Quit): no Sway binding left behind.
app.send_signal(signal.SIGTERM)
try:
    res["app_exit_code"] = app.wait(8)
except subprocess.TimeoutExpired:
    app.kill(); res["app_exit_code"] = "killed after 8 s"
res["after_quit"] = {s: unbind_probe(s) for s in
                     ["Control+Mod1+space", "--release Control+Mod1+space", "Escape",
                      "Control+Mod1+d", "--release Control+Mod1+d"]}
res["after_quit_hud_window_left"] = [w for w in tree_windows() if w["app_id"] == "cantrip"]
wtype("-k", "Escape"); time.sleep(0.3)
res["escape_reaches_receiver_after_quit"] = sum(1 for e in events() if e == ["key", "Escape"]) >= 2
res["cantrip_log_tail"] = [l.strip() for l in open(app_log_path) if l.startswith("[cantrip]")]
for p in reversed(procs):
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(5)
    except Exception:
        p.kill()
print(json.dumps(res, indent=1))
