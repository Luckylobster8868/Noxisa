#!/usr/bin/env python3
"""
patch_34_mmio.py — Phase B.1, step 2: real MMIO mapping + NVMe register
                    representation + mmiotest kshell command.

Run from kernel/src, same convention as prior patches.

Context (established via patch_33b/33c diagnostics): this kernel runs
under Limine base revision 3 (BASE_REVISION.actual_revision() == 3),
under which HHDM only covers Usable/Bootloader-reclaimable/Executable-
and-modules/Framebuffer memmap regions — NOT arbitrary PCI MMIO holes.
So phys_to_virt() alone is NOT safe for the NVMe BAR0; map_mmio() below
builds real page-table entries rather than assuming Limine already
mapped anything there.

What this does:
  1. memory/paging.rs: adds map_mmio(phys_start, size) -> u64.
     Builds real PML4->PDPT->PD->PT entries at 4KB granularity (not
     identity_map_region()'s 2MB huge pages - the NVMe register window
     is small and huge pages would be needlessly coarse and could
     spill onto adjacent devices' MMIO ranges). Uses the existing
     PageFlags::KERNEL_MMIO flags (present+writable+cache-disable+NX)
     that were defined in patch_23 but never actually consumed by any
     caller until now. Maps at the HHDM-numbered virtual address
     (hhdm_offset + phys) so downstream code's addressing convention
     stays consistent with the rest of the kernel - the difference
     from phys_to_virt() alone is that this function actually CREATES
     the mapping instead of assuming Limine did. Guards against an
     existing PD/PDPT entry already being a huge page (bit 7) at the
     spot we need to walk through - refuses rather than corrupts.
     Does NOT touch map_page() (still a no-op, still relied on as
     such by security/memory_hardening.rs - out of scope here).
  2. drivers/nvme.rs (new): NVMe register offset constants (CAP, VS,
     INTMS, INTMC, CC, CSTS, AQA, ASQ, ACQ, per the NVMe Base Spec)
     and a minimal NvmeRegs volatile-access wrapper. Read-only for
     this patch (cap/version/status_raw/doorbell_stride) - admin
     queue setup and CC/CSTS write sequencing land in patch_35.
  3. drivers/mod.rs: adds `pub mod nvme;`
  4. main.rs: adds "mmiotest" to help's command list, and a mmiotest
     dispatch arm that finds the NVMe controller (same as pcitest),
     maps its BAR0 via map_mmio(), and reads back CAP/VS/CSTS/DSTRD.

Explicitly NOT in this patch: admin queue allocation, CC/CSTS enable
sequencing, Identify Controller/Namespace, doorbells actually being
rung, I/O queues. Those are patch_35 onward per the approved Phase
B.1 progression.
"""
import sys

PAGING_RS_PATH = "memory/paging.rs"
NVME_RS_PATH = "drivers/nvme.rs"
DRIVERS_MOD_PATH = "drivers/mod.rs"
MAIN_RS_PATH = "main.rs"

# ---------------------------------------------------------------------------
# 1. paging.rs — add map_mmio()
# ---------------------------------------------------------------------------

PAGING_OLD = """pub fn map_page(_virt: u64, _phys: u64, _flags: PageFlags) {
    // identity mapped — no-op for now
}"""

