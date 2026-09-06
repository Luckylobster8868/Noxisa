#!/usr/bin/env python3
import sys

PATH = "kernel/src/main.rs"

edits = [
(
    'crate::kprintln!("[conctest] IPC A->B delivered: {} ({} bytes, expect true / 20 - \\"hello from worker A\\")", delivered, ipc_bytes);',
    'crate::kprintln!("[conctest] IPC A->B delivered: {} ({} bytes, expect true / 19 - \\"hello from worker A\\")", delivered, ipc_bytes);'
),
(
    'let pass = final_a == 5 && final_b == 5 && final_c == 5 && delivered && ipc_bytes == 20 && c_denied;',
    'let pass = final_a == 5 && final_b == 5 && final_c == 5 && delivered && ipc_bytes == 19 && c_denied;'
),
]

with open(PATH, "r") as f:
    content = f.read()

for old, new in edits:
    count = content.count(old)
    if count != 1:
        print(f"ABORT: expected exactly 1 match, found {count} for a block starting: {old[:60]!r}")
        sys.exit(1)
    content = content.replace(old, new)

with open(PATH, "w") as f:
    f.write(content)

print("patch_7_fix_conctest_bytecount.py: applied 2/2 edits to", PATH)
