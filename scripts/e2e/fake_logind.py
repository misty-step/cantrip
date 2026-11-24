#!/usr/bin/env python3
"""Fake systemd-logind on a private bus (test only): Inhibit(ssss)->h hands out
a pipe end and logs when the holder closes it; SIGUSR1 emits
PrepareForSleep(true). Logs JSON lines to argv[1]."""
import sys, os, json, time, signal, threading
import dbus, dbus.service, dbus.mainloop.glib
from gi.repository import GLib
log = sys.argv[1]
def note(**k):
    k["t"] = round(time.time(), 3)
    with open(log, "a") as f: f.write(json.dumps(k) + "\n")
dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
bus = dbus.bus.BusConnection(os.environ["FAKE_BUS"])
name = dbus.service.BusName("org.freedesktop.login1", bus)
class Manager(dbus.service.Object):
    @dbus.service.method("org.freedesktop.login1.Manager", in_signature="ssss", out_signature="h")
    def Inhibit(self, what, who, why, mode):
        r, w = os.pipe()
        note(ev="inhibit", what=str(what), who=str(who), why=str(why), mode=str(mode))
        def watch(fd=r, what=str(what), mode=str(mode)):
            os.read(fd, 1)  # EOF when every copy of the write end is closed
            note(ev="released", what=what, mode=mode)
        threading.Thread(target=watch, daemon=True).start()
        fd = dbus.types.UnixFd(w); os.close(w)
        return fd
    @dbus.service.signal("org.freedesktop.login1.Manager", signature="b")
    def PrepareForSleep(self, start): pass
m = Manager(bus, "/org/freedesktop/login1")
def usr1(*_):
    note(ev="PrepareForSleep", start=True); m.PrepareForSleep(True); return True
GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signal.SIGUSR1, usr1)
note(ev="ready")
GLib.MainLoop().run()
