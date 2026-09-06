//! GDT — Global Descriptor Table
//!
//! Minimal 64-bit GDT required for the kernel:
//!   Selector 0x00 — null
//!   Selector 0x08 — kernel code (ring 0, 64-bit)
//!   Selector 0x10 — kernel data (ring 0)
//!   Selector 0x18 — user code  (ring 3, 64-bit) [for future syscalls]
//!   Selector 0x20 — user data  (ring 3)
//!   Selector 0x28 — TSS (takes 16 bytes in 64-bit mode)

use x86_64::{
    instructions::{
        segmentation::{CS, DS, ES, SS, Segment},
        tables::load_tss,
    },
    structures::{
        gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector},
        tss::TaskStateSegment,
    },
    VirtAddr,
};
use lazy_static::lazy_static;

/// Stack used exclusively by the double-fault handler (via IST slot 0).
/// Must be static — the CPU jumps to it unconditionally on double fault.
static mut DF_STACK: [u8; 4096 * 5] = [0; 4096 * 5];
static mut RSP0_STACK: [u8; 4096 * 5] = [0; 4096 * 5];

lazy_static! {
    static ref TSS: TaskStateSegment = {
        let mut tss = TaskStateSegment::new();
        tss.interrupt_stack_table[0] = unsafe {
            let top = DF_STACK.as_ptr().add(DF_STACK.len());
            VirtAddr::new(top as u64)
        };
        tss.privilege_stack_table[0] = unsafe {
            let top = RSP0_STACK.as_ptr().add(RSP0_STACK.len());
            VirtAddr::new(top as u64)
        };
        tss
    };

    static ref GDT_DATA: (GlobalDescriptorTable, [SegmentSelector; 5]) = {
        let mut gdt = GlobalDescriptorTable::new();
        let kcode = gdt.add_entry(Descriptor::kernel_code_segment());
        let kdata = gdt.add_entry(Descriptor::kernel_data_segment());
        let udata = gdt.add_entry(Descriptor::user_data_segment());
        let ucode = gdt.add_entry(Descriptor::user_code_segment());
        let tss_s = gdt.add_entry(Descriptor::tss_segment(&TSS));
        (gdt, [kcode, kdata, tss_s, udata, ucode])
    };
}

/// Load the GDT, set all segment registers, and load the TSS.
pub fn init() {
    GDT_DATA.0.load();
    unsafe {
        CS::set_reg(GDT_DATA.1[0]);
        DS::set_reg(GDT_DATA.1[1]);
        ES::set_reg(GDT_DATA.1[1]);
        SS::set_reg(GDT_DATA.1[1]);
        load_tss(GDT_DATA.1[2]);
    }
}

/// Returns user code selector (ring 3)
pub fn user_code_selector() -> SegmentSelector { GDT_DATA.1[4] }
/// Returns user data selector (ring 3)
pub fn user_data_selector() -> SegmentSelector { GDT_DATA.1[3] }
