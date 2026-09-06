import sys

TARGET = "main.rs"

OLD = '''            Ok(pid) => {
                scheduler::yield_now();
                scheduler::Scheduler::get().reap(pid);
                crate::kprintln!("[restest] probe process pid={} spawned+reaped cleanly (own, empty fd table by construction)", pid);
            }'''

NEW = '''            Ok(pid) => {
                // Bounded wait-for-Zombie -- see Part 4 bug #26.
                let sched_ref = scheduler::Scheduler::get();
                let mut became_zombie = sched_ref.is_zombie(pid);
                for _ in 0..10 {
                    if became_zombie { break; }
                    scheduler::yield_now();
                    became_zombie = sched_ref.is_zombie(pid);
                }
                scheduler::Scheduler::get().reap(pid);
                crate::kprintln!("[restest] probe process pid={} spawned+reaped cleanly (own, empty fd table by construction)", pid);
            }'''

with open(TARGET) as f:
    content = f.read()

count = content.count(OLD)
print(f"[patch_32] match count in {TARGET}: {count}")
if count != 1:
    print("[patch_32] ABORT: expected exactly 1 match. Nothing written.")
    sys.exit(1)

content = content.replace(OLD, NEW)
with open(TARGET, "w") as f:
    f.write(content)

print("[patch_32] OK: restest now bounded-waits for Zombie before reap()")
