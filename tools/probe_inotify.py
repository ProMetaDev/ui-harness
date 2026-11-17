#!/usr/bin/env python3
"""探测 inotify 可用性与上限（不依赖任何库）。"""
import ctypes
import ctypes.util
import os

libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)

SYS_INOTIFY_INIT1 = 294
IN_NONBLOCK = 0o4000
IN_CLOEXEC = 0o2000000

fd = libc.syscall(SYS_INOTIFY_INIT1, ctypes.c_int(IN_NONBLOCK | IN_CLOEXEC))
if fd < 0:
    errno = ctypes.get_errno()
    print(f"inotify: 不可用（{os.strerror(errno)}）")
    raise SystemExit(1)
print(f"inotify: 可用（fd={fd}）")
os.close(fd)

for p in ("/proc/sys/fs/inotify/max_user_watches", "/proc/sys/fs/inotify/max_queued_events"):
    try:
        print(f"{p} = {open(p).read().strip()}")
    except OSError as e:
        print(f"{p}: {e}")
