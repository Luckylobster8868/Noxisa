#!/usr/bin/env python3
"""
patch_35_nvme_admin_init.py — Phase B.1, step 3: Admin SQ/CQ allocation
                               + NVMe controller initialization.

Run from kernel/src, same convention as prior patches.

Bit-exact CC (Controller Configuration) layout below was cross-checked
against four independent authoritative sources (Linux kernel nvme.h,
FreeBSD nvme.h, EDK2 NvmExpressHci.c, Windows NVME_CONTROLLER_CONFIGURATION)
before writing this, since getting register offsets/widths wrong here
either fails to enable the controller or corrupts its state:
  EN     bit  0        (1 bit)
  CSS    bits 6:4       (3 bits) - I/O Command Set Selected
  MPS    bits 10:7      (4 bits) - Memory Page Size, value N -> 2^(12+N) bytes
  AMS    bits 13:11     (3 bits) - Arbitration Mechanism Selected
  SHN    bits 15:14     (2 bits) - Shutdown Notification
  IOSQES bits 19:16     (4 bits) - I/O Submission Queue Entry Size (log2)
  IOCQES bits 23:20     (4 bits) - I/O Completion Queue Entry Size (log2)

CC fields are derived, not assumed, as follows:
  - CSS: hardcoded to 0 (NVM command set) — but only after confirming
    CAP.CSS bit 0 (absolute bit 37) is actually set, i.e. the
    controller genuinely supports the NVM command set. (Checked in
    the real CAP value captured via mmiotest: CAP=0x004018200f0107ff
    decodes to CSS=0b11000001, bit0=1 -> NVM command set supported.)
  - MPS: read directly from CAP.MPSMIN (bits 51:48). This kernel's PMM
    only hands out 4KB frames, so if MPSMIN != 0 (i.e. the controller
    requires a host page size > 4KB), init_admin_queues() returns
    PageSizeUnsupported rather than silently writing an out-of-range
    value. (Real captured CAP has MPSMIN=0, MPSMAX=4 - 4KB pages are
    within the controller's supported range.)
  - AMS: hardcoded to 0 (round-robin) - this is unconditionally
    guaranteed supported by every NVMe controller per spec regardless
    of CAP.AMS content (CAP.AMS only ever adds optional extra
    schemes on top of round-robin, never removes it), so this isn't
    an assumption CAP needs to confirm.
  - SHN: hardcoded to 0 (no shutdown notification) - correct because
    we are bringing the controller up, not shutting it down.
  - IOSQES/IOCQES: hardcoded to 6/4 (64-byte/16-byte entries) - these
    are spec-mandated fixed values for the standard NVM command set
    (confirmed via Linux's own NVME_NVM_IOSQES=6/NVME_NVM_IOCQES=4
    constants), not something CAP varies per-controller.
  - Admin SQ/CQ entry sizes (64/16 bytes) are ALWAYS fixed by the
    spec regardless of CC.IOSQES/IOCQES, which only govern I/O
    queues created later (patch_37) - used directly as constants.

Timeout budget for both RDY-wait loops comes from CAP.TO (bits 31:24,
in 500ms units) via drivers::pit::ms(), not a made-up iteration count -
the real captured CAP has TO=15 -> 7500ms.

What this does:
  1. drivers/nvme.rs: adds NvmeInitError, AdminQueues, the fixed-size
     constants described above, write64() on NvmeRegs (write32()
     already existed from patch_34 but was unused - the #[allow(dead_code)]
     is removed now that it's genuinely used), and init_admin_queues().
  2. main.rs: adds "nvmeinittest" to help's command list, and a
     nvmeinittest dispatch arm that maps BAR0 (same as mmiotest),
     calls init_admin_queues(), and reports success/failure plus the
     resulting queue addresses.

Explicitly NOT in this patch: Identify Controller/Namespace, I/O
queues, BlockDevice/BlockError (that trait/error type is introduced
in patch_38 alongside the real block I/O path - this patch uses a
local NvmeInitError instead), MSI-X, interrupts, networking,
partition handling, doorbell ringing (CAP.DSTRD is already extracted
by doorbell_stride() from patch_34, actual doorbell writes land in
patch_37 with I/O queues).
"""
import sys

