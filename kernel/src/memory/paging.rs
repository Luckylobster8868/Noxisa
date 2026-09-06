//! Virtual Memory — identity mapped by boot.s (1:1 phys=virt for first 512GB)
extern crate alloc;
use spin::Mutex;

static HHDM: Mutex<u64> = Mutex::new(0);

pub fn init(offset: u64) { *HHDM.lock() = offset; }

#[inline] pub fn phys_to_virt(phys: u64) -> u64 { phys + *HHDM.lock() }
#[inline] pub fn virt_to_phys(virt: u64) -> u64 { virt - *HHDM.lock() }
#[inline] pub fn hhdm_offset() -> u64 { *HHDM.lock() }

#[derive(Debug, Clone, Copy)]
pub struct PageFlags(u64);
impl PageFlags {
    pub const PRESENT:     Self = Self(1 << 0);
    pub const WRITABLE:    Self = Self(1 << 1);
    pub const NO_EXEC:     Self = Self(1 << 63);
    pub const KERNEL_RO:   Self = Self(Self::PRESENT.0 | Self::NO_EXEC.0);
    pub const KERNEL_RW:   Self = Self(Self::PRESENT.0 | Self::WRITABLE.0 | Self::NO_EXEC.0);
    pub const KERNEL_RX:   Self = Self(Self::PRESENT.0);
    pub const KERNEL_MMIO: Self = Self(Self::PRESENT.0 | Self::WRITABLE.0 | (1 << 4) | Self::NO_EXEC.0);
    pub fn bits(self) -> u64 { self.0 }
    pub fn or(self, other: Self) -> Self { Self(self.0 | other.0) }
}

pub fn map_page(_virt: u64, _phys: u64, _flags: PageFlags) {
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
}

