import sys

TARGET = "main.rs"

OLD = '''                crate::kprintln!("[drawtest] launched pid={}, yielding to it...", pid);
                let free_before = memory::pmm::free_frames();
                scheduler::yield_now();
                crate::kprintln!("[drawtest] kshell resumed after pid={} exited - PASS", pid);'''

NEW = '''                crate::kprintln!("[drawtest] launched pid={}, yielding to it...", pid);
                let free_before = memory::pmm::free_frames();
                // Bounded wait-for-Zombie -- see Part 4 bug #26. This is
                // the exact call site that surfaced the race: a single
                // yield_now() only guarantees SOME switch happened, not
                // that this pid specifically reached Zombie, so an
                // immediate reap() could panic on a merely-preempted task.
                let sched_ref = scheduler::Scheduler::get();
                let mut became_zombie = sched_ref.is_zombie(pid);
                for _ in 0..10 {
                    if became_zombie { break; }
                    scheduler::yield_now();
                    became_zombie = sched_ref.is_zombie(pid);
                }
                crate::kprintln!("[drawtest] kshell resumed after pid={} exited - PASS", pid);'''

with open(TARGET) as f:
    content = f.read()

count = content.count(OLD)
print(f"[patch_31] match count in {TARGET}: {count}")
if count != 1:
    print("[patch_31] ABORT: expected exactly 1 match. Nothing written.")
    sys.exit(1)

content = content.replace(OLD, NEW)
with open(TARGET, "w") as f:
    f.write(content)

print("[patch_31] OK: drawtest now bounded-waits for Zombie before reap()")
