import sys

TARGET = "main.rs"

OLD = '''            crate::kprintln!("[captest] spawned isolated task pid={}, yielding to it...", pid);
            let free_before = memory::pmm::free_frames();
            scheduler::yield_now();
            crate::kprintln!("[captest] kshell resumed after isolated task exited - PASS");'''

NEW = '''            crate::kprintln!("[captest] spawned isolated task pid={}, yielding to it...", pid);
            let free_before = memory::pmm::free_frames();
            // Bounded wait-for-Zombie -- see Part 4 bug #26.
            let sched_ref = scheduler::Scheduler::get();
            let mut became_zombie = sched_ref.is_zombie(pid);
            for _ in 0..10 {
                if became_zombie { break; }
                scheduler::yield_now();
                became_zombie = sched_ref.is_zombie(pid);
            }
            crate::kprintln!("[captest] kshell resumed after isolated task exited - PASS");'''

with open(TARGET) as f:
    content = f.read()

count = content.count(OLD)
print(f"[patch_30] match count in {TARGET}: {count}")
if count != 1:
    print("[patch_30] ABORT: expected exactly 1 match. Nothing written.")
    sys.exit(1)

content = content.replace(OLD, NEW)
with open(TARGET, "w") as f:
    f.write(content)

print("[patch_30] OK: captest now bounded-waits for Zombie before reap()")
