#!/usr/bin/env python3
"""
patch_12_fix_drawtest_doccomment.py -- fixes E0753 introduced by
gen_drawtest_elf.py: it used //! (inner doc comment, only legal at the
very top of a file/module) for the DRAWTEST_ELF header, but it's
appended mid-file after HELLO_ELF. Swaps to /// (outer doc comment,
documents the following item, legal anywhere) -- matches how a real
doc comment on a mid-file item should look.

Run from ~/nexus-os/kernel/src/fs.
"""

with open('initrd.rs') as f:
    full = f.read()

old = '''//! Embedded userspace drawtest binary -- calls sys_draw (rax=2) then
//! sys_exit(0). Used by kshell's drawtest command to prove CAP_DRAW
//! is actually enforced, not just defined.
pub static DRAWTEST_ELF: &[u8] = &['''

new = '''/// Embedded userspace drawtest binary -- calls sys_draw (rax=2) then
/// sys_exit(0). Used by kshell's drawtest command to prove CAP_DRAW
/// is actually enforced, not just defined.
pub static DRAWTEST_ELF: &[u8] = &['''

n = full.count(old)
print("Match count:", n)

if n == 1:
    full = full.replace(old, new, 1)
    with open('initrd.rs', 'w') as f:
        f.write(full)
    print("APPLIED - //! swapped to /// on DRAWTEST_ELF's doc comment")
else:
    print(f"NOT WRITTEN - expected 1 match, found {n}")
