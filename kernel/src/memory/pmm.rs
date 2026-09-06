use spin::Mutex;
extern crate alloc;
use alloc::vec::Vec;

pub const PAGE_SIZE: u64 = 4096;
const MAX_REGIONS: usize = 32;

#[derive(Copy, Clone)]
struct Region { base: u64, end: u64 }

struct Pmm {
    regions:   [Region; MAX_REGIONS],
    count:     usize,
    cur:       usize,
    next:      u64,
    free:      u64,
    free_list: Vec<u64>,  // scrubbed frames available for reuse, LIFO
}

static PMM: Mutex<Pmm> = Mutex::new(Pmm {
    regions: [Region { base: 0, end: 0 }; MAX_REGIONS],
    count: 0, cur: 0, next: 0, free: 0,
    free_list: Vec::new(),
});

pub fn add_region(base: u64, len: u64) {
    let mut p = PMM.lock();
    if p.count >= MAX_REGIONS { return; }
    let aligned_base = (base + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let aligned_end  = (base + len) & !(PAGE_SIZE - 1);
    if aligned_end <= aligned_base { return; }
    p.free += (aligned_end - aligned_base) / PAGE_SIZE;
    let idx = p.count;
    p.regions[idx] = Region { base: aligned_base, end: aligned_end };
    p.count += 1;
}

pub fn init() {
    let mut p = PMM.lock();
    for i in 0..p.count {
        if p.regions[i].end > 0x100000 {
            p.cur  = i;
            p.next = p.regions[i].base.max(0x300000); // skip kernel image
            return;
        }
    }
}

/// Allocate a physical frame. Prefers a scrubbed (zeroed) frame from the
/// free list over bumping `next` forward - this is what makes real reuse,
/// and therefore real scrubbing, provable rather than just claimed.
pub fn alloc_frame() -> Option<u64> {
    let mut p = PMM.lock();
    if let Some(addr) = p.free_list.pop() {
        p.free -= 1;
        return Some(addr);
    }
    loop {
        if p.cur >= p.count { return None; }
        let end  = p.regions[p.cur].end;
        let next = p.next;
        if next + PAGE_SIZE <= end {
            p.next += PAGE_SIZE;
            if p.free > 0 { p.free -= 1; }
            return Some(next);
        }
        p.cur += 1;
        if p.cur < p.count {
            p.next = p.regions[p.cur].base;
        }
    }
}

/// Return a physical frame to the pool. Zeroes its full contents via the
/// HHDM mapping BEFORE it goes back on the free list - a frame is never
/// observably reusable with its previous owner's data still in it. This is
/// the same "scrub, don't just leak" principle as the heap allocator's
/// zero-on-free, applied one level down at the physical frame layer.
/// Allocate `n` PHYSICALLY CONTIGUOUS frames, bypassing the free-list
/// entirely (bump-pointer only, matching alloc_frame()'s own bump-path
/// logic). Used for kernel-mode stacks, which are addressed as a single
/// flat virtual range via the straight HHDM 1:1 offset and therefore
/// require REAL physical contiguity - `n` individually-valid but
/// possibly-scattered frames from the free-list's LIFO reuse is not
/// good enough. Calling plain alloc_frame() n times here is exactly what
/// silently produced non-contiguous "stacks" that could alias unrelated
/// live allocations (Part 4 bug #18 - the PMM double-free this fixes).
pub fn alloc_contiguous_frames(n: usize) -> Option<u64> {
    let mut p = PMM.lock();
    let need = n as u64 * PAGE_SIZE;
    loop {
        if p.cur >= p.count { return None; }
        let end = p.regions[p.cur].end;
        let start = p.next;
        if start + need <= end {
            p.next += need;
            if p.free >= n as u64 { p.free -= n as u64; } else { p.free = 0; }
            return Some(start);
        }
        p.cur += 1;
        if p.cur < p.count {
            p.next = p.regions[p.cur].base;
        }
    }
}

pub fn free_frame(addr: u64) {
    unsafe {
        let va = crate::memory::paging::phys_to_virt(addr) as *mut u8;
        core::ptr::write_bytes(va, 0, PAGE_SIZE as usize);
    }
    let mut p = PMM.lock();
    // Double-free guard: catch a frame being freed while it's already
    // sitting on the free-list, immediately and loudly, rather than
    // silently planting a duplicate entry that a LATER, unrelated
    // alloc_frame() call could pop and hand out to two different
    // logical owners at once - exactly the kind of corruption this
    // project's own hardened allocator philosophy (Part 2) exists to
    // catch at the point of the actual bug, not several calls later
    // when the symptom finally surfaces as something else entirely.
    if p.free_list.contains(&addr) {
        panic!("PMM double-free: frame {:#x} is already on the free-list", addr);
    }
    p.free_list.push(addr);
    p.free += 1;
}

pub fn free_frames() -> u64 { PMM.lock().free }
