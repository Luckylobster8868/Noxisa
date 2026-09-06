import sys
TARGET = "main.rs"
OLD = """// ─── Kernel init extension (added for full feature set) ───────────────────────
// This function is called from kernel_main after the base services are up.
// Separated to keep kernel_main readable.
unsafe fn init_extended_services() {
    // Watchdog MUST be first — catches deadlocks during subsequent init
    watchdog::init();
    crate::kprintln!("[boot] Watchdog ready");

    // Kernel tracing — captures all subsequent events with timestamps
    // tracing::init(); // module pending
    crate::kprintln!("[boot] Tracing ready");

    // Network stack
    net::init();
    crate::kprintln!("[boot] Network stack ready");

    // Power management (ACPI)
    hal::power::init();
    crate::kprintln!("[boot] Power management ready");
}

"""
with open(TARGET) as f:
    content = f.read()
count = content.count(OLD)
print(f"[patch_18] match count in {TARGET}: {count}")
if count != 1:
    print("[patch_18] ABORT: expected exactly 1 match. Nothing written.")
    sys.exit(1)
content = content.replace(OLD, "")
with open(TARGET, "w") as f:
    f.write(content)
print("[patch_18] OK: removed dead init_extended_services() from main.rs")
