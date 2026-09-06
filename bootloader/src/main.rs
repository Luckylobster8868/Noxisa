//! Noxisa UEFI Bootloader
//!
//! Steps:
//!  1. UEFI init + logging
//!  2. Get GOP framebuffer
//!  3. Load kernel ELF from ESP: \EFI\Noxisa\kernel.elf
//!  4. Build memory map from UEFI
//!  5. Set up page tables (identity map + HHDM at 0xFFFF_8000_0000_0000)
//!  6. Exit boot services
//!  7. Jump to kernel_main with BootInfo pointer

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use uefi::prelude::*;
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};
use uefi::proto::media::file::{File, FileAttribute, FileMode, RegularFile};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::table::boot::{AllocateType, MemoryType};
use uefi::table::cfg;
use x86_64::{
    PhysAddr, VirtAddr,
    structures::paging::{
        FrameAllocator, Mapper, OffsetPageTable, Page, PageTableFlags,
        PhysFrame, Size4KiB, Size2MiB,
    },
    registers::control::{Cr3, Cr3Flags},
};

// ─── Boot info (must match kernel's struct layout exactly) ────────────────────

const NEXUS_MAGIC: u64 = 0x4E455855_4E455855;
const HHDM_BASE:   u64 = 0xFFFF_8000_0000_0000;

#[repr(C)]
struct BootInfo {
    magic:          u64,
    hhdm_offset:    u64,
    framebuffer:    FramebufferInfo,
    memory_map_len: u32,
    _pad:           u32,
    memory_map_ptr: *const MemoryRegion,
}

#[repr(C)]
struct FramebufferInfo {
    phys_addr: u64,
    width:     u32,
    height:    u32,
    pitch:     u32,
    bpp:       u8,
    _pad:      [u8; 3],
}

#[repr(C)]
struct MemoryRegion {
    base:   u64,
    length: u64,
    kind:   u32,
    _pad:   u32,
}

// ─── Entry point ─────────────────────────────────────────────────────────────

