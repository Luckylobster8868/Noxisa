#!/usr/bin/env python3
"""
patch_37b_fix_stale_admin_state.py — fix a real regression found while
regression-testing patch_37.

ROOT CAUSE: nvmeinittest (patch_35) calls init_admin_queues() directly
against a fresh map_mmio() mapping, completely independent of the
ADMIN_QUEUE/IO_QUEUE persistent singletons patch_36/37 introduced.
That call disables and re-enables the real controller and reprograms
AQA/ASQ/ACQ to a new set of physical addresses. If ADMIN_QUEUE (and/or
IO_QUEUE) was already populated from an earlier identifytest/
nvmeiotest run in the same boot, its stored addresses and tail/head/
phase tracking are now stale - the controller no longer recognizes
them as its admin queue. ensure_admin_queue() correctly sees
ADMIN_QUEUE is already Some() and reuses it (working as designed),
but the underlying hardware state has since changed out from under
it, so the next command submitted against the stale state times out.
Reproduced exactly as described: nvmeiotest passed (using the
persistent singletons), then nvmeinittest ran (independently resetting
the controller), then identifytest failed with CommandTimeout twice
in a row - a real, reproducible bug, not a flaky timing issue.

FIX: add nvme::invalidate_persistent_state(), which clears both
ADMIN_QUEUE and IO_QUEUE back to None. Call it from nvmeinittest's
dispatch arm right after its raw init_admin_queues() call, regardless
of Ok/Err (even a failed attempt may have left CC.EN cleared mid-
sequence, so the safe thing is to always force the next
identifytest/nvmeiotest to reinitialize from the controller's actual
current state rather than trust anything cached). This does not
change nvmeinittest's own behavior or purpose (still a standalone,
independent proof that the raw init_admin_queues() function works) -
it only ensures it doesn't leave other commands' cached state
dangling afterward.

Run from kernel/src, same convention as prior patches.
"""
import sys

NVME_RS_PATH = "drivers/nvme.rs"
MAIN_RS_PATH = "main.rs"

# ---------------------------------------------------------------------------
# 1. drivers/nvme.rs — add invalidate_persistent_state()
# ---------------------------------------------------------------------------

NVME_OLD = """/// I/O queue ID used throughout this module. Only one I/O SQ/CQ pair
/// is created (QID 1) - this patch does not implement multiple I/O
/// queues.
const IO_QID: u16 = 1;"""

NVME_NEW = """/// Clear both persistent singletons (ADMIN_QUEUE, IO_QUEUE) back to
/// None, forcing the next call that needs them to fully reinitialize
/// against the controller's actual current state.
///
/// This exists because nvmeinittest (see main.rs) deliberately calls
/// init_admin_queues() directly, independent of ensure_admin_queue(),
/// to prove that raw function works standalone. That call resets the
/// real controller and reprograms its admin queue addresses - if
/// ADMIN_QUEUE/IO_QUEUE were already populated from an earlier
/// identifytest/nvmeiotest call, they would otherwise keep pointing
/// at addresses the controller no longer recognizes, causing the
/// next command to time out. main.rs's nvmeinittest dispatch arm
/// calls this immediately after its raw init_admin_queues() call.
pub fn invalidate_persistent_state() {
    *ADMIN_QUEUE.lock() = None;
    *IO_QUEUE.lock() = None;
}

/// I/O queue ID used throughout this module. Only one I/O SQ/CQ pair
/// is created (QID 1) - this patch does not implement multiple I/O
/// queues.
const IO_QID: u16 = 1;"""

# ---------------------------------------------------------------------------
# 2. main.rs — call invalidate_persistent_state() after nvmeinittest's
#    raw init_admin_queues() call, on both the Ok and Err paths.
# ---------------------------------------------------------------------------

MAIN_OLD = """                    match drivers::nvme::init_admin_queues(&regs, 64) {
                        Ok(aq) => {
                            crate::kprintln!(
                                "[nvmeinittest] admin SQ: phys={:#x} virt={:#x}",
                                aq.sq_phys, aq.sq_virt
                            );
                            crate::kprintln!(
                                "[nvmeinittest] admin CQ: phys={:#x} virt={:#x}",
                                aq.cq_phys, aq.cq_virt
                            );
                            crate::kprintln!("[nvmeinittest] depth={}", aq.depth);
                            crate::kprintln!("[nvmeinittest] CSTS after init = {:#010x}", regs.status_raw());
                            crate::kprintln!("[nvmeinittest] PASS - controller cycled disable->enable and reports RDY=1");
                        }
                        Err(e) => {
                            crate::kprintln!("[nvmeinittest] FAIL - init_admin_queues returned {:?}", e);
                        }
                    }"""

MAIN_NEW = """                    match drivers::nvme::init_admin_queues(&regs, 64) {
                        Ok(aq) => {
                            crate::kprintln!(
                                "[nvmeinittest] admin SQ: phys={:#x} virt={:#x}",
                                aq.sq_phys, aq.sq_virt
                            );
                            crate::kprintln!(
                                "[nvmeinittest] admin CQ: phys={:#x} virt={:#x}",
                                aq.cq_phys, aq.cq_virt
                            );
                            crate::kprintln!("[nvmeinittest] depth={}", aq.depth);
                            crate::kprintln!("[nvmeinittest] CSTS after init = {:#010x}", regs.status_raw());
                            crate::kprintln!("[nvmeinittest] PASS - controller cycled disable->enable and reports RDY=1");
                        }
                        Err(e) => {
                            crate::kprintln!("[nvmeinittest] FAIL - init_admin_queues returned {:?}", e);
                        }
                    }
                    // This call reset the real controller independent of
                    // ADMIN_QUEUE/IO_QUEUE - drop any cached state so the
                    // next identifytest/nvmeiotest reinitializes fresh
                    // instead of using now-stale addresses (see patch_37b).
                    drivers::nvme::invalidate_persistent_state();"""


def read(path):
    with open(path, "r", encoding="utf-8") as f:
        return f.read()


def write(path, content):
    with open(path, "w", encoding="utf-8") as f:
        f.write(content)


def patch_inplace(path, old, new, label):
    src = read(path)
    count = src.count(old)
    if count != 1:
        print(f"[patch_37b] {label} match count in {path}: {count}, ABORT: expected exactly 1 match.")
        sys.exit(1)
    write(path, src.replace(old, new, 1))
    print(f"[patch_37b] patched {path} ({label})")


def main():
    patch_inplace(NVME_RS_PATH, NVME_OLD, NVME_NEW, "invalidate_persistent_state()")
    patch_inplace(MAIN_RS_PATH, MAIN_OLD, MAIN_NEW, "nvmeinittest calls invalidate_persistent_state()")

    print("[patch_37b] OK — stale-state bug fixed.")
    print("[patch_37b] Next: cargo build, then re-run the full regression sweep (order no longer matters).")


if __name__ == "__main__":
    main()