PAGING_NEW = """pub fn map_page(_virt: u64, _phys: u64, _flags: PageFlags) {
    // identity mapped — no-op for now
}

/// Map a physical MMIO region so it can actually be accessed.
///
/// Confirmed via BASE_REVISION.actual_revision() == 3 (patch_33b/33c
/// diagnostic) that this kernel runs under Limine base revision 3,
/// where HHDM only covers Usable/Bootloader-reclaimable/Executable-
/// and-modules/Framebuffer memmap regions. A PCI MMIO BAR (like an
/// NVMe controller's BAR0) is none of those, so phys_to_virt() alone
/// cannot be trusted here — this function builds real page-table
/// entries instead of assuming a mapping already exists, the same
/// lesson identity_map_region() already applies to the ELF loader's
/// low-memory pages, now applied to MMIO too.
///
/// Maps at 4KB granularity (not identity_map_region()'s 2MB huge
/// pages — an MMIO register window is small, and a 2MB page could
/// spill onto neighbouring devices' physical address space) using
/// PageFlags::KERNEL_MMIO (present+writable+cache-disable+NX) so
/// reads/writes actually reach the device instead of being cached.
///
/// Chooses the HHDM-numbered virtual address (hhdm_offset + phys) so
/// code written against this mapping uses the same addressing
/// convention as the rest of the kernel — phys_to_virt() would
/// compute the identical number, this function just backs it with a
/// real mapping first.
///
/// Refuses (panics) rather than silently corrupting page tables if
/// an existing PDPT/PD entry along the walk is already a huge page
/// (bit 7) — splitting a huge page isn't implemented, and blindly
/// treating its address bits as a next-level table pointer would be
/// memory corruption.
///
/// # Safety
/// Caller must ensure `phys_start`/`size` genuinely describe a
/// device MMIO region (e.g. from a PCI BAR), not RAM already in use.
pub unsafe fn map_mmio(phys_start: u64, size: u64) -> u64 {
    const PAGE_PRESENT: u64 = 1 << 0;
    const PAGE_WRITABLE: u64 = 1 << 1;
    const PAGE_PCD: u64 = 1 << 4; // cache-disable — required for MMIO correctness
    const PAGE_HUGE: u64 = 1 << 7;
    const NO_EXECUTE: u64 = 1 << 63;
    const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

    let page_start = phys_start & !0xFFF;
    let page_end = (phys_start + size + 0xFFF) & !0xFFF;
    let offset = hhdm_offset();

    let cr3: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack)); }
    let pml4 = phys_to_virt(cr3 & ADDR_MASK) as *mut u64;

    let mut phys = page_start;
    while phys < page_end {
        let virt = offset + phys;

        let pml4_idx = ((virt >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((virt >> 30) & 0x1FF) as usize;
        let pd_idx   = ((virt >> 21) & 0x1FF) as usize;
        let pt_idx   = ((virt >> 12) & 0x1FF) as usize;

        let pml4e = unsafe { pml4.add(pml4_idx) };
        if unsafe { *pml4e } & PAGE_PRESENT == 0 {
            let new_pdpt = crate::memory::pmm::alloc_frame()
                .expect("out of memory mapping MMIO region (PDPT)");
            unsafe { core::ptr::write_bytes(phys_to_virt(new_pdpt) as *mut u8, 0, 4096); }
            unsafe { *pml4e = new_pdpt | PAGE_PRESENT | PAGE_WRITABLE; }
        }
        let pdpt = phys_to_virt(unsafe { *pml4e } & ADDR_MASK) as *mut u64;

        let pdpte = unsafe { pdpt.add(pdpt_idx) };
        if unsafe { *pdpte } & PAGE_PRESENT != 0 && unsafe { *pdpte } & PAGE_HUGE != 0 {
            panic!("map_mmio: PDPT entry at {:#x} is a 1GiB huge page, cannot split", virt);
        }
        if unsafe { *pdpte } & PAGE_PRESENT == 0 {
            let new_pd = crate::memory::pmm::alloc_frame()
                .expect("out of memory mapping MMIO region (PD)");
            unsafe { core::ptr::write_bytes(phys_to_virt(new_pd) as *mut u8, 0, 4096); }
            unsafe { *pdpte = new_pd | PAGE_PRESENT | PAGE_WRITABLE; }
        }
        let pd = phys_to_virt(unsafe { *pdpte } & ADDR_MASK) as *mut u64;

        let pde = unsafe { pd.add(pd_idx) };
        if unsafe { *pde } & PAGE_PRESENT != 0 && unsafe { *pde } & PAGE_HUGE != 0 {
            panic!("map_mmio: PD entry at {:#x} is a 2MiB huge page, cannot split", virt);
        }
        if unsafe { *pde } & PAGE_PRESENT == 0 {
            let new_pt = crate::memory::pmm::alloc_frame()
                .expect("out of memory mapping MMIO region (PT)");
            unsafe { core::ptr::write_bytes(phys_to_virt(new_pt) as *mut u8, 0, 4096); }
            unsafe { *pde = new_pt | PAGE_PRESENT | PAGE_WRITABLE; }
        }
        let pt = phys_to_virt(unsafe { *pde } & ADDR_MASK) as *mut u64;

        unsafe {
            pt.add(pt_idx).write_volatile(phys | PAGE_PRESENT | PAGE_WRITABLE | PAGE_PCD | NO_EXECUTE);
        }
        flush_tlb(virt);

        phys += 0x1000;
    }

    offset + page_start
}"""

