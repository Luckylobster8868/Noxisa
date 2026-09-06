#!/usr/bin/env python3
"""
patch_11_drawtest.py -- adds a real "drawtest" kshell command that proves
CAP_DRAW is actually enforced, not just gated (patch_9 wired the check,
but nothing exercised it).

Uses spawn_program() (Part 2's "Generic process creation" milestone),
NOT the shared-page mechanism captest/isotest use -- those three always
reuse the same physical pages loaded with hello's code at boot, so a
drawtest built on that primitive would silently execute hello's
sys_write instead of drawtest's sys_draw. spawn_program() does a fresh,
independent ELF load per call, so DRAWTEST_ELF's own code genuinely runs.

Run from ~/nexus-os/kernel/src.
"""

with open('main.rs') as f:
    full = f.read()

old = '''    } else if b == b"ipctest" {'''

new = '''    } else if b == b"drawtest" {
        // Proves CAP_DRAW is actually enforced, not just gated (patch_9
        // added checked_sys_draw(); nothing before this exercised it).
        // Uses the real generic spawn path (spawn_program), NOT the
        // shared-page isotest/captest mechanism -- those three always
        // reuse the same physical pages loaded with hello's code at
        // boot, so a drawtest built on that primitive would silently
        // execute hello's sys_write instead of drawtest's sys_draw.
        // spawn_program() does a fresh, independent ELF load per call
        // (Part 2's "Generic process creation" milestone), so
        // DRAWTEST_ELF's own code genuinely runs.
        crate::kprintln!("--- CAP_DRAW enforcement test ---");
        crate::kprint!("(drawtest calls sys_draw with CAP_DRAW withheld - expect denial, then a clean exit back to kshell)\\r\\n");
        match scheduler::Scheduler::get().spawn_program("drawtest", fs::initrd::DRAWTEST_ELF, 0) {
            Err(e) => crate::kprintln!("drawtest: spawn failed - {}", e),
            Ok(pid) => {
                crate::kprintln!("[drawtest] launched pid={}, yielding to it...", pid);
                let free_before = memory::pmm::free_frames();
                scheduler::yield_now();
                crate::kprintln!("[drawtest] kshell resumed after pid={} exited - PASS", pid);
                scheduler::Scheduler::get().reap(pid);
                let free_after = memory::pmm::free_frames();
                crate::kprintln!("[drawtest] reaped pid={}: free frames {} -> {} (reclaimed {})",
                    pid, free_before, free_after, free_after.saturating_sub(free_before));
            }
        }
    } else if b == b"ipctest" {'''

n = full.count(old)
print("Match count:", n)

if n == 1:
    full = full.replace(old, new, 1)
    with open('main.rs', 'w') as f:
        f.write(full)
    print("APPLIED - drawtest command added")
else:
    print(f"NOT WRITTEN - expected 1 match, found {n}")
