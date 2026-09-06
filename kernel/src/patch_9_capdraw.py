#!/usr/bin/env python3
"""
patch_9_capdraw.py -- closes the "CAP_DRAW / sys_draw is defined but not
wired to an actual check" weakness from NOXISA_COMPLETE.pdf Part 2.

Run from ~/nexus-os/kernel/src.
"""

with open('main.rs') as f:
    full = f.read()

edits = []

old1 = '''extern "C" fn draw_success_marker() {
    crate::drivers::gpu::fill_rect(20, 20, 100, 100, 0x00FF00);
}'''

new1 = '''/// Capability-checked entry point for sys_draw (rax=2). Previously
/// draw_success_marker() was called directly from syscall_handler with
/// no gate at all -- CAP_DRAW existed as a bit but nothing ever tested
/// it, so any process could draw regardless of its capability mask.
/// Mirrors checked_sys_write's chokepoint pattern exactly (single check,
/// same audit log wiring) rather than inventing a second convention.
extern "C" fn checked_sys_draw() {
    if scheduler::Scheduler::get().caps() & CAP_DRAW == 0 {
        crate::kprintln!("[cap] sys_draw DENIED - process lacks CAP_DRAW");
        audit::audit_log("cap", "draw", fs::vfs::current_uid(), false, alloc::string::String::from("CAP_DRAW"));
        return;
    }
    audit::audit_log("cap", "draw", fs::vfs::current_uid(), true, alloc::string::String::from("CAP_DRAW"));
    draw_success_marker();
}

extern "C" fn draw_success_marker() {
    crate::drivers::gpu::fill_rect(20, 20, 100, 100, 0x00FF00);
}'''

edits.append((old1, new1, "insert checked_sys_draw() capability gate"))

old2 = "        draw_fn  = sym draw_success_marker,"
new2 = "        draw_fn  = sym checked_sys_draw,"

edits.append((old2, new2, "repoint draw_fn symbol to checked_sys_draw"))

counts = [full.count(old) for old, new, desc in edits]
print("Match counts:", counts)

if all(c == 1 for c in counts):
    for old, new, desc in edits:
        full = full.replace(old, new, 1)
    with open('main.rs', 'w') as f:
        f.write(full)
    print("APPLIED - CAP_DRAW is now enforced at the sys_draw chokepoint")
else:
    for (old, new, desc), c in zip(edits, counts):
        status = "ok" if c == 1 else f"NOT WRITTEN (found {c})"
        print(f"  [{status}] {desc}")
    print("NOT WRITTEN - one or more edits didn't match exactly once.")