# ---------------------------------------------------------------------------
# 2. drivers/nvme.rs — new file
# ---------------------------------------------------------------------------

NVME_RS_CONTENT = r"""//! NVMe register representation (Phase B.1, patch_34)
//!
//! Register offsets and a minimal volatile-access wrapper over the
//! MMIO region mapped by memory::paging::map_mmio(). This patch only
//! covers register *layout* plus a read-only smoke test (mmiotest) —
//! no admin queue setup, no CC/CSTS enable sequencing yet. Those come
//! in patch_35 per the approved Phase B.1 progression.

/// NVMe register offsets, per the NVMe Base Specification. Validated
/// against the values called out in the approved design doc; cross-
/// check against Redox's nvmed reference (storage/nvmed/) before
/// patch_35 adds anything beyond these four already-tested here.
pub mod offset {
    pub const CAP: u64 = 0x00; // Controller Capabilities (u64)
    pub const VS: u64 = 0x08; // Version (u32)
    pub const INTMS: u64 = 0x0C; // Interrupt Mask Set (u32)
    pub const INTMC: u64 = 0x10; // Interrupt Mask Clear (u32)
    pub const CC: u64 = 0x14; // Controller Configuration (u32)
    pub const CSTS: u64 = 0x1C; // Controller Status (u32)
    pub const AQA: u64 = 0x24; // Admin Queue Attributes (u32)
    pub const ASQ: u64 = 0x28; // Admin Submission Queue Base (u64)
    pub const ACQ: u64 = 0x30; // Admin Completion Queue Base (u64)
}

/// Wraps an already-mapped NVMe MMIO base (a virtual address, as
/// returned by memory::paging::map_mmio()) with volatile register
/// accessors. Does not map anything itself — the caller must call
/// map_mmio() first and pass the returned virtual base in here.
pub struct NvmeRegs {
    base: u64,
}

impl NvmeRegs {
    /// # Safety
    /// `virt_base` must be a valid, already-mapped MMIO virtual
    /// address (the return value of memory::paging::map_mmio() for
    /// this controller's BAR0), and must remain mapped for the
    /// lifetime of this struct.
    pub unsafe fn new(virt_base: u64) -> Self {
        Self { base: virt_base }
    }

    #[inline]
    unsafe fn read32(&self, off: u64) -> u32 {
        unsafe { core::ptr::read_volatile((self.base + off) as *const u32) }
    }

    #[inline]
    unsafe fn read64(&self, off: u64) -> u64 {
        unsafe { core::ptr::read_volatile((self.base + off) as *const u64) }
    }

    #[inline]
    #[allow(dead_code)] // unused until patch_35 (admin queue / CC setup)
    unsafe fn write32(&self, off: u64, val: u32) {
        unsafe { core::ptr::write_volatile((self.base + off) as *mut u32, val) }
    }

    /// Controller Capabilities (CAP, offset 0x00, 64-bit).
    pub fn cap(&self) -> u64 {
        unsafe { self.read64(offset::CAP) }
    }

    /// Version (VS, offset 0x08, 32-bit), decoded as (major, minor, tertiary).
    pub fn version(&self) -> (u16, u8, u8) {
        let raw = unsafe { self.read32(offset::VS) };
        let major = (raw >> 16) as u16;
        let minor = ((raw >> 8) & 0xFF) as u8;
        let tertiary = (raw & 0xFF) as u8;
        (major, minor, tertiary)
    }

    /// Controller Status (CSTS, offset 0x1C, 32-bit) — raw for now;
    /// bit-level decode (RDY, CFS, ...) is added when patch_35 needs
    /// to poll CSTS.RDY during the enable sequence.
    pub fn status_raw(&self) -> u32 {
        unsafe { self.read32(offset::CSTS) }
    }

    /// CAP.DSTRD (doorbell stride), bits 32-35 of CAP. Needed later
    /// for I/O doorbell addressing (patch_37) — extracted now since
    /// it comes from the same CAP read already being exercised here,
    /// and the design doc explicitly calls out not hard-coding this.
    pub fn doorbell_stride(&self) -> u32 {
        ((self.cap() >> 32) & 0xF) as u32
    }
}
"""

# ---------------------------------------------------------------------------
# 3. drivers/mod.rs — add pub mod nvme;
# ---------------------------------------------------------------------------

