//! Interrupt Descriptor Table — exception and IRQ handlers

use x86_64::structures::idt::{
    InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode,
};
use lazy_static::lazy_static;

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();

        // CPU exceptions
        idt.divide_error.set_handler_fn(divide_error);
        idt.debug.set_handler_fn(debug_handler);
        idt.breakpoint.set_handler_fn(breakpoint);
        idt.overflow.set_handler_fn(overflow);
        idt.invalid_opcode.set_handler_fn(invalid_opcode);
        idt.general_protection_fault.set_handler_fn(gpf);
        idt.page_fault.set_handler_fn(page_fault);
        // Double-fault uses IST slot 0 (separate stack from GDT setup).
        unsafe {
            idt.double_fault.set_handler_fn(double_fault).set_stack_index(0);
        }
        idt[0x20].set_handler_fn(irq0_timer);
        idt[0x21].set_handler_fn(irq1_keyboard);

        idt
    };
}

/// Load the IDT. Call after GDT is set up.
pub fn init() {
    IDT.load();
    // Interrupts stay disabled until APIC is configured (milestone 1.x)
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

extern "x86-interrupt" fn divide_error(f: InterruptStackFrame) {
    panic!("#DE divide error at {:#x}", f.instruction_pointer);
}

extern "x86-interrupt" fn debug_handler(f: InterruptStackFrame) {
    crate::kprintln!("[INT] #DB debug at {:#x}", f.instruction_pointer);
}

extern "x86-interrupt" fn breakpoint(f: InterruptStackFrame) {
    crate::kprintln!("[INT] #BP breakpoint at {:#x}", f.instruction_pointer);
}

extern "x86-interrupt" fn overflow(f: InterruptStackFrame) {
    panic!("#OF overflow at {:#x}", f.instruction_pointer);
}

extern "x86-interrupt" fn invalid_opcode(f: InterruptStackFrame) {
    panic!("#UD invalid opcode at {:#x}", f.instruction_pointer);
}

extern "x86-interrupt" fn gpf(f: InterruptStackFrame, code: u64) {
    panic!("#GP error={:#x} at {:#x}", code, f.instruction_pointer);
}

extern "x86-interrupt" fn page_fault(
    f:    InterruptStackFrame,
    code: PageFaultErrorCode,
) {
    let cr2 = x86_64::registers::control::Cr2::read();
    let cr3_now: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3_now, options(nomem, nostack)); }
    panic!("#PF accessing {:#x} ({:?}) at {:#x}  [cr3={:#x}]",
        cr2.as_u64(), code, f.instruction_pointer, cr3_now);
}

extern "x86-interrupt" fn irq0_timer(_f: InterruptStackFrame) {
    // Increment tick counter atomically
    crate::drivers::pit::tick();
    // EOI to PIC1 — sent BEFORE attempting a preemptive switch below,
    // not after. try_preempt() may not return for a long time (control
    // can jump straight into a different task and only unwind back
    // through this call once this exact pid is rescheduled, possibly
    // several timer periods later). Sending EOI after instead would
    // leave IRQ0 marked in-service at the PIC the whole time some other
    // task runs - one single preemption, then silence, forever.
    unsafe {
        core::arch::asm!(
            "mov al, 0x20",
            "out 0x20, al",
            options(nomem, nostack, preserves_flags)
        );
    }
    crate::scheduler::Scheduler::get().try_preempt();
}

extern "x86-interrupt" fn irq1_keyboard(_f: InterruptStackFrame) {
    crate::drivers::keyboard::handle_irq();
    unsafe {
        core::arch::asm!(
            "mov al, 0x20",
            "out 0x20, al",
            options(nomem, nostack, preserves_flags)
        );
    }
}

extern "x86-interrupt" fn double_fault(f: InterruptStackFrame, _: u64) -> ! {
    panic!("#DF double fault at {:#x}", f.instruction_pointer);
}
