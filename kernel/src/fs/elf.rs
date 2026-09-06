//! Minimal ELF64 loader — parses and loads static executables into memory
extern crate alloc;

/// ELF64 header (first 64 bytes of every ELF file)
#[repr(C, packed)]
struct Elf64Header {
    magic:      [u8; 4],   // 0x7F 'E' 'L' 'F'
    class:      u8,        // 2 = 64-bit
    data:       u8,        // 1 = little-endian
    version:    u8,
    os_abi:     u8,
    _pad:       [u8; 8],
    e_type:     u16,       // 2 = ET_EXEC
    machine:    u16,       // 0x3E = x86-64
    e_version:  u32,
    entry:      u64,       // entry point virtual address
    phoff:      u64,       // program header table offset
    shoff:      u64,
    flags:      u32,
    ehsize:     u16,
    phentsize:  u16,       // size of one program header
    phnum:      u16,       // number of program headers
    shentsize:  u16,
    shnum:      u16,
    shstrndx:   u16,
}

/// ELF64 program header
#[repr(C, packed)]
struct Elf64Phdr {
    p_type:   u32,   // 1 = PT_LOAD
    p_flags:  u32,   // R/W/X bits
    p_offset: u64,   // offset in file
    p_vaddr:  u64,   // virtual address to load at
    p_paddr:  u64,
    p_filesz: u64,   // bytes in file
    p_memsz:  u64,   // bytes in memory (may be > filesz, rest = 0)
    p_align:  u64,
}

#[derive(Debug)]
pub enum ElfError {
    TooSmall,
    BadMagic,
    Not64Bit,
    NotExecutable,
    NotX86_64,
    SegmentOutOfBounds,
}

/// Load an ELF64 binary from `data` into memory.
/// Returns the entry point virtual address on success.
/// 
/// # Safety
/// Writes to virtual addresses specified in the ELF — caller must ensure
/// those addresses are mapped and writable.
pub unsafe fn load(data: &[u8]) -> Result<u64, ElfError> {
    if data.len() < 64 { return Err(ElfError::TooSmall); }

    let hdr = &*(data.as_ptr() as *const Elf64Header);

    // Validate magic
    if hdr.magic != [0x7F, b'E', b'L', b'F'] {
        return Err(ElfError::BadMagic);
    }
    if hdr.class != 2   { return Err(ElfError::Not64Bit); }
    if hdr.e_type != 2  { return Err(ElfError::NotExecutable); }
    if hdr.machine != 0x3E { return Err(ElfError::NotX86_64); }

    let entry    = hdr.entry;
    let phoff    = hdr.phoff    as usize;
    let phnum    = hdr.phnum    as usize;
    let phentsz  = hdr.phentsize as usize;

    // Walk program headers, load PT_LOAD segments
    for i in 0..phnum {
        let ph_off = phoff + i * phentsz;
        if ph_off + phentsz > data.len() {
            return Err(ElfError::SegmentOutOfBounds);
        }
        let ph = &*(data.as_ptr().add(ph_off) as *const Elf64Phdr);

        if ph.p_type != 1 { continue; } // only PT_LOAD

        let file_off = ph.p_offset as usize;
        let filesz   = ph.p_filesz as usize;
        let memsz    = ph.p_memsz  as usize;
        let vaddr    = ph.p_vaddr  as *mut u8;

        if file_off + filesz > data.len() {
            return Err(ElfError::SegmentOutOfBounds);
        }

        // Copy file bytes
        core::ptr::copy_nonoverlapping(
            data.as_ptr().add(file_off),
            vaddr,
            filesz,
        );

        // Zero the BSS portion (memsz > filesz)
        if memsz > filesz {
            core::ptr::write_bytes(vaddr.add(filesz), 0, memsz - filesz);
        }
    }

    Ok(entry)
}

