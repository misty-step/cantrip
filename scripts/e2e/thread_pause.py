#!/usr/bin/env python3
"""Pause ONE thread of a running process (not the whole process): argv: pid
thread-name. `SIGSTOP`, even sent with `tgkill` to a single thread, stops
the entire thread group; ptrace is per thread, so `PTRACE_SEIZE` +
`PTRACE_INTERRUPT` on the thread's TID stops only that thread while every
other thread keeps running. Needs ptrace rights over the process (with Yama
`ptrace_scope=1`, run it with sudo).

Prints `paused <tid>` once the thread is in a ptrace stop, then keeps it
stopped until stdin closes (prints `resumed`) or the process exits (prints
`exited`)."""
import ctypes, os, select, sys, time

PTRACE_DETACH, PTRACE_SEIZE, PTRACE_INTERRUPT = 17, 0x4206, 0x4207
WALL = 0x40000000
libc = ctypes.CDLL(None, use_errno=True)
libc.ptrace.argtypes = [ctypes.c_long, ctypes.c_long, ctypes.c_void_p, ctypes.c_void_p]
pid, name = int(sys.argv[1]), sys.argv[2]
tid = next(int(t) for t in os.listdir(f"/proc/{pid}/task")
           if open(f"/proc/{pid}/task/{t}/comm").read().strip() == name[:15])


def state():
    s = open(f"/proc/{pid}/task/{tid}/stat").read()
    return s[s.rindex(")") + 2]


def call(req):
    if libc.ptrace(req, tid, None, None) != 0:
        raise SystemExit(f"ptrace {req}: {os.strerror(ctypes.get_errno())}")


call(PTRACE_SEIZE)
call(PTRACE_INTERRUPT)
os.waitpid(tid, WALL)
print(f"paused {tid} state={state()}", flush=True)
while True:
    r, _, _ = select.select([sys.stdin], [], [], 0.1)
    if r and not sys.stdin.readline():
        try:
            call(PTRACE_DETACH)
            print("resumed", flush=True)
        except SystemExit:
            print("exited", flush=True)
        break
    if not os.path.exists(f"/proc/{pid}/task/{tid}"):
        print("exited", flush=True)
        break
    # Reap the tracee's exit notification, if any, without blocking.
    try:
        os.waitpid(tid, WALL | os.WNOHANG)
    except ChildProcessError:
        print("exited", flush=True)
        break
