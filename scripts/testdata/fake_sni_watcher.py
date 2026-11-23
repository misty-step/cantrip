"""Minimal org.kde.StatusNotifierWatcher for tray tests on a private bus."""
import sys
from gi.repository import Gio, GLib

XML = """<node><interface name="org.kde.StatusNotifierWatcher">
<method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
<method name="RegisterStatusNotifierHost"><arg type="s" direction="in"/></method>
<property name="RegisteredStatusNotifierItems" type="as" access="read"/>
<property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
<property name="ProtocolVersion" type="i" access="read"/>
<signal name="StatusNotifierItemRegistered"><arg type="s"/></signal>
</interface></node>"""
items = []

def call(conn, sender, path, iface, method, params, inv):
    if method == "RegisterStatusNotifierItem":
        svc = params.unpack()[0]
        items.append(svc if svc.startswith(":") or "." in svc else sender)
        print("registered", sender, svc, flush=True)
    inv.return_value(None)

def get(conn, sender, path, iface, prop):
    return {"RegisteredStatusNotifierItems": GLib.Variant("as", items),
            "IsStatusNotifierHostRegistered": GLib.Variant("b", True),
            "ProtocolVersion": GLib.Variant("i", 0)}[prop]

def acquired(conn, name):
    node = Gio.DBusNodeInfo.new_for_xml(XML)
    conn.register_object("/StatusNotifierWatcher", node.interfaces[0], call, get, None)

def owned(conn, name):
    print("ready", flush=True)

Gio.bus_own_name(Gio.BusType.SESSION, "org.kde.StatusNotifierWatcher",
                 Gio.BusNameOwnerFlags.NONE, acquired, owned, lambda *a: sys.exit(1))
GLib.MainLoop().run()
