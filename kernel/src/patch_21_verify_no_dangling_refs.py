import subprocess, sys
PATTERNS = ["hal::", "net::", "heal::", "watchdog::", "perf::", "privacy::",
            "compat::", "syscall::", "signal::", "io_uring::", "fuzz::",
            "drivers::pci", "drivers::nvme", "drivers::wifi", "drivers::rtl8139",
            "drivers::amdgpu", "drivers::intel_xe", "drivers::ax201", "drivers::font"]
result = subprocess.run(["grep", "-rn", "-E", "|".join(PATTERNS), "--include=*.rs", "."],
                         capture_output=True, text=True)
if result.stdout.strip():
    print("[patch_21] FAIL: dangling references found — do not build yet:")
    print(result.stdout)
    sys.exit(1)
print("[patch_21] OK: no dangling references to any removed module. Safe to build.")
