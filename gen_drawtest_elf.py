#!/usr/bin/env python3
"""
gen_drawtest_elf.py -- reads userspace/drawtest (compiled ELF) and appends
a DRAWTEST_ELF byte array to kernel/src/fs/initrd.rs, matching HELLO_ELF's
exact formatting (16 bytes/line, "0x.., " style) so the two arrays are
visually consistent in the file.

Run from ~/nexus-os (so both relative paths resolve).
"""

import sys

ELF_PATH = "userspace/drawtest"
INITRD_PATH = "kernel/src/fs/initrd.rs"

with open(ELF_PATH, "rb") as f:
    data = f.read()

print(f"Read {len(data)} bytes from {ELF_PATH}")

lines = []
for i in range(0, len(data), 16):
    chunk = data[i:i+16]
    hexed = ", ".join(f"0x{b:02x}" for b in chunk)
    lines.append(f"    {hexed},")

array_src = "\n//! Embedded userspace drawtest binary -- calls sys_draw (rax=2) then\n" \
            "//! sys_exit(0). Used by kshell's drawtest command to prove CAP_DRAW\n" \
            "//! is actually enforced, not just defined.\n" \
            "pub static DRAWTEST_ELF: &[u8] = &[\n" + "\n".join(lines) + "\n];\n"

with open(INITRD_PATH) as f:
    existing = f.read()

if "DRAWTEST_ELF" in existing:
    print("NOT WRITTEN - DRAWTEST_ELF already present in initrd.rs")
    sys.exit(1)

with open(INITRD_PATH, "a") as f:
    f.write(array_src)

print(f"APPLIED - appended DRAWTEST_ELF ({len(data)} bytes) to {INITRD_PATH}")
