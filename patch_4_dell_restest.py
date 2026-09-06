#!/usr/bin/env python3
import sys

PATH = "kernel/src/main.rs"

old = '''fn shell_thread() -> ! {'''
new = '''fn shell_thread() -> ! {
    // TEMPORARY - Dell verification for restest (Part 6 item 1). REVERT once confirmed on the Dell.
    crate::drivers::gpu::fill_rect(1160, 20, 100, 100, 0x00FF0000); // red default
    shell_cmd(&alloc::string::String::from("restest"));
    crate::drivers::gpu::fill_rect(1160, 20, 100, 100, 0x0000FF00); // green - completed
'''

with open(PATH, "r") as f:
    content = f.read()

count = content.count(old)
if count != 1:
    print(f"ABORT: expected exactly 1 match, found {count}")
    sys.exit(1)

content = content.replace(old, new, 1)

with open(PATH, "w") as f:
    f.write(content)

print("patch_4_dell_restest.py: applied 1/1 edit to", PATH)