pub unsafe fn create_isolated_user_table(user_pages: &[u64], stack_phys: u64) -> (u64, alloc::vec::Vec<u64>) {
    const PAGE_PRESENT: u64 = 1 << 0;
    const PAGE_WRITABLE: u64 = 1 << 1;
    const PAGE_USER: u64 = 1 << 2;
    const NO_EXECUTE: u64 = 1 << 63;
    const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

    let mut table_frames: alloc::vec::Vec<u64> = alloc::vec::Vec::new();

    unsafe fn map_user_page(new_pml4: *mut u64, virt_phys: u64, writable: bool, executable: bool, table_frames: &mut alloc::vec::Vec<u64>) {
        const PAGE_PRESENT: u64 = 1 << 0;
        const PAGE_WRITABLE: u64 = 1 << 1;
        const PAGE_USER: u64 = 1 << 2;
        const NO_EXECUTE: u64 = 1 << 63;
        const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

        let pml4_idx = ((virt_phys >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((virt_phys >> 30) & 0x1FF) as usize;
        let pd_idx   = ((virt_phys >> 21) & 0x1FF) as usize;
        let pt_idx   = ((virt_phys >> 12) & 0x1FF) as usize;

        let pml4e = new_pml4.add(pml4_idx);
        if *pml4e & PAGE_PRESENT == 0 {
            let new_pdpt = crate::memory::pmm::alloc_frame()
                .expect("out of memory (PDPT, isolated table)");
            core::ptr::write_bytes(phys_to_virt(new_pdpt) as *mut u8, 0, 4096);
            *pml4e = new_pdpt | PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER;
            table_frames.push(new_pdpt);
        } else {
            *pml4e |= PAGE_USER;
        }
        let pdpt = phys_to_virt(*pml4e & ADDR_MASK) as *mut u64;

        let pdpte = pdpt.add(pdpt_idx);
        if *pdpte & PAGE_PRESENT == 0 {
            let new_pd = crate::memory::pmm::alloc_frame()
                .expect("out of memory (PD, isolated table)");
            core::ptr::write_bytes(phys_to_virt(new_pd) as *mut u8, 0, 4096);
            *pdpte = new_pd | PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER;
            table_frames.push(new_pd);
        } else {
            *pdpte |= PAGE_USER;
        }
        let pd = phys_to_virt(*pdpte & ADDR_MASK) as *mut u64;

        let pde = pd.add(pd_idx);
        if *pde & PAGE_PRESENT == 0 {
            let new_pt = crate::memory::pmm::alloc_frame()
                .expect("out of memory (PT, isolated table)");
            core::ptr::write_bytes(phys_to_virt(new_pt) as *mut u8, 0, 4096);
            *pde = new_pt | PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER;
            table_frames.push(new_pt);
        } else {
            *pde |= PAGE_USER;
        }
        let pt = phys_to_virt(*pde & ADDR_MASK) as *mut u64;

        let phys_page = virt_phys & ADDR_MASK;
        let mut flags = PAGE_PRESENT | PAGE_USER;
        if writable { flags |= PAGE_WRITABLE; }
        if !executable { flags |= NO_EXECUTE; }
        pt.add(pt_idx).write_volatile(phys_page | flags);
    }

    let cr3: u64;
    core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
    let kernel_pml4 = phys_to_virt(cr3 & ADDR_MASK) as *const u64;

    let new_pml4_phys = crate::memory::pmm::alloc_frame()
        .expect("out of memory building isolated PML4");
    table_frames.push(new_pml4_phys);
    let new_pml4 = phys_to_virt(new_pml4_phys) as *mut u64;
    core::ptr::write_bytes(new_pml4 as *mut u8, 0, 4096);

    for i in 256..512usize {
        new_pml4.add(i).write_volatile(kernel_pml4.add(i).read_volatile());
    }

    for &page in user_pages {
        map_user_page(new_pml4, page, false, true, &mut table_frames);
    }
    map_user_page(new_pml4, stack_phys, true, false, &mut table_frames);

    (new_pml4_phys, table_frames)
}

pub unsafe fn create_isolated_user_table_fresh(pages: &[(u64, u64, bool, bool)], stack_phys: u64) -> (u64, alloc::vec::Vec<u64>) {
    const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
    let mut table_frames: alloc::vec::Vec<u64> = alloc::vec::Vec::new();

    unsafe fn map_user_page_split(
        new_pml4: *mut u64, vaddr: u64, paddr: u64,
        writable: bool, executable: bool, table_frames: &mut alloc::vec::Vec<u64>,
    ) {
        const PAGE_PRESENT: u64 = 1 << 0;
        const PAGE_WRITABLE: u64 = 1 << 1;
        const PAGE_USER: u64 = 1 << 2;
        const NO_EXECUTE: u64 = 1 << 63;
        const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

        let pml4_idx = ((vaddr >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((vaddr >> 30) & 0x1FF) as usize;
        let pd_idx   = ((vaddr >> 21) & 0x1FF) as usize;
        let pt_idx   = ((vaddr >> 12) & 0x1FF) as usize;

        let pml4e = new_pml4.add(pml4_idx);
        if *pml4e & PAGE_PRESENT == 0 {
            let new_pdpt = crate::memory::pmm::alloc_frame()
                .expect("out of memory (PDPT, isolated table)");
            core::ptr::write_bytes(phys_to_virt(new_pdpt) as *mut u8, 0, 4096);
            *pml4e = new_pdpt | PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER;
            table_frames.push(new_pdpt);
        } else {
            *pml4e |= PAGE_USER;
        }
        let pdpt = phys_to_virt(*pml4e & ADDR_MASK) as *mut u64;

        let pdpte = pdpt.add(pdpt_idx);
        if *pdpte & PAGE_PRESENT == 0 {
            let new_pd = crate::memory::pmm::alloc_frame()
                .expect("out of memory (PD, isolated table)");
            core::ptr::write_bytes(phys_to_virt(new_pd) as *mut u8, 0, 4096);
            *pdpte = new_pd | PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER;
            table_frames.push(new_pd);
        } else {
            *pdpte |= PAGE_USER;
        }
        let pd = phys_to_virt(*pdpte & ADDR_MASK) as *mut u64;

        let pde = pd.add(pd_idx);
        if *pde & PAGE_PRESENT == 0 {
            let new_pt = crate::memory::pmm::alloc_frame()
                .expect("out of memory (PT, isolated table)");
            core::ptr::write_bytes(phys_to_virt(new_pt) as *mut u8, 0, 4096);
            *pde = new_pt | PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER;
            table_frames.push(new_pt);
        } else {
            *pde |= PAGE_USER;
        }
        let pt = phys_to_virt(*pde & ADDR_MASK) as *mut u64;

        let phys_page = paddr & ADDR_MASK;
        let mut flags = PAGE_PRESENT | PAGE_USER;
        if writable { flags |= PAGE_WRITABLE; }
        if !executable { flags |= NO_EXECUTE; }
        pt.add(pt_idx).write_volatile(phys_page | flags);
    }

    let cr3: u64;
    core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
    let kernel_pml4 = phys_to_virt(cr3 & ADDR_MASK) as *const u64;

    let new_pml4_phys = crate::memory::pmm::alloc_frame()
        .expect("out of memory building isolated PML4");
    table_frames.push(new_pml4_phys);
    let new_pml4 = phys_to_virt(new_pml4_phys) as *mut u64;
    core::ptr::write_bytes(new_pml4 as *mut u8, 0, 4096);

    for i in 256..512usize {
        new_pml4.add(i).write_volatile(kernel_pml4.add(i).read_volatile());
    }

    for &(vaddr, paddr, writable, executable) in pages {
        // Real per-segment R/W/X now threaded through from load_fresh() -
        // closes the W^X gap: code pages arrive as (writable=false,
        // executable=true), data pages as (writable=true, executable=false),
        // instead of every page being mapped writable+executable
        // unconditionally as before.
        map_user_page_split(new_pml4, vaddr, paddr, writable, executable, &mut table_frames);
    }
    // Stack: identity-mapped (vaddr == paddr) same as isotest/captest's
    // stack handling - map_user_page_split() already covers this case,
    // nothing further needed. The original map_user_page() this used to
    // call is a nested fn private to create_isolated_user_table()'s own
    // body, invisible here - not an available module-level function.
    map_user_page_split(new_pml4, stack_phys, stack_phys, true, false, &mut table_frames);

    (new_pml4_phys, table_frames)
}

/// Read back a leaf PTE's writable/executable bits for a given vaddr in a
/// given PML4 - used to prove (not just assume) that per-segment W^X flags
/// actually landed in the page table, rather than trusting the call site
/// that built them. Returns None if any level of the walk isn't present.
pub unsafe fn debug_read_pte_flags(pml4_phys: u64, vaddr: u64) -> Option<(bool, bool)> {
    const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
    const PAGE_PRESENT: u64 = 1 << 0;
    const PAGE_WRITABLE: u64 = 1 << 1;
    const NO_EXECUTE: u64 = 1 << 63;

    let pml4 = phys_to_virt(pml4_phys & ADDR_MASK) as *const u64;
    let pml4_idx = ((vaddr >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((vaddr >> 30) & 0x1FF) as usize;
    let pd_idx   = ((vaddr >> 21) & 0x1FF) as usize;
    let pt_idx   = ((vaddr >> 12) & 0x1FF) as usize;

    let pml4e = pml4.add(pml4_idx).read_volatile();
    if pml4e & PAGE_PRESENT == 0 { return None; }
    let pdpt = phys_to_virt(pml4e & ADDR_MASK) as *const u64;
    let pdpte = pdpt.add(pdpt_idx).read_volatile();
    if pdpte & PAGE_PRESENT == 0 { return None; }
    let pd = phys_to_virt(pdpte & ADDR_MASK) as *const u64;
    let pde = pd.add(pd_idx).read_volatile();
    if pde & PAGE_PRESENT == 0 { return None; }
    let pt = phys_to_virt(pde & ADDR_MASK) as *const u64;
    let pte = pt.add(pt_idx).read_volatile();
    if pte & PAGE_PRESENT == 0 { return None; }

    Some((pte & PAGE_WRITABLE != 0, pte & NO_EXECUTE == 0))
}

pub unsafe fn teardown_isolated_process(table_frames: &[u64], stack_phys: u64) {
    for &frame in table_frames {
        crate::memory::pmm::free_frame(frame);
    }
    crate::memory::pmm::free_frame(stack_phys);
}

pub unsafe fn identity_map_region(phys_start: u64, size: u64) {
    const PAGE_PRESENT: u64 = 1 << 0;
    const PAGE_WRITABLE: u64 = 1 << 1;
    const PAGE_HUGE: u64 = 1 << 7;
    const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

    let start = phys_start & !0x1FFFFF;
    let end = (phys_start + size + 0x1FFFFF) & !0x1FFFFF;

    let cr3: u64;
    core::arch::asm!("mov {}, cr3", out(reg) cr3);
    let pml4 = phys_to_virt(cr3 & ADDR_MASK) as *mut u64;

    let mut addr = start;
    while addr < end {
        let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((addr >> 30) & 0x1FF) as usize;
        let pd_idx   = ((addr >> 21) & 0x1FF) as usize;

        let pml4_entry = pml4.add(pml4_idx);
        if *pml4_entry & PAGE_PRESENT == 0 {
            let new_pdpt = crate::memory::pmm::alloc_frame()
                .expect("out of memory mapping framebuffer (PDPT)");
            core::ptr::write_bytes(phys_to_virt(new_pdpt) as *mut u8, 0, 4096);
            *pml4_entry = new_pdpt | PAGE_PRESENT | PAGE_WRITABLE;
        }
        let pdpt = phys_to_virt((*pml4_entry) & ADDR_MASK) as *mut u64;

        let pdpt_entry = pdpt.add(pdpt_idx);
        if *pdpt_entry & PAGE_PRESENT == 0 {
            let new_pd = crate::memory::pmm::alloc_frame()
                .expect("out of memory mapping framebuffer (PD)");
            core::ptr::write_bytes(phys_to_virt(new_pd) as *mut u8, 0, 4096);
            *pdpt_entry = new_pd | PAGE_PRESENT | PAGE_WRITABLE;
        }
        let pd = phys_to_virt((*pdpt_entry) & ADDR_MASK) as *mut u64;

        let pd_entry = pd.add(pd_idx);
        *pd_entry = addr | PAGE_PRESENT | PAGE_WRITABLE | PAGE_HUGE;
        flush_tlb(addr);

        addr += 0x200000;
    }
}

#[inline]
pub fn flush_tlb(virt: u64) {
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) virt,
            options(nostack, preserves_flags));
    }
}
