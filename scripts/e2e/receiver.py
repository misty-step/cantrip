#!/usr/bin/env python3
"""Receiver A/B for delivery evidence: a GTK3 window with one text entry that
logs every change of its value (length + text) to a log file. With
RECEIVER_EVENTS=<file>, every key press (key name) and pointer press
(window coordinates) it receives is logged there too: proof of what reached
the focused app (e.g. that a cancelling Escape never did). With
RECEIVER_COUNTER=<marker dir>, a large on-screen panel shows how many Escape
key presses the entry received and how many lines each user Sway binding
appended to <marker dir>/escape and <marker dir>/chord, plus the text of
<marker dir>/caption (the driving script's current step), for screenshots."""
import os, sys, gi, time
gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Gdk, Gtk
name, log = sys.argv[1], sys.argv[2]
win = Gtk.Window(title=f"Receiver {name}")
win.set_default_size(900, 80)
entry = Gtk.Entry()
counter_dir = os.environ.get("RECEIVER_COUNTER")
if counter_dir:
    from gi.repository import GLib
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
    box.set_border_width(24)
    box.pack_start(entry, False, False, 0)
    panel = Gtk.Label(xalign=0)
    box.pack_start(panel, False, False, 0)
    win.add(box)
    escapes = [0]

    def lines(name):
        try:
            with open(os.path.join(counter_dir, name)) as f:
                return sum(1 for _ in f)
        except OSError:
            return 0

    def caption():
        try:
            with open(os.path.join(counter_dir, "caption")) as f:
                return f.read().strip()
        except OSError:
            return ""

    def refresh():
        panel.set_markup(
            f"<span size='xx-large'><b>Receiver {name}</b> (GTK3, focused)\n"
            f"Escape keys received by this app: <b>{escapes[0]}</b>\n"
            f"User Sway Escape binding fired: <b>{lines('escape')}</b>\n"
            f"User Sway chord binding fired: <b>{lines('chord')}</b></span>\n\n"
            f"<span size='large'>{GLib.markup_escape_text(caption())}</span>")
        return True

    def count_escape(_w, e):
        if Gdk.keyval_name(e.keyval) == "Escape":
            escapes[0] += 1
            refresh()
        return False

    entry.connect("key-press-event", count_escape)
    refresh()
    GLib.timeout_add(100, refresh)
else:
    win.add(entry)
def changed(e):
    with open(log, "a") as f:
        f.write(f"{time.time():.3f}\t{name}\t{e.get_text()}\n")
entry.connect("changed", changed)
events = os.environ.get("RECEIVER_EVENTS")
if events:
    def log_event(kind, detail):
        with open(events, "a") as f:
            f.write(f"{time.time():.3f}\t{name}\t{kind}\t{detail}\n")
        return False  # let the entry handle it as usual
    entry.connect("key-press-event", lambda w, e: log_event("key", Gdk.keyval_name(e.keyval)))
    entry.connect("button-press-event", lambda w, e: log_event("button", f"{e.x:.0f},{e.y:.0f}"))
win.connect("destroy", Gtk.main_quit)
win.show_all()
Gtk.main()