#[entry]
fn main(image: Handle, mut st: SystemTable<Boot>) -> Status {
    uefi_services::init(&mut st).expect("uefi-services init failed");

    let bt = st.boot_services();
    log::info!("Noxisa bootloader starting");

    // ── 1. Graphics ───────────────────────────────────────────────────────
    let fb = setup_graphics(bt);
    log::info!("Framebuffer: {}×{} at {:#x}", fb.width, fb.height, fb.phys_addr);

    // ── 2. Load kernel ELF ────────────────────────────────────────────────
    let kernel_bytes = load_file(bt, r"\EFI\Noxisa\kernel.elf");
    log::info!("Kernel ELF: {} bytes", kernel_bytes.len());

    let (kernel_entry, kernel_end) = parse_elf(&kernel_bytes, bt);
    log::info!("Kernel entry: {:#x}", kernel_entry);

    // ── 3. Allocate space for page tables ─────────────────────────────────
    let pt_pages = 16u64; // 64 KiB — enough for initial page tables
    let pt_phys  = bt
        .allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, pt_pages as usize)
        .expect("Failed to allocate page table pages");
    unsafe {
        core::ptr::write_bytes(pt_phys as *mut u8, 0, (pt_pages * 4096) as usize);
    }

    // ── 4. Get ACPI RSDP ──────────────────────────────────────────────────
    let rsdp = st.config_table().iter()
        .find(|e| e.guid == cfg::ACPI2_GUID || e.guid == cfg::ACPI_GUID)
        .map(|e| e.address as u64)
        .unwrap_or(0);

    // ── 5. Allocate BootInfo ──────────────────────────────────────────────
    let boot_info_phys = bt
        .allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, 1)
        .expect("Failed to allocate BootInfo page");

    // ── 6. Get memory map + exit boot services ────────────────────────────
    let mmap_size    = bt.memory_map_size().map_size + 8 * core::mem::size_of::<uefi::table::boot::MemoryDescriptor>();
    let mmap_buf     = bt.allocate_pool(MemoryType::LOADER_DATA, mmap_size)
        .expect("Failed to allocate memory map buffer");
    let mmap_buf_slice = unsafe { core::slice::from_raw_parts_mut(mmap_buf, mmap_size) };

    // Exit boot services — point of no return
    let (_st_runtime, mmap_iter) = st
        .exit_boot_services(image, mmap_buf_slice)
        .expect("exit_boot_services failed");

    // ── 7. Build our memory map from UEFI descriptors ─────────────────────
    // We need to store regions without alloc (boot services are gone).
    // Use a static array — 256 regions covers any real machine.
    static mut REGIONS: [MemoryRegion; 256] = [MemoryRegion {
        base: 0, length: 0, kind: 1 /*Reserved*/, _pad: 0,
    }; 256];
    let mut region_count = 0usize;

    for desc in mmap_iter {
        if region_count >= 256 { break; }
        let kind = match desc.ty {
            MemoryType::CONVENTIONAL => 0u32, // Usable
            MemoryType::ACPI_RECLAIM => 2,
            MemoryType::ACPI_NON_VOLATILE => 3,
            MemoryType::UNUSABLE      => 4,
            MemoryType::LOADER_CODE
            | MemoryType::LOADER_DATA => 5,   // BootloaderData
            _                         => 1,   // Reserved
        };
        unsafe {
            REGIONS[region_count] = MemoryRegion {
                base:   desc.phys_start,
                length: desc.page_count * 4096,
                kind,
                _pad: 0,
            };
        }
        region_count += 1;
    }

    // ── 8. Build page tables ──────────────────────────────────────────────
    // Identity map first 4 GiB so we can keep running after CR3 swap.
    // Map HHDM: all RAM accessible at HHDM_BASE + phys.
    // Map kernel: ELF load address → physical.
    build_page_tables(pt_phys, HHDM_BASE);

    // ── 9. Fill BootInfo ──────────────────────────────────────────────────
    let boot_info = unsafe { &mut *(boot_info_phys as *mut BootInfo) };
    *boot_info = BootInfo {
        magic:          NEXUS_MAGIC,
        hhdm_offset:    HHDM_BASE,
        framebuffer:    fb,
        memory_map_len: region_count as u32,
        _pad:           0,
        memory_map_ptr: unsafe { REGIONS.as_ptr() },
    };

    // ── 10. Switch to our page tables ─────────────────────────────────────
    unsafe {
        Cr3::write(
            PhysFrame::containing_address(PhysAddr::new(pt_phys)),
            Cr3Flags::empty(),
        );
    }

    // ── 11. Jump into the kernel ──────────────────────────────────────────
    type KernelMain = unsafe extern "C" fn(*const BootInfo) -> !;
    let kernel_main: KernelMain = unsafe { core::mem::transmute(kernel_entry) };
    unsafe { kernel_main(boot_info as *const BootInfo) }
}

// ─── Graphics setup ───────────────────────────────────────────────────────────

fn setup_graphics(bt: &BootServices) -> FramebufferInfo {
    let gop_handle = bt.get_handle_for_protocol::<GraphicsOutput>()
        .expect("No GOP handle found");
    let mut gop    = bt.open_protocol_exclusive::<GraphicsOutput>(gop_handle)
        .expect("Failed to open GOP");

    let mode   = gop.current_mode_info();
    let (w, h) = mode.resolution();
    let stride = mode.stride();
    let addr   = gop.frame_buffer().as_mut_ptr() as u64;

    let bpp = match mode.pixel_format() {
        PixelFormat::Rgb | PixelFormat::Bgr => 32,
        _ => 32,
    };

    FramebufferInfo {
        phys_addr: addr,
        width:  w as u32,
        height: h as u32,
        pitch:  (stride * 4) as u32,
        bpp,
        _pad: [0; 3],
    }
}

// ─── File loading ─────────────────────────────────────────────────────────────

fn load_file(bt: &BootServices, path: &str) -> Vec<u8> {
    let fs_handle = bt.get_handle_for_protocol::<SimpleFileSystem>()
        .expect("No filesystem protocol found");
    let mut fs = bt.open_protocol_exclusive::<SimpleFileSystem>(fs_handle)
        .expect("Failed to open SimpleFileSystem");

    let mut root = fs.open_volume().expect("Failed to open root volume");

    // Convert path to UCS-2
    let ucs2: alloc::vec::Vec<u16> = path.encode_utf16().chain(core::iter::once(0)).collect();
    let cstr = uefi::CStr16::from_u16_with_nul(&ucs2).expect("Invalid path");

    let mut file_handle = root
        .open(cstr, FileMode::Read, FileAttribute::empty())
        .expect("Failed to open kernel ELF")
        .into_regular_file()
        .expect("Kernel ELF is not a regular file");

    // Get file size
    let info_buf_size = 512;
    let mut info_buf  = alloc::vec![0u8; info_buf_size];
    let file_size = {
        let info = file_handle
            .get_info::<uefi::proto::media::file::FileInfo>(&mut info_buf)
            .expect("Failed to get file info");
        info.file_size() as usize
    };

    let mut data = alloc::vec![0u8; file_size];
    file_handle.read(&mut data).expect("Failed to read kernel ELF");
    data
}

