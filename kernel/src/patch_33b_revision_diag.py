#!/usr/bin/env python3
"""
patch_33b_revision_diag.py — diagnostic-only, zero functional change.

Prints the actual Limine base revision that was negotiated at boot
(BaseRevision::loaded_revision()), so we can determine definitively
whether HHDM/phys_to_virt() is safe to reuse for MMIO (revision 0)
or whether map_mmio() needs its own dedicated mapping (revision >= 1).

This does NOT implement map_mmio() or touch NVMe/PCI code at all —
it's a one-line print, added and run in isolation before committing
to either design, per the "don't assume phys_to_virt() is valid for
PCI MMIO" instruction.
"""
import sys

MAIN_RS_PATH = "main.rs"

OLD = 'limine_boot::assert_base_revision_supported();\n    kprintln!("[boot] Limine base revision supported");'

NEW = '''limine_boot::assert_base_revision_supported();
    kprintln!("[boot] Limine base revision supported");
    kprintln!("[boot-diag] BASE_REVISION.loaded_revision() = {:?}", limine_boot::BASE_REVISION.loaded_revision());'''


def main():
    with open(MAIN_RS_PATH, "r", encoding="utf-8") as f:
        src = f.read()

    count = src.count(OLD)
    if count != 1:
        print(f"[patch_33b] match count: {count}, ABORT: expected exactly 1 match.")
        sys.exit(1)

    src = src.replace(OLD, NEW, 1)
    with open(MAIN_RS_PATH, "w", encoding="utf-8") as f:
        f.write(src)

    print("[patch_33b] patched main.rs with one diagnostic print line.")
    print("[patch_33b] Next: cargo build, then build-iso.sh + QEMU boot, read the [boot-diag] line.")


if __name__ == "__main__":
    main()
