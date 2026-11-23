#!/usr/bin/env python3
"""Receiver A/B for delivery evidence: a GTK3 window with one text entry that
logs every change of its value (length + text) to a log file."""
import sys, gi, time
gi.require_version("Gtk", "3.0")
from gi.repository import Gtk
name, log = sys.argv[1], sys.argv[2]
win = Gtk.Window(title=f"Receiver {name}")
win.set_default_size(900, 80)
entry = Gtk.Entry()
win.add(entry)
def changed(e):
    with open(log, "a") as f:
        f.write(f"{time.time():.3f}\t{name}\t{e.get_text()}\n")
entry.connect("changed", changed)
win.connect("destroy", Gtk.main_quit)
win.show_all()
Gtk.main()
