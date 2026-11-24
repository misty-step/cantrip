#!/usr/bin/env python3
"""Receiver A/B for delivery evidence: a GTK3 window with one text entry that
logs every change of its value (length + text) to a log file. With
RECEIVER_EVENTS=<file>, every key press (key name) and pointer press
(window coordinates) it receives is logged there too: proof of what reached
the focused app (e.g. that a cancelling Escape never did)."""
import os, sys, gi, time
gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Gdk, Gtk
name, log = sys.argv[1], sys.argv[2]
win = Gtk.Window(title=f"Receiver {name}")
win.set_default_size(900, 80)
entry = Gtk.Entry()
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
