use linked_list_allocator::LockedHeap;

// Put heap well above kernel (loads at 0x100000, ~1MB image)
// 0x500000 = 5MB — safe gap
pub const HEAP_START: usize = 0x00A0_0000;
pub const HEAP_SIZE:  usize = 2 * 1024 * 1024; // 2 MiB

pub fn init(heap: &LockedHeap) {
    // Identity mapped by boot.s — no frame allocation needed
    unsafe {
        heap.lock().init(HEAP_START as *mut u8, HEAP_SIZE);
    }
}