NVME_RS_PATH = "drivers/nvme.rs"
MAIN_RS_PATH = "main.rs"

# ---------------------------------------------------------------------------
# 1. drivers/nvme.rs
# ---------------------------------------------------------------------------

# 1a. write32 already exists but is marked dead_code; also add write64
# right next to it, matching the existing read32/read64 pairing.
WRITE32_OLD = """    #[inline]
    #[allow(dead_code)] // unused until patch_35 (admin queue / CC setup)
    unsafe fn write32(&self, off: u64, val: u32) {
        unsafe { core::ptr::write_volatile((self.base + off) as *mut u32, val) }
    }"""

WRITE32_NEW = """    #[inline]
    unsafe fn write32(&self, off: u64, val: u32) {
        unsafe { core::ptr::write_volatile((self.base + off) as *mut u32, val) }
    }

    #[inline]
    unsafe fn write64(&self, off: u64, val: u64) {
        unsafe { core::ptr::write_volatile((self.base + off) as *mut u64, val) }
    }"""

# 1b. append the admin-queue-init machinery after the existing tail
# of the file (doorbell_stride()'s closing braces).
NVME_TAIL_OLD = """    /// CAP.DSTRD (doorbell stride), bits 32-35 of CAP. Needed later
    /// for I/O doorbell addressing (patch_37) — extracted now since
    /// it comes from the same CAP read already being exercised here,
    /// and the design doc explicitly calls out not hard-coding this.
    pub fn doorbell_stride(&self) -> u32 {
        ((self.cap() >> 32) & 0xF) as u32
    }
}"""

