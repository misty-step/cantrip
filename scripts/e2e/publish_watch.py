#!/usr/bin/env python3
"""Watch a Meeting folder during publication (real-path atomicity check).

    publish_watch.py FOLDER LOG STOPFILE

Until STOPFILE exists, scans FOLDER recursively in a tight loop *and* reacts to
inotify events (IN_CREATE, IN_MODIFY, IN_MOVED_TO, IN_CLOSE_WRITE, plus new
subdirectories), recording every regular file, symlink or other entry it sees
as one JSON line per distinct (path, kind, size, sha256) observation with the
first time it was seen. Contents are hashed, never logged. Writes a summary
line {"scans": n, "events": m} at the end.
"""
import ctypes, hashlib, json, os, select, struct, sys, time

folder, log, stopfile = sys.argv[1], sys.argv[2], sys.argv[3]
libc = ctypes.CDLL("libc.so.6", use_errno=True)
IN_MODIFY, IN_CLOSE_WRITE, IN_MOVED_TO, IN_CREATE = 0x2, 0x8, 0x80, 0x100
IN_NONBLOCK = 0o4000
MASK = IN_MODIFY | IN_CLOSE_WRITE | IN_MOVED_TO | IN_CREATE
ifd = libc.inotify_init1(IN_NONBLOCK)
watched = set()
seen = set()
out = open(log, "w")
scans = events = 0


def add_watches():
    for root, dirs, _ in os.walk(folder):
        if root not in watched:
            libc.inotify_add_watch(ifd, root.encode(), MASK)
            watched.add(root)


def observe(via):
    global scans
    scans += 1
    for root, dirs, files in os.walk(folder):
        for name in files + [d for d in dirs if os.path.islink(os.path.join(root, d))]:
            p = os.path.join(root, name)
            try:
                st = os.lstat(p)
                if os.path.islink(p):
                    kind, size, digest = "link", 0, ""
                elif os.path.isfile(p):
                    with open(p, "rb") as f:
                        data = f.read()
                    kind, size, digest = "file", len(data), hashlib.sha256(data).hexdigest()
                else:
                    kind, size, digest = "special", st.st_size, ""
            except FileNotFoundError:
                continue
            key = (os.path.relpath(p, folder), kind, size, digest)
            if key not in seen:
                seen.add(key)
                out.write(json.dumps({"t": time.time(), "via": via, "path": key[0], "kind": kind,
                                      "size": size, "sha256": digest}) + "\n")
                out.flush()


while not os.path.exists(stopfile):
    add_watches()
    r, _, _ = select.select([ifd], [], [], 0.001)
    if r:
        buf = os.read(ifd, 65536)
        events += len(buf) // struct.calcsize("iIII")  # lower bound
        add_watches()
        observe("inotify")
    else:
        observe("poll")
add_watches()
observe("final")
out.write(json.dumps({"scans": scans, "events": events}) + "\n")
out.close()