DRIVERS_MOD_OLD = "pub mod pci;       // Phase B.1 - PCI config-space + enumeration\n"
DRIVERS_MOD_NEW = (
    "pub mod pci;       // Phase B.1 - PCI config-space + enumeration\n"
    "pub mod nvme;       // Phase B.1 - NVMe register representation + MMIO test\n"
)

# ---------------------------------------------------------------------------
# 4. main.rs — help string + mmiotest dispatch arm
# ---------------------------------------------------------------------------

HELP_OLD = 'preempttest drawtest ownertest haltest pcitest\n");'
HELP_NEW = 'preempttest drawtest ownertest haltest pcitest mmiotest\n");'

DISPATCH_OLD = """            None => crate::kprintln!("[pcitest] no NVMe controller found (expected on hardware/QEMU config without one attached)"),
        }
    } else if !b.is_empty() {"""

DISPATCH_NEW = """            None => crate::kprintln!("[pcitest] no NVMe controller found (expected on hardware/QEMU config without one attached)"),
        }
    } else if b == b"mmiotest" {
        crate::kprintln!("--- NVMe MMIO register test ---");
        match drivers::pci::find_nvme() {
            Some(dev) => match dev.bar0 {
                Some(bar) if bar.is_mmio => {
                    let virt = unsafe { memory::paging::map_mmio(bar.base, 0x1000) };
                    crate::kprintln!("[mmiotest] mapped BAR0 phys={:#x} -> virt={:#x}", bar.base, virt);
                    let regs = unsafe { drivers::nvme::NvmeRegs::new(virt) };
                    let cap = regs.cap();
                    let (maj, min, ter) = regs.version();
                    let csts = regs.status_raw();
                    let dstrd = regs.doorbell_stride();
                    crate::kprintln!("[mmiotest] CAP  = {:#018x}", cap);
                    crate::kprintln!("[mmiotest] VS   = {}.{}.{}", maj, min, ter);
                    crate::kprintln!("[mmiotest] CSTS = {:#010x}", csts);
                    crate::kprintln!("[mmiotest] CAP.DSTRD = {}", dstrd);
                    if cap != 0 && cap != u64::MAX {
                        crate::kprintln!("[mmiotest] PASS - CAP register reads a plausible non-degenerate value");
                    } else {
                        crate::kprintln!("[mmiotest] FAIL - CAP read as {:#x} (0 or all-ones suggests a mapping/bus problem)", cap);
                    }
                }
                Some(_) => crate::kprintln!("[mmiotest] FAIL - NVMe BAR0 is not MMIO"),
                None => crate::kprintln!("[mmiotest] FAIL - NVMe controller found but BAR0 is unreadable"),
            },
            None => crate::kprintln!("[mmiotest] no NVMe controller found - nothing to test"),
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
        print(f"[patch_34] {label} match count in {path}: {count}, ABORT: expected exactly 1 match.")
        sys.exit(1)
    write(path, src.replace(old, new, 1))
    print(f"[patch_34] patched {path} ({label})")


def main():
    # 1. paging.rs — add map_mmio()
    patch_inplace(PAGING_RS_PATH, PAGING_OLD, PAGING_NEW, "map_mmio()")

    # 2. drivers/nvme.rs — new file, abort if it already exists
    try:
        with open(NVME_RS_PATH, "x", encoding="utf-8") as f:
            f.write(NVME_RS_CONTENT)
        print(f"[patch_34] created {NVME_RS_PATH}")
    except FileExistsError:
        print(f"[patch_34] ABORT: {NVME_RS_PATH} already exists — refusing to overwrite.")
        sys.exit(1)

    # 3. drivers/mod.rs — add pub mod nvme;
    patch_inplace(DRIVERS_MOD_PATH, DRIVERS_MOD_OLD, DRIVERS_MOD_NEW, "pub mod nvme;")

    # 4. main.rs — help string + mmiotest dispatch arm
    patch_inplace(MAIN_RS_PATH, HELP_OLD, HELP_NEW, "help string")
    patch_inplace(MAIN_RS_PATH, DISPATCH_OLD, DISPATCH_NEW, "mmiotest dispatch arm")

    print("[patch_34] OK — map_mmio() added, nvme.rs created, drivers/mod.rs + main.rs patched.")
    print("[patch_34] Next: cargo build, then build-iso.sh + QEMU (with -device nvme attached) and run 'mmiotest'.")


if __name__ == "__main__":
    main()
