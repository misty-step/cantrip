"""Print the Cantrip HUD's accessible name via AT-SPI (AccessKit), or nothing."""
import gi; gi.require_version("Atspi", "2.0"); from gi.repository import Atspi
KEYS = ("Copied", "Raw", "Posted", "Inserted", "Listening", "Transcribing", "Sent", "Not delivered")
d = Atspi.get_desktop(0)
for i in range(d.get_child_count()):
    a = d.get_child_at_index(i)
    if a and (a.get_name() or "").lower().startswith("cantrip"):
        stack = [a]
        while stack:
            n = stack.pop()
            nm = n.get_name() or ""
            if any(k in nm for k in KEYS):
                print(nm); raise SystemExit
            stack.extend(c for c in (n.get_child_at_index(j) for j in range(min(n.get_child_count(), 50))) if c)