/// Load an ELF64 binary into FRESH physical frames, one per page touched by
/// any PT_LOAD segment, rather than writing directly to whatever is mapped
/// at the segment's own vaddr in the CURRENT address space.
///
/// Returns (entry point, Vec<(vaddr_page, phys_frame)>).
///
/// # Safety
/// Caller is responsible for eventually freeing the returned physical
/// frames (e.g. via reap()/teardown_isolated_process()).
pub unsafe fn load_fresh(data: &[u8]) -> Result<(u64, alloc::vec::Vec<(u64, u64, bool, bool)>), ElfError> {
    if data.len() < 64 { return Err(ElfError::TooSmall); }

    let hdr = &*(data.as_ptr() as *const Elf64Header);
    if hdr.magic != [0x7F, b'E', b'L', b'F'] { return Err(ElfError::BadMagic); }
    if hdr.class != 2   { return Err(ElfError::Not64Bit); }
    if hdr.e_type != 2  { return Err(ElfError::NotExecutable); }
    if hdr.machine != 0x3E { return Err(ElfError::NotX86_64); }

    let entry   = hdr.entry;
    let phoff   = hdr.phoff    as usize;
    let phnum   = hdr.phnum    as usize;
    let phentsz = hdr.phentsize as usize;

    const PAGE_SIZE: u64 = 4096;
    let mut pages: alloc::vec::Vec<(u64, u64, bool, bool)> = alloc::vec::Vec::new();

    for i in 0..phnum {
        let ph_off = phoff + i * phentsz;
        if ph_off + phentsz > data.len() {
            return Err(ElfError::SegmentOutOfBounds);
        }
        let ph = &*(data.as_ptr().add(ph_off) as *const Elf64Phdr);
        if ph.p_type != 1 { continue; } // only PT_LOAD

        let file_off = ph.p_offset as usize;
        let filesz   = ph.p_filesz as usize;
        let memsz    = ph.p_memsz  as usize;
        let vaddr    = ph.p_vaddr;
        // PF_X = 1, PF_W = 2, PF_R = 4 (standard ELF p_flags bits) - thread
        // the segment's real R/W/X permissions through instead of hardcoding
        // writable+executable for every page. Closes the W^X gap noted in
        // Part 2/Part 4 of the roadmap doc for generic process spawning.
        let seg_writable   = (ph.p_flags & 0x2) != 0;
        let seg_executable = (ph.p_flags & 0x1) != 0;

        if file_off + filesz > data.len() {
            return Err(ElfError::SegmentOutOfBounds);
        }

        let seg_start = vaddr & !(PAGE_SIZE - 1);
        let seg_end   = (vaddr + memsz as u64 + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let mut page_vaddr = seg_start;
        while page_vaddr < seg_end {
            if !pages.iter().any(|&(v, _, _, _)| v == page_vaddr) {
                let frame = crate::memory::pmm::alloc_frame()
                    .ok_or(ElfError::SegmentOutOfBounds)?;
                core::ptr::write_bytes(
                    crate::memory::paging::phys_to_virt(frame) as *mut u8, 0, PAGE_SIZE as usize,
                );
                pages.push((page_vaddr, frame, seg_writable, seg_executable));
            }
            page_vaddr += PAGE_SIZE;
        }

        let mut copied = 0usize;
        while copied < filesz {
            let cur_vaddr = vaddr + copied as u64;
            let page_base = cur_vaddr & !(PAGE_SIZE - 1);
            let page_off  = (cur_vaddr - page_base) as usize;
            let frame = pages.iter().find(|&&(v, _, _, _)| v == page_base).unwrap().1;
            let dst = (crate::memory::paging::phys_to_virt(frame) as usize + page_off) as *mut u8;
            let chunk = core::cmp::min(filesz - copied, PAGE_SIZE as usize - page_off);
            core::ptr::copy_nonoverlapping(data.as_ptr().add(file_off + copied), dst, chunk);
            copied += chunk;
        }
    }

    Ok((entry, pages))
}

/// Validate ELF header without loading — returns entry point or error.
pub fn validate(data: &[u8]) -> Result<u64, ElfError> {
    if data.len() < 64 { return Err(ElfError::TooSmall); }
    let hdr = unsafe { &*(data.as_ptr() as *const Elf64Header) };
    if hdr.magic != [0x7F, b'E', b'L', b'F'] { return Err(ElfError::BadMagic); }
    if hdr.class   != 2    { return Err(ElfError::Not64Bit); }
    if hdr.e_type  != 2    { return Err(ElfError::NotExecutable); }
    if hdr.machine != 0x3E { return Err(ElfError::NotX86_64); }
    Ok(hdr.entry)
}
