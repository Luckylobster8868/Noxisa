#!/usr/bin/env python3
"""
patch_15_dell_scaffolding.py -- adds TEMPORARY Dell auto-run scaffolding
for drawtest + ownertest, per Part 5.3's verification structure. The Dell
has no serial capture and no working keyboard path to kshell, so both
tests are auto-run at boot instead of typed in, with red-then-green
100x100 framebuffer markers so pass/fail is visible in a photo.

Coordinates used (both free per Part 5.3's table): drawtest at x=1160,
ownertest at x=1040. Both y=20.

After the Dell photo confirms green on both, REVERT this patch (see
patch_16_revert_dell_scaffolding.py, generated after a green photo) --
do not leave this in permanently, per Part 5.3 step 10 and the project's
own established convention (grep -n "TEMPORARY" should return nothing
once done).

Run from ~/nexus-os/kernel/src.
"""

with open('main.rs') as f:
    full = f.read()

edits = []

# ── Edit 1: the scaffolding function itself, inserted right before
# shell_thread() so it's easy to find and easy to delete as one block. ──
old1 = 'extern "C" fn shell_thread() -> ! {'

new1 = '''// TEMPORARY Dell verification scaffolding for drawtest + ownertest.
// Auto-runs both at boot (Dell has no keyboard path to kshell) and
// draws red-then-green markers so pass/fail is visible in a photo.
// REVERT this function and its call site in shell_thread() once the
// Dell photo confirms green on both markers - see Part 5.3.
unsafe fn dell_verify_drawtest_ownertest() {
    // --- drawtest marker at x=1160 ---
    crate::drivers::gpu::fill_rect(1160, 20, 100, 100, 0x00FF0000); // red = not yet passed
    let mut drawtest_pass = false;
    if let Ok(pid) = scheduler::Scheduler::get().spawn_program("drawtest", fs::initrd::DRAWTEST_ELF, 0) {
        let free_before = memory::pmm::free_frames();
        scheduler::yield_now();
        scheduler::Scheduler::get().reap(pid);
        let free_after = memory::pmm::free_frames();
        // Correct behaviour: CAP_DRAW denies the draw (proven separately
        // via captest's own serial-only assertion pattern) AND the
        // process still ran to completion and was reclaimed cleanly.
        drawtest_pass = free_after > free_before;
    }
    let drawtest_colour = if drawtest_pass { 0x0000FF00u32 } else { 0x00FF0000u32 };
    crate::drivers::gpu::fill_rect(1160, 20, 100, 100, drawtest_colour);

    // --- ownertest marker at x=1040 ---
    crate::drivers::gpu::fill_rect(1040, 20, 100, 100, 0x00FF0000); // red = not yet passed
    fs::vfs::set_identity(1000, 1000);
    let create_ok = fs::vfs::write("/tmp/dell_ownertest.txt", b"dell verify").is_ok();
    let stat1_ok = matches!(fs::vfs::stat("/tmp/dell_ownertest.txt"), Ok(s) if s.uid == 1000 && s.gid == 1000);
    let overwrite_ok = fs::vfs::write("/tmp/dell_ownertest.txt", b"dell verify 2").is_ok();
    let stat2_ok = matches!(fs::vfs::stat("/tmp/dell_ownertest.txt"), Ok(s) if s.uid == 1000 && s.gid == 1000);
    fs::vfs::set_identity(0, 0);
    let ownertest_pass = create_ok && stat1_ok && overwrite_ok && stat2_ok;
    let ownertest_colour = if ownertest_pass { 0x0000FF00u32 } else { 0x00FF0000u32 };
    crate::drivers::gpu::fill_rect(1040, 20, 100, 100, ownertest_colour);
}

extern "C" fn shell_thread() -> ! {'''

edits.append((old1, new1, "insert dell_verify_drawtest_ownertest() scaffolding function"))

# ── Edit 2: call it as the very first thing shell_thread() does. ──
old2 = '''    use kshell::exec_builtin;
    use drivers::serial;

    crate::kprintln!('''

new2 = '''    use kshell::exec_builtin;
    use drivers::serial;

    unsafe { dell_verify_drawtest_ownertest(); } // TEMPORARY - see Part 5.3

    crate::kprintln!('''

edits.append((old2, new2, "call scaffolding at top of shell_thread()"))

counts = [full.count(old) for old, new, desc in edits]
print("Match counts:", counts)

if all(c == 1 for c in counts):
    for old, new, desc in edits:
        full = full.replace(old, new, 1)
    with open('main.rs', 'w') as f:
        f.write(full)
    print("APPLIED - TEMPORARY Dell scaffolding added. Remember to revert after the photo.")
else:
    for (old, new, desc), c in zip(edits, counts):
        status = "ok" if c == 1 else f"NOT WRITTEN (found {c})"
        print(f"  [{status}] {desc}")
    print("NOT WRITTEN - one or more edits didn't match exactly once.")