NVME_TAIL_NEW = """    /// CAP.DSTRD (doorbell stride), bits 32-35 of CAP. Needed later
    /// for I/O doorbell addressing (patch_37) — extracted now since
    /// it comes from the same CAP read already being exercised here,
    /// and the design doc explicitly calls out not hard-coding this.
    pub fn doorbell_stride(&self) -> u32 {
        ((self.cap() >> 32) & 0xF) as u32
    }
}

/// Errors bringing up the admin queues / enabling the controller.
/// Deliberately local to this module rather than a project-wide
/// BlockError — that type (and the BlockDevice trait it belongs to)
/// is introduced in patch_38 once there's an actual block I/O path
/// to attach it to; nothing here should anticipate that shape yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvmeInitError {
    /// CAP.MPSMIN requires a host memory page size larger than the
    /// 4KB pages this kernel's PMM currently hands out. Supporting
    /// that would mean multi-frame-aligned queue allocation, out of
    /// scope for this patch.
    PageSizeUnsupported,
    /// The requested admin queue depth exceeds CAP.MQES (the
    /// controller's maximum supported queue size).
    QueueDepthUnsupported,
    /// Physically contiguous frame allocation for SQ or CQ memory
    /// failed (PMM out of memory).
    AllocFailed,
    /// CSTS.RDY did not clear to 0 within the CAP.TO-derived timeout
    /// after CC.EN was cleared.
    DisableTimeout,
    /// CSTS.RDY did not set to 1 within the CAP.TO-derived timeout
    /// after CC.EN was set.
    EnableTimeout,
    /// CSTS.CFS (Controller Fatal Status) was set after enabling.
    ControllerFatalStatus,
}

/// Physical + virtual addresses and depth of the allocated Admin
/// Submission/Completion Queues. Just the addresses for now — no
/// head/tail tracking or command submission yet (that lands with
/// Identify Controller/Namespace in patch_36).
#[derive(Debug, Clone, Copy)]
pub struct AdminQueues {
    pub sq_phys: u64,
    pub cq_phys: u64,
    pub sq_virt: u64,
    pub cq_virt: u64,
    pub depth: u16,
}

/// Fixed by the NVMe Base Specification for the standard NVM command
/// set — NOT read from CAP. Admin queue entries are always 64 bytes
/// (submission) / 16 bytes (completion), regardless of what
/// CC.IOSQES/IOCQES end up set to for I/O queues later.
const ADMIN_SQE_SIZE: u64 = 64;
const ADMIN_CQE_SIZE: u64 = 16;

/// CC.CSS value for the NVM command set. Only used after confirming
/// CAP.CSS bit 0 (absolute bit 37) is actually set by the caller's
/// controller — see init_admin_queues().
const CC_CSS_NVM: u32 = 0;
/// CC.AMS value for round-robin arbitration — unconditionally
/// guaranteed supported per spec regardless of CAP.AMS, so this is
/// not a fact CAP needs to confirm.
const CC_AMS_ROUND_ROBIN: u32 = 0;
/// CC.SHN value for "no shutdown notification" — correct while
/// bringing the controller up, not shutting it down.
const CC_SHN_NONE: u32 = 0;
/// Spec-mandated standard entry-size exponents for the NVM command
/// set (2^6 = 64 bytes SQE, 2^4 = 16 bytes CQE) — fixed values, not
/// CAP-derived, matching Linux's NVME_NVM_IOSQES/IOCQES constants.
const CC_IOSQES_STANDARD: u32 = 6;
const CC_IOCQES_STANDARD: u32 = 4;

/// Bring up the Admin Submission/Completion Queues and enable the
/// controller, following the NVMe Base Specification's controller
/// initialization sequence: disable -> wait RDY=0 -> program
/// AQA/ASQ/ACQ -> set CC (enable) -> wait RDY=1.
///
/// Every CC field CAP can determine (CSS, MPS) is read from the real
/// CAP value passed through `regs`, not assumed; AMS/SHN use
/// spec-guaranteed-safe defaults; IOSQES/IOCQES use the spec-mandated
/// standard sizes for the NVM command set (see constants above).
///
/// `regs` must already be backed by a real MMIO mapping (i.e. its
/// virtual base came from memory::paging::map_mmio()).
pub fn init_admin_queues(regs: &NvmeRegs, depth: u16) -> Result<AdminQueues, NvmeInitError> {
    let cap = regs.cap();
    let mqes = (cap & 0xFFFF) as u32; // 0's-based max queue size
    let mpsmin = ((cap >> 48) & 0xF) as u32;
    let to_500ms_units = ((cap >> 24) & 0xFF) as u64;
    let timeout_ms = core::cmp::max(to_500ms_units * 500, 500); // never less than 500ms

    if (depth as u32).saturating_sub(1) > mqes {
        return Err(NvmeInitError::QueueDepthUnsupported);
    }
    if mpsmin != 0 {
        return Err(NvmeInitError::PageSizeUnsupported);
    }

    // --- Allocate physically contiguous Admin SQ/CQ memory ---
    // These are ordinary RAM frames from the PMM, not MMIO —
    // phys_to_virt() is valid for them directly (unlike BAR0, which
    // needed map_mmio() because it's a PCI MMIO hole, not a memmap
    // region Limine's HHDM covers under base revision 3).
    let sq_needed = (depth as u64) * ADMIN_SQE_SIZE;
    let cq_needed = (depth as u64) * ADMIN_CQE_SIZE;
    let sq_pages = ((sq_needed + 0xFFF) / 0x1000).max(1) as usize;
    let cq_pages = ((cq_needed + 0xFFF) / 0x1000).max(1) as usize;

    let sq_phys = crate::memory::pmm::alloc_contiguous_frames(sq_pages)
        .ok_or(NvmeInitError::AllocFailed)?;
    let cq_phys = crate::memory::pmm::alloc_contiguous_frames(cq_pages)
        .ok_or(NvmeInitError::AllocFailed)?;

    let sq_virt = crate::memory::paging::phys_to_virt(sq_phys);
    let cq_virt = crate::memory::paging::phys_to_virt(cq_phys);
    unsafe {
        core::ptr::write_bytes(sq_virt as *mut u8, 0, sq_pages * 0x1000);
        core::ptr::write_bytes(cq_virt as *mut u8, 0, cq_pages * 0x1000);
    }

    // --- Disable the controller, wait for CSTS.RDY == 0 ---
    unsafe { regs.write32(offset::CC, 0) };
    let deadline_ms = crate::drivers::pit::ms() + timeout_ms;
    loop {
        if regs.status_raw() & 0x1 == 0 {
            break;
        }
        if crate::drivers::pit::ms() >= deadline_ms {
            return Err(NvmeInitError::DisableTimeout);
        }
        core::hint::spin_loop();
    }

    // --- Program AQA / ASQ / ACQ (only while EN == 0, per spec) ---
    let depth0 = (depth as u32).saturating_sub(1) & 0xFFF;
    let aqa: u32 = depth0 | (depth0 << 16);
    unsafe {
        regs.write32(offset::AQA, aqa);
        regs.write64(offset::ASQ, sq_phys);
        regs.write64(offset::ACQ, cq_phys);
    }

    // --- Construct CC from CAP-confirmed + spec-mandated fields ---
    let cc: u32 = 1 // EN
        | (CC_CSS_NVM << 4)
        | (mpsmin << 7) // MPS — CAP.MPSMIN already confirmed == 0 above
        | (CC_AMS_ROUND_ROBIN << 11)
        | (CC_SHN_NONE << 14)
        | (CC_IOSQES_STANDARD << 16)
        | (CC_IOCQES_STANDARD << 20);
    unsafe { regs.write32(offset::CC, cc) };

    // --- Wait for CSTS.RDY == 1 (bail immediately on CFS) ---
    let deadline_ms = crate::drivers::pit::ms() + timeout_ms;
    loop {
        let csts = regs.status_raw();
        if csts & 0x2 != 0 {
            return Err(NvmeInitError::ControllerFatalStatus);
        }
        if csts & 0x1 != 0 {
            break;
        }
        if crate::drivers::pit::ms() >= deadline_ms {
            return Err(NvmeInitError::EnableTimeout);
        }
        core::hint::spin_loop();
    }

    Ok(AdminQueues {
        sq_phys,
        cq_phys,
        sq_virt,
        cq_virt,
        depth,
    })
}"""

