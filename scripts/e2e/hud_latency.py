#!/usr/bin/env python3
"""Press the real shortcut (XTEST via xdotool) N times; measure press → HUD
on-screen, verify tap keeps listening, Escape cancels and parks the HUD.
Uses python-xlib-free polling through xdotool (upper-bound timings)."""
import subprocess, time, json, os, sys
def sh(*a): return subprocess.run(a, capture_output=True, text=True).stdout
hud = sh('xdotool','search','--name','^Cantrip HUD$').split()[0]
def hud_x():
    g = sh('xdotool','getwindowgeometry',hud)
    return int(g.split('Position: ')[1].split(',')[0])
out = []
for i in range(int(sys.argv[1])):
    t0 = time.monotonic()
    subprocess.run(['xdotool','keydown','ctrl+alt+space'])
    seen = None
    while time.monotonic() - t0 < 1.0:
        if hud_x() > -1000: seen = time.monotonic(); break
    subprocess.run(['xdotool','keyup','space']); subprocess.run(['xdotool','keyup','alt','ctrl'])
    time.sleep(0.3)
    still = hud_x() > -1000
    t1 = time.monotonic(); subprocess.run(['xdotool','key','Escape'])
    gone = None
    while time.monotonic() - t1 < 1.0:
        if hud_x() < -1000: gone = time.monotonic(); break
    d = os.path.expanduser('~/.local/share/cantrip/v2/dictations')
    deleted = None
    while time.monotonic() - t1 < 3.0:
        if not os.listdir(d): deleted = time.monotonic(); break
        time.sleep(0.005)
    staged = os.listdir(d)
    out.append({"press_to_hud_ms": round((seen - t0)*1000) if seen else None,
                "tap_keeps_listening": still,
                "escape_to_hud_hidden_ms": round((gone - t1)*1000) if gone else None,
                "escape_to_audio_deleted_ms": round((deleted - t1)*1000) if deleted else None,
                "staged_items_after_escape": len(staged)})
    time.sleep(0.5)
print(json.dumps(out))
