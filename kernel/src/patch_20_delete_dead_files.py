import shutil, os, sys
DIRS = ["hal", "net", "heal", "watchdog", "perf", "privacy",
        "compat", "signal", "io_uring", "fuzz", "syscall"]
FILES = ["drivers/amdgpu.rs", "drivers/intel_xe.rs", "drivers/ax201.rs",
         "drivers/wifi.rs", "drivers/rtl8139.rs", "drivers/nvme.rs",
         "drivers/pci.rs", "drivers/font.rs"]
missing = [d for d in DIRS if not os.path.isdir(d)] + \
          [f for f in FILES if not os.path.isfile(f)]
if missing:
    print("[patch_20] ABORT: expected targets not found:")
    for m in missing:
        print(f"             - {m}")
    sys.exit(1)
deleted = []
for d in DIRS:
    shutil.rmtree(d)
    deleted.append(d + "/")
for fpath in FILES:
    os.remove(fpath)
    deleted.append(fpath)
print(f"[patch_20] OK: deleted {len(deleted)} targets:")
for d in deleted:
    print(f"             - {d}")
