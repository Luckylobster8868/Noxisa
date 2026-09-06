import sys
TARGET = "main.rs"
OLD = """pub mod hal;        // Hardware Abstraction Layer — device traits + registry
pub mod net;        // Full network stack (TCP/IP, DNS, TLS, firewall)
pub mod heal;       // Self-healing engine — auto problem detection + remedy
pub mod watchdog;   // Fault tolerance: watchdog timer, deadlock detection, tracing
pub mod perf;       // Performance monitor: PMU counters, NUMA, RDTSC
pub mod privacy;    // Federated Learning + Differential Privacy (FL+DP)
pub mod compat;     // Linux binary compatibility (syscall translation)
pub mod syscall;    // Native syscall dispatch + AI isolation boundary
pub mod signal;     // POSIX signals: SIGKILL, SIGTERM, SIGCHLD, etc.
pub mod io_uring;   // Async I/O ring buffers (io_uring)
pub mod fuzz;       // In-kernel syscall fuzzer (DISABLED in production)     // Linux binary compatibility (syscall translation)
"""
with open(TARGET) as f:
    content = f.read()
count = content.count(OLD)
print(f"[patch_17] match count in {TARGET}: {count}")
if count != 1:
    print("[patch_17] ABORT: expected exactly 1 match. Nothing written.")
    sys.exit(1)
content = content.replace(OLD, "")
with open(TARGET, "w") as f:
    f.write(content)
print("[patch_17] OK: removed 11 dead pub mod declarations from main.rs")