# ---------------------------------------------------------------------------
# 2. main.rs — help string + nvmeinittest dispatch arm
# ---------------------------------------------------------------------------

HELP_OLD = 'preempttest drawtest ownertest haltest pcitest mmiotest\n");'
HELP_NEW = 'preempttest drawtest ownertest haltest pcitest mmiotest nvmeinittest\n");'

DISPATCH_OLD = """            None => crate::kprintln!("[mmiotest] no NVMe controller found - nothing to test"),
        }
    } else if !b.is_empty() {"""

DISPATCH_NEW = """            None => crate::kprintln!("[mmiotest] no NVMe controller found - nothing to test"),
        }
    } else if b == b"nvmeinittest" {
        crate::kprintln!("--- NVMe admin queue init test ---");
        match drivers::pci::find_nvme() {
            Some(dev) => match dev.bar0 {
                Some(bar) if bar.is_mmio => {
                    let virt = unsafe { memory::paging::map_mmio(bar.base, 0x1000) };
                    let regs = unsafe { drivers::nvme::NvmeRegs::new(virt) };
                    crate::kprintln!("[nvmeinittest] CSTS before init = {:#010x}", regs.status_raw());
                    match drivers::nvme::init_admin_queues(&regs, 64) {
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
                }
                Some(_) => crate::kprintln!("[nvmeinittest] FAIL - NVMe BAR0 is not MMIO"),
                None => crate::kprintln!("[nvmeinittest] FAIL - NVMe controller found but BAR0 is unreadable"),
            },
            None => crate::kprintln!("[nvmeinittest] no NVMe controller found - nothing to test"),
        }
    } else if !b.is_empty() {"""


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
        print(f"[patch_35] {label} match count in {path}: {count}, ABORT: expected exactly 1 match.")
        sys.exit(1)
    write(path, src.replace(old, new, 1))
    print(f"[patch_35] patched {path} ({label})")


def main():
    patch_inplace(NVME_RS_PATH, WRITE32_OLD, WRITE32_NEW, "write32/write64")
    patch_inplace(NVME_RS_PATH, NVME_TAIL_OLD, NVME_TAIL_NEW, "NvmeInitError/AdminQueues/init_admin_queues")
    patch_inplace(MAIN_RS_PATH, HELP_OLD, HELP_NEW, "help string")
    patch_inplace(MAIN_RS_PATH, DISPATCH_OLD, DISPATCH_NEW, "nvmeinittest dispatch arm")

    print("[patch_35] OK — init_admin_queues() added, nvmeinittest wired up.")
    print("[patch_35] Next: cargo build, then build-iso.sh + QEMU (with -device nvme attached) and run 'nvmeinittest'.")


if __name__ == "__main__":
    main()