// ─── ELF loader ───────────────────────────────────────────────────────────────

/// Parse and load an ELF64 binary into memory.
/// Returns (entry_point_virt, end_of_loaded_segments).
fn parse_elf(elf: &[u8], bt: &BootServices) -> (u64, u64) {
    // ELF64 header offsets
    assert!(&elf[0..4] == b"\x7FELF", "Not an ELF binary");
    assert!(elf[4] == 2, "Not ELF64");
    assert!(elf[5] == 1, "Not little-endian ELF");

    let entry_point   = u64::from_le_bytes(elf[24..32].try_into().unwrap());
    let ph_offset     = u64::from_le_bytes(elf[32..40].try_into().unwrap()) as usize;
    let ph_entry_size = u16::from_le_bytes(elf[54..56].try_into().unwrap()) as usize;
    let ph_count      = u16::from_le_bytes(elf[56..58].try_into().unwrap()) as usize;

    let mut end = 0u64;

    for i in 0..ph_count {
        let ph = &elf[ph_offset + i * ph_entry_size..];
        let p_type   = u32::from_le_bytes(ph[0..4].try_into().unwrap());
        if p_type != 1 /* PT_LOAD */ { continue; }

        let p_offset = u64::from_le_bytes(ph[8..16].try_into().unwrap()) as usize;
        let p_vaddr  = u64::from_le_bytes(ph[16..24].try_into().unwrap());
        let p_paddr  = u64::from_le_bytes(ph[24..32].try_into().unwrap());
        let p_filesz = u64::from_le_bytes(ph[32..40].try_into().unwrap()) as usize;
        let p_memsz  = u64::from_le_bytes(ph[40..48].try_into().unwrap()) as usize;

        let pages = (p_memsz + 4095) / 4096;

        // Allocate at the physical address the kernel expects
        let phys = if p_paddr != 0 {
            bt.allocate_pages(
                AllocateType::Address(p_paddr),
                MemoryType::LOADER_CODE,
                pages,
            ).unwrap_or(p_paddr)
        } else {
            bt.allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_CODE, pages)
                .expect("Failed to allocate pages for ELF segment")
        };

        unsafe {
            // Copy file data
            core::ptr::copy_nonoverlapping(
                elf.as_ptr().add(p_offset),
                phys as *mut u8,
                p_filesz,
            );
            // Zero BSS
            if p_memsz > p_filesz {
                core::ptr::write_bytes(
                    (phys as *mut u8).add(p_filesz),
                    0,
                    p_memsz - p_filesz,
                );
            }
        }

        end = end.max(p_vaddr + p_memsz as u64);
    }

    (entry_point, end)
}

// ─── Page table setup ─────────────────────────────────────────────────────────

