import sys

TARGET = "main.rs"

OLD = '''            crate::kprintln!("[isotest] spawned isolated task pid={}, yielding to it...", pid);
            let free_before = memory::pmm::free_frames();
            scheduler::yield_now();
            crate::kprintln!("[isotest] kshell resumed after isolated task exited - PASS");'''

NEW = '''            crate::kprintln!("[isotest] spawned isolated task pid={}, yielding to it...", pid);
            let free_before = memory::pmm::free_frames();
            // Bounded wait-for-Zombie, same pattern multitest/conctest/
            // preempttest already established -- a single yield_now() only
            // guarantees SOME switch happened, not that this specific task
            // reached Zombie. With real preemption, switch_to() re-enqueues
            // the outgoing task (kshell) immediately, so a timer tick can
            // bounce control back here before the spawned task finishes,
            // and reap() would then panic on a still-live task (see Part 4
            // bug #26 - drawtest hit this intermittently).
            let sched_ref = scheduler::Scheduler::get();
            let mut became_zombie = sched_ref.is_zombie(pid);
            for _ in 0..10 {
                if became_zombie { break; }
                scheduler::yield_now();
                became_zombie = sched_ref.is_zombie(pid);
            }
            crate::kprintln!("[isotest] kshell resumed after isolated task exited - PASS");'''

with open(TARGET) as f:
    content = f.read()

count = content.count(OLD)
print(f"[patch_28] match count in {TARGET}: {count}")
if count != 1:
    print("[patch_28] ABORT: expected exactly 1 match. Nothing written.")
    sys.exit(1)

content = content.replace(OLD, NEW)
with open(TARGET, "w") as f:
    f.write(content)

print("[patch_28] OK: isotest now bounded-waits for Zombie before reap()")
