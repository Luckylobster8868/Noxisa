#!/usr/bin/env python3
"""
patch_33c_fix_revision_diag.py — fixes patch_33b's two build errors:
  1. BASE_REVISION was private in limine_boot.rs -> make it pub
  2. loaded_revision() doesn't exist on limine 0.6.5's BaseRevision;
     the real method (confirmed against the actual installed crate
     source at ~/.cargo/registry/src/.../limine-0.6.5/src/lib.rs) is
     actual_revision() -> Option<u64>.
"""
import sys

LIMINE_BOOT_PATH = "limine_boot.rs"
MAIN_RS_PATH = "main.rs"

LB_OLD = "static BASE_REVISION: BaseRevision = BaseRevision::new();"
LB_NEW = "pub static BASE_REVISION: BaseRevision = BaseRevision::new();"

MAIN_OLD = 'limine_boot::BASE_REVISION.loaded_revision()'
MAIN_NEW = 'limine_boot::BASE_REVISION.actual_revision()'


def patch(path, old, new):
    with open(path, "r", encoding="utf-8") as f:
        src = f.read()
    count = src.count(old)
    if count != 1:
        print(f"[patch_33c] {path} match count: {count}, ABORT: expected exactly 1 match.")
        sys.exit(1)
    src = src.replace(old, new, 1)
    with open(path, "w", encoding="utf-8") as f:
        f.write(src)
    print(f"[patch_33c] patched {path}")


patch(LIMINE_BOOT_PATH, LB_OLD, LB_NEW)
patch(MAIN_RS_PATH, MAIN_OLD, MAIN_NEW)
print("[patch_33c] OK. Next: cargo build again.")