/// Set up minimal page tables for the kernel.
///
/// Layout (x86-64, 4-level paging, 2 MiB huge pages):
///   - Identity map:  virt 0x0000_0000_0000_0000  →  phys 0x0000_0000_0000_0000  (first 4 GiB)
///   - HHDM map:      virt HHDM_BASE              →  phys 0x0000_0000_0000_0000  (first 4 GiB)
///   - Kernel map:    virt 0xFFFF_FFFF_8000_0000  →  phys 0x0000_0000_0010_0000  (kernel ELF)
///
/// Called with CR3 still pointing at UEFI page tables.
/// After this function the caller must:
///   1. Write pt_phys into CR3
///   2. Exit UEFI boot services
///   3. Jump to kernel_main
fn build_page_tables(pt_phys: u64, hhdm_base: u64) {
    // Page table entry flags
    const PRESENT:   u64 = 1 << 0;
    const WRITABLE:  u64 = 1 << 1;
    const HUGE:      u64 = 1 << 7;  // PS bit — marks 2 MiB pages in PD
    const PAGE_2M:   u64 = 2 * 1024 * 1024;
    const PAGE_4K:   u64 = 4096;

    // ── Helpers to read/write physical memory ──────────────────────────────
    // During bootloader execution UEFI has identity-mapped all RAM,
    // so physical address == virtual address.
    let write_entry = |table_phys: u64, index: usize, value: u64| {
        let ptr = (table_phys + index as u64 * 8) as *mut u64;
        unsafe { ptr.write_volatile(value); }
    };
    let alloc_table = |base: u64, offset: u64| -> u64 {
        // Tables are laid out contiguously starting at pt_phys.
        // PML4 @ base+0, PDPT_low @ base+4K, PD_low @ base+8K,
        // PDPT_hhdm @ base+12K, PD_hhdm @ base+16K
        // (HHDM needs its own PDPT + one PD per GiB; we use 4 PDs)
        base + offset * PAGE_4K
    };

    // Table layout (each table = 4 KiB = 512 × 8-byte entries):
    //   Offset 0  → PML4       (root)
    //   Offset 1  → PDPT_low   (identity map, covers 0..512 GiB via [0])
    //   Offset 2  → PD_low_0   (identity 2 MiB pages, GiB 0)
    //   Offset 3  → PD_low_1   (identity 2 MiB pages, GiB 1)
    //   Offset 4  → PD_low_2   (identity 2 MiB pages, GiB 2)
    //   Offset 5  → PD_low_3   (identity 2 MiB pages, GiB 3)
    //   Offset 6  → PDPT_hhdm  (HHDM, same 4 GiB)
    //   Offset 7  → (reuse PD_low_* for HHDM — same physical mapping)
    //   Offset 8  → PDPT_kern  (kernel higher-half, PML4[511])
    //   Offset 9  → PD_kern    (kernel 2 MiB pages)

    let pml4      = alloc_table(pt_phys, 0);
    let pdpt_low  = alloc_table(pt_phys, 1);
    let pdpt_hhdm = alloc_table(pt_phys, 6);
    let pdpt_kern = alloc_table(pt_phys, 8);
    let pd_kern   = alloc_table(pt_phys, 9);

    // ── PML4 entries ───────────────────────────────────────────────────────
    // [0]   → identity map (low 512 GiB)
    write_entry(pml4, 0,   pdpt_low  | PRESENT | WRITABLE);
    // [hhdm_pml4_idx] → HHDM
    let hhdm_pml4_idx = ((hhdm_base >> 39) & 0x1FF) as usize;
    write_entry(pml4, hhdm_pml4_idx, pdpt_hhdm | PRESENT | WRITABLE);
    // [511] → kernel higher-half (0xFFFF_FFFF_8000_0000)
    write_entry(pml4, 511, pdpt_kern | PRESENT | WRITABLE);

    // ── Identity PDPT [0..3] → PD per GiB ─────────────────────────────────
    for gib in 0u64..4 {
        let pd_phys = alloc_table(pt_phys, 2 + gib);
        write_entry(pdpt_low, gib as usize, pd_phys | PRESENT | WRITABLE);

        // Fill PD with 512 × 2 MiB entries covering that GiB
        for mb2 in 0u64..512 {
            let phys = gib * 1024 * 1024 * 1024 + mb2 * PAGE_2M;
            write_entry(pd_phys, mb2 as usize, phys | PRESENT | WRITABLE | HUGE);
        }

        // HHDM PDPT — point at the same PDs (same physical mapping)
        let hhdm_pdpt_idx = ((hhdm_base >> 30) & 0x1FF) as usize + gib as usize;
        write_entry(pdpt_hhdm, hhdm_pdpt_idx & 0x1FF, pd_phys | PRESENT | WRITABLE);
    }

    // ── Kernel higher-half mapping ──────────────────────────────────────────
    // virt 0xFFFF_FFFF_8000_0000 → phys 0x0010_0000 (where we loaded kernel.elf)
    // PML4[511] → pdpt_kern; PDPT[510] → pd_kern
    write_entry(pdpt_kern, 510, pd_kern | PRESENT | WRITABLE);
    // Map 128 MiB of kernel space (64 × 2 MiB) — more than enough for the kernel
    for mb2 in 0u64..64 {
        let phys = 0x0010_0000_u64 + mb2 * PAGE_2M;
        write_entry(pd_kern, mb2 as usize, phys | PRESENT | WRITABLE | HUGE);
    }
}
