#!/usr/bin/env python3
"""探测 Landlock 支持：不装任何东西，直接用 ctypes 调 syscall。

syscall 号（x86_64）：create_ruleset=444, add_rule=445, restrict_self=446
create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION=1) 返回 ABI 版本。
"""
import ctypes
import ctypes.util
import os
import platform

libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)

SYS_CREATE_RULESET = 444
VERSION_FLAG = 1 << 0

print("kernel:", platform.release(), "arch:", platform.machine())

rc = libc.syscall(SYS_CREATE_RULESET, None, ctypes.c_size_t(0), ctypes.c_uint32(VERSION_FLAG))
if rc < 0:
    errno = ctypes.get_errno()
    print(f"landlock: 不可用（rc={rc}, errno={errno} {os.strerror(errno)}）")
    if errno == 38:
        print("  → 内核没有这个 syscall（< 5.13）")
    elif errno == 95:
        print("  → 内核编译时没启用 Landlock，或启动参数关了")
    elif errno == 22:
        print("  → 参数不被接受（可能是 ABI 探测方式问题）")
else:
    print(f"landlock: 可用，ABI 版本 = {rc}")

# ABI 能力说明
if rc >= 1:
    print("  ABI>=1: 文件系统读写限制")
if rc >= 2:
    print("  ABI>=2: 可以限制 refer（跨目录 rename/link）")
if rc >= 3:
    print("  ABI>=3: 可以限制 truncate")
if rc >= 4:
    print("  ABI>=4: 可以限制 TCP bind/connect（网络）")
if rc >= 5:
    print("  ABI>=5: 可以限制 ioctl on device")
if rc >= 6:
    print("  ABI>=6: 可以限制抽象 unix socket 与信号")
