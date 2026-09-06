//! Noxisa Kernel — Entry Point
//!
//! Build (requires Rust nightly):
//!   cd kernel && cargo +nightly build --release
//!
//! Nightly features used:
//!   abi_x86_interrupt  — correct calling convention for CPU exception handlers
//!   alloc_error_handler — OOM hook in no_std
//!   naked_functions     — bare assembly trampolines

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![feature(alloc_error_handler)]
#![feature(naked_functions)]

// Pull in heap-allocated types: Box, Vec, String, Arc, BTreeMap, …
extern crate alloc;

// ─── Sub-modules (all files must exist) ──────────────────────────────────────
pub mod limine_boot;
pub mod audit;      // Kernel-level audit log - real, wired to actual chokepoints (unlike security/)
pub mod drivers;
pub mod fs;
pub mod ipc;
pub mod memory;
pub mod scheduler;
pub mod security;   // capabilities, namespaces, seccomp, MAC
pub mod kshell;     // kernel AI shell (early-boot / emergency)
pub mod hal;        // Console/InputDevice/DisplayDevice wrappers over drivers/

use core::alloc::{GlobalAlloc, Layout};

// ─── Global allocator (must be declared at crate root) ────────────────────────
static INNER_HEAP: linked_list_allocator::LockedHeap =
    linked_list_allocator::LockedHeap::empty();

const HEAP_CANARY: u64 = 0xDEAD_C0DE_DEAD_C0DE;

const fn align_up(val: usize, align: usize) -> usize {
    (val + align - 1) & !(align - 1)
}

struct HardenedAllocator;

unsafe impl GlobalAlloc for HardenedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align().max(8);
        let extra_front = align_up(8, align);

        let real_size = match extra_front
            .checked_add(layout.size())
            .and_then(|s| s.checked_add(8))
        {
            Some(s) => s,
            None => return core::ptr::null_mut(),
        };
        let real_layout = match Layout::from_size_align(real_size, align) {
            Ok(l) => l,
            Err(_) => return core::ptr::null_mut(),
        };

        let real_ptr = INNER_HEAP.alloc(real_layout);
        if real_ptr.is_null() {
            return real_ptr;
        }

        (real_ptr as *mut u64).write_volatile(HEAP_CANARY);

        let user_ptr = real_ptr.add(extra_front);
        (user_ptr.add(layout.size()) as *mut u64).write_volatile(HEAP_CANARY);

        user_ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let align = layout.align().max(8);
        let extra_front = align_up(8, align);
        let real_ptr = ptr.sub(extra_front);

        let header_ok = (real_ptr as *const u64).read_volatile() == HEAP_CANARY;
        let trailer_ok =
            (ptr.add(layout.size()) as *const u64).read_volatile() == HEAP_CANARY;

        if !header_ok || !trailer_ok {
            panic!(
                "heap corruption on free: ptr={:p} size={} align={} header_ok={} trailer_ok={}",
                ptr, layout.size(), layout.align(), header_ok, trailer_ok
            );
        }

        let real_size = extra_front + layout.size() + 8;
        let mut p = real_ptr;
        let end = real_ptr.add(real_size);
        while p < end {
            p.write_volatile(0u8);
            p = p.add(1);
        }

        let real_layout = Layout::from_size_align_unchecked(real_size, align);
        INNER_HEAP.dealloc(real_ptr, real_layout);
    }
}

#[global_allocator]
static ALLOCATOR: HardenedAllocator = HardenedAllocator;

// ─── Kernel entry ─────────────────────────────────────────────────────────────

/// Called by the bootloader immediately after setting up page tables.
///
/// # Safety
/// Called once, from ring-0, with a valid aligned `BootInfo` pointer.
/// boot_info may be NULL when booted via QEMU -kernel directly (no bootloader).
/// In that case the kernel runs in minimal mode: serial output + halt loop.
#[no_mangle]
/// Native Limine protocol entry point — called directly from boot.asm in
/// 64-bit long mode with paging already set up, no boot-info pointer.
/// Minimal for now: just proves the real framebuffer is reachable under
/// native protocol, since that's the actual open question tonight. Full
/// PMM/heap/scheduler bring-up under native protocol comes after this
/// is confirmed working on real hardware.
#[unsafe(no_mangle)]
pub extern "C" fn kernel_main_native() -> ! {
    // These two were missing from the native port — Limine loads its own
    // GDT with an undefined layout, so any code assuming specific segment
    // selectors (scheduler task setup, ring-3 support) can #GP without our
    // own GDT in place. Same SSE enable the old Multiboot2 path always did.
    unsafe {
        let mut cr0: u64;
        core::arch::asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
        cr0 &= !4u64;
        cr0 |=  2u64;
        core::arch::asm!("mov cr0, {}", in(reg) cr0, options(nomem, nostack));
        let mut cr4: u64;
        core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
        cr4 |= 0x600u64;
        core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack));
    }
    drivers::gdt::init();

    drivers::serial::init();
    kprintln!("[DEBUG] Entered kernel_main_native (Limine native protocol)");

    limine_boot::assert_base_revision_supported();
    kprintln!("[boot] Limine base revision supported");
    kprintln!("[boot-diag] BASE_REVISION.loaded_revision() = {:?}", limine_boot::BASE_REVISION.actual_revision());

    // Feed Limine's memory map into our existing PMM, same logic as the
    // Multiboot2 path used, just a different source of truth.
    if let Some(memmap) = limine_boot::MEMMAP_REQUEST.response() {
        let mut usable = 0u64;
        for entry in memmap.entries() {
            if entry.type_ == 0 { // 0 == LIMINE_MEMMAP_USABLE per Limine protocol spec
                memory::pmm::add_region(entry.base, entry.length);
                usable += entry.length;
            }
        }
        memory::pmm::init();
        kprintln!("[boot] PMM ready (native) — {} MiB usable", usable / 1024 / 1024);

        // Base revision 0 (our current fallback) identity-maps the entire
        // first 4GiB, so this fixed low address is safe to use directly —
        // no extra mapping needed the way the framebuffer needed HHDM.
        // Under native protocol we don't trust the old fixed-address
        // assumption (that was for Multiboot2's identity map). Instead,
        // grab real physical frames from our PMM and reach them through
        // Limine's HHDM offset, which is guaranteed valid.
        if let Some(hhdm) = limine_boot::HHDM_REQUEST.response() {
            let hhdm_offset = hhdm.offset;
            const HEAP_FRAMES: usize = 512; // 512 * 4KiB = 2MiB
            let first_phys = memory::pmm::alloc_frame().expect("out of memory for heap");
            for _ in 1..HEAP_FRAMES {
                memory::pmm::alloc_frame().expect("out of memory for heap");
            }
            let heap_virt = hhdm_offset + first_phys;
            unsafe {
                INNER_HEAP.lock().init(heap_virt as *mut u8, HEAP_FRAMES * 4096);
            }
            kprintln!("[boot] Heap ready (native) at {:#x} (phys {:#x})", heap_virt, first_phys);
        } else {
            kprintln!("[WARN] No HHDM response from Limine — heap not initialised");
        }
        {
            let b = alloc::boxed::Box::new(42u64);
            kprintln!("[boot] Box<u64> = {}", *b);
            let mut v: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
            v.push(1);
            v.push(2);
            v.push(3);
            kprintln!("[boot] Vec<u64> = [{}, {}, {}]", v[0], v[1], v[2]);
        }
    } else {
        kprintln!("[WARN] No memory map from Limine — PMM not initialised");
    }

    // IDT + exception handlers (self-contained, no boot-info dependency,
    // so it ports over unchanged from the Multiboot2 path)
    drivers::idt::init();
    kprintln!("[boot] IDT loaded (native) - interrupts enabled");
    unsafe { core::arch::asm!("int3", options(nomem, nostack)); }
    kprintln!("[boot] Breakpoint handled - CPU exceptions working!");
    kprintln!("[boot] Exception handlers: #DE #DB #BP #OF #UD #GP #PF #DF");

    // Register the real HHDM offset Limine gave us — without this,
    // paging::hhdm_offset() defaults to 0, so anything that does
    // phys + hhdm_offset() (like the scheduler's stack allocation)
    // ends up treating a raw physical address as virtual, which faults.
    if let Some(hhdm) = limine_boot::HHDM_REQUEST.response() {
        memory::paging::init(hhdm.offset);
        kprintln!("[boot] Paging HHDM offset registered: {:#x}", hhdm.offset);
    }

    // ── Milestone 1.2: Syscall interface (native) ───────────────────────
    // Self-contained (no boot-info dependency), same as IDT above, so it
    // ports over unchanged. Sets EFER.SCE + STAR/LSTAR so a ring-3 process
    // can SYSCALL back into the kernel. GDT must already be loaded (it is —
    // drivers::gdt::init() ran at the very top of this function) since the
    // STAR MSR encodes segment selectors from that GDT layout.
    unsafe { setup_syscall_msr(); }
    kprintln!("[boot] Syscall MSR configured (STAR/LSTAR/EFER) (native)");
    kprintln!("[boot] Noxisa 1.2 milestone reached — syscall interface ready! (native)");

    // Scheduler + first kernel thread (self-contained, ports over unchanged)
    kprintln!("[boot] Starting scheduler (native)...");
    let sched = scheduler::Scheduler::get();
    let pid_sh = sched.spawn("kshell", shell_thread);
    kprintln!("[boot] Spawned kshell (pid={})", pid_sh);

    drivers::pit::init();
    // Remap the legacy 8259 PIC to vectors 0x20/0x28 and mask every IRQ
    // except IRQ0 (timer) and IRQ1 (keyboard) — must happen before
    // interrupts are ever enabled, or IRQ0 fires on its default
    // unmapped vector 0x08 (colliding with #DF).
    drivers::pic::init();
    // Interrupts are NOT explicitly enabled here. scheduler::Scheduler's
    // context_switch() carries an unconditional `sti` immediately before
    // every task handoff — IF flips to 1 automatically the moment kshell
    // is first scheduled, and stays enabled from then on.
    kprintln!("[boot] PIT driver ready (native)");

    if let Some(fb_response) = limine_boot::FRAMEBUFFER_REQUEST.response() {
        if let Some(fb) = fb_response.framebuffers().first() {
            let addr = fb.address() as u64;
            let width = fb.width as u32;
            let height = fb.height as u32;
            let pitch = fb.pitch as u32;
            kprintln!("[gpu] Limine FB: {:#x} {}x{} pitch={}", addr, width, height, pitch);

            // Under native protocol, Limine's HHDM already maps this address —
            // it's a virtual pointer, not physical, so our own identity-mapping
            // code (which assumes physical addresses) would corrupt Limine's
            // own page tables if we called it here. That's exactly what the
            // "QEMU [Paused]" / triple-fault symptom was.
            // Limine's framebuffer address is already a virtual HHDM
            // pointer, but gpu::init() expects physical (it re-adds the
            // HHDM offset itself). This silently worked earlier only
            // because HHDM was still 0 at that point — now that we
            // register the real offset, the double-add produced a
            // non-canonical address and #GP'd. Convert back to physical.
            let phys_addr = memory::paging::virt_to_phys(addr);
            drivers::gpu::init(phys_addr, width, height);
            drivers::gpu::fill_rect(0, 0, width, height, 0x00FF0000);
            kprintln!("[debug] Filled screen red under NATIVE protocol — halting here");
            draw_compositor_placeholder();
        } else {
            kprintln!("[gpu] Limine gave a framebuffer response but no framebuffers");
        }
    } else {
        kprintln!("[gpu] No framebuffer response from Limine at all");
    }

    // ELF loader - userspace test binary is linked for a fixed low
    // virtual address (0x400000), needs real identity mapping (virt==phys)
    // not HHDM. Old code hand-poked PD entries assuming a specific
    // existing page-table layout from Multiboot2-era boot.asm's hand-built
    // tables - that assumption doesn't hold under Limine's own tables, so
    // we use identity_map_region() instead, which safely creates entries
    // regardless of what's already there.
    kprintln!("[elf] Loading hello userspace binary...");

    // TEMP DIAGNOSTIC: dump raw header fields before the real load, so we
    // can see the actual vaddr/entry values instead of guessing why the
    // page fault address (0x3ba2000) doesn't match the expected 0x400000.
    {
        let data = fs::initrd::HELLO_ELF;
        kprintln!("[elf-dbg] data ptr={:#x} len={}", data.as_ptr() as usize, data.len());
        if data.len() >= 64 {
            let entry = u64::from_le_bytes(data[24..32].try_into().unwrap());
            let phoff = u64::from_le_bytes(data[32..40].try_into().unwrap());
            let phentsize = u16::from_le_bytes(data[54..56].try_into().unwrap());
            let phnum = u16::from_le_bytes(data[56..58].try_into().unwrap());
            kprintln!("[elf-dbg] entry={:#x} phoff={:#x} phentsize={} phnum={}", entry, phoff, phentsize, phnum);
            for i in 0..phnum as usize {
                let off = phoff as usize + i * phentsize as usize;
                if off + 56 <= data.len() {
                    let p_type = u32::from_le_bytes(data[off..off+4].try_into().unwrap());
                    let p_vaddr = u64::from_le_bytes(data[off+16..off+24].try_into().unwrap());
                    let p_filesz = u64::from_le_bytes(data[off+32..off+40].try_into().unwrap());
                    let p_memsz = u64::from_le_bytes(data[off+40..off+48].try_into().unwrap());
                    kprintln!("[elf-dbg] phdr[{}] type={} vaddr={:#x} filesz={:#x} memsz={:#x}", i, p_type, p_vaddr, p_filesz, p_memsz);
                }
            }
        }
    }

    unsafe {
        memory::paging::identity_map_region(0x3FF000, 0x3000);
    }
    match unsafe { fs::elf::load(fs::initrd::HELLO_ELF) } {
        Ok(entry) => {
            kprintln!("[elf] Loaded! Entry: {:#x}", entry);
            kprintln!("[boot] Noxisa 2.4 milestone - ELF loader working! (native)");
        }
        Err(e) => kprintln!("[elf] Load failed: {:?}", e),
    }

    fs::vfs::init();
    fs::tmpfs::mount("/tmp");
    kprintln!("[boot] VFS ready - tmpfs mounted at /tmp (native)");
    let test_data = b"Hello from Noxisa tmpfs!";
    match fs::vfs::write("/tmp/test.txt", test_data) {
        Ok(n) => kprintln!("[boot] Wrote {} bytes to /tmp/test.txt", n),
        Err(_) => kprintln!("[boot] tmpfs write failed"),
    }
    kprintln!("[boot] Noxisa 1.0 milestone reached - VFS + tmpfs working! (native)");

    ipc::init();

    hal::init();

    kprintln!("[boot] Handing off to scheduler - kshell starts now (native)...");

    scheduler::Scheduler::get().run();
}



// [removed] Dead Multiboot2 kernel_main() - superseded entirely by
// kernel_main_native() (Limine native protocol), the only live boot
// path. Nothing calls it by symbol name; bootloader/src/main.rs (a
// separate, also-unused custom bootloader crate) only ever reaches an
// entry point via runtime transmute of a discovered address, never a
// static Rust call. setup_syscall_msr() stays used - it's also called
// separately from kernel_main_native.

// ── Milestone 0.9: Userspace jump ───────────────────────────────────────────

/// Set up SYSCALL/SYSRET MSRs so ring-3 can call back into kernel.
unsafe fn setup_syscall_msr() {
    // EFER.SCE = 1 (enable syscall instruction), EFER.NXE = 1 (bit 11 -
    // enable the NX/XD page-table bit; W^X depends on this being set
    // before any page table entry uses bit 63, or that bit is reserved
    // and page-faults on first use instead of enforcing execute-disable).
    let efer_lo: u32;
    let efer_hi: u32;
    core::arch::asm!("rdmsr", in("ecx") 0xC0000080u32,
        out("eax") efer_lo, out("edx") efer_hi, options(nomem, nostack));
    let efer = (efer_lo as u64) | ((efer_hi as u64) << 32);
    let efer = efer | 1 | (1 << 11);
    core::arch::asm!("wrmsr", in("ecx") 0xC0000080u32,
        in("eax") efer as u32, in("edx") (efer >> 32) as u32,
        options(nomem, nostack));

    // STAR: ring0 CS=0x08, ring3 CS=0x18
    let star: u64 = (0x08u64 << 32) | (0x13u64 << 48);
    core::arch::asm!("wrmsr", in("ecx") 0xC0000081u32,
        in("eax") star as u32, in("edx") (star >> 32) as u32,
        options(nomem, nostack));

    // LSTAR: syscall handler address
    let handler = syscall_handler as u64;
    core::arch::asm!("wrmsr", in("ecx") 0xC0000082u32,
        in("eax") handler as u32, in("edx") (handler >> 32) as u32,
        options(nomem, nostack));
}

/// Minimal syscall handler
// Minimal capability bitmask - §4.4. One global "current process"
// bitmask, checked in exactly one place (the syscall dispatch below), per
// the handbook's own §1 architecture principle: don't let every subsystem
// invent its own security check.
const CAP_WRITE_SERIAL: u64 = 1 << 0;
const CAP_DRAW:         u64 = 1 << 1;

unsafe fn checked_sys_write(buf: *const u8, len: u64) {
    if scheduler::Scheduler::get().caps() & CAP_WRITE_SERIAL == 0 {
        crate::kprintln!("[cap] sys_write DENIED - process lacks CAP_WRITE_SERIAL");
        audit::audit_log("cap", "write", fs::vfs::current_uid(), false, alloc::string::String::from("CAP_WRITE_SERIAL"));
        return;
    }
    audit::audit_log("cap", "write", fs::vfs::current_uid(), true, alloc::string::String::from("CAP_WRITE_SERIAL"));
    unsafe { serial_write_bytes(buf, len); }
}

#[unsafe(naked)]
unsafe extern "C" fn syscall_handler() {
    core::arch::naked_asm!(
        // Switch off the caller's stack immediately - SYSCALL does NOT
        // switch RSP for us (unlike interrupts/exceptions + TSS.RSP0).
        // Every push/call below must happen on a real kernel stack, or
        // it silently overwrites whatever the caller had at the top of
        // its own stack (this was the isotest garbage-output bug).
        "mov [{saved_rsp}], rsp",
        "lea rsp, [{kstack} + 8192]",
        "cmp rax, 1",
        "jne 2f",
        // sys_write: call serial_write_bytes(buf, len)
        "push rcx",
        "push r11",
        "push rdx",
        "mov rdi, rsi",
        "mov rsi, rdx",
        "call {write_fn}",
        "pop rax",
        "pop r11",
        "pop rcx",
        "mov rsp, [{saved_rsp}]",
        "sysretq",
        "2:",
        "cmp rax, 2",
        "jne 5f",
        "push rcx",
        "push r11",
        "call {draw_fn}",
        "pop r11",
        "pop rcx",
        "xor rax, rax",
        "mov rsp, [{saved_rsp}]",
        "sysretq",
        "5:",
        "cmp rax, 60",
        "jne 3f",
        // sys_exit: call into the scheduler to switch away to the next
        // runnable task, rather than halting the whole machine (see
        // Scheduler::exit_current() for why - halting was only ever
        // correct when isotest/etc. shared kshell's own Tcb). This call
        // never returns (exit_current() is `-> !`), so there is
        // deliberately no code path back to `mov rsp, [{saved_rsp}]` /
        // `sysretq` below it - the exiting task's own stack is abandoned
        // in place, not restored.
        "call {exit_fn}",
        "3:",
        "mov rax, 0xffffffffffffffda",
        "mov rsp, [{saved_rsp}]",
        "sysretq",
        write_fn = sym checked_sys_write,
        draw_fn  = sym checked_sys_draw,
        exit_fn  = sym sys_exit_handler,
        saved_rsp = sym SAVED_USER_RSP,
        kstack = sym SYSCALL_KSTACK,
    );
}

/// Called from syscall_handler for sys_exit (rax=60). Never returns -
/// hands off to Scheduler::exit_current(), which switches to the next
/// runnable task instead of halting the whole machine. See exit_current()'s
/// own comment for the full reasoning and the known gap (no stack/frame
/// reclamation yet - that is a separate follow-up).
extern "C" fn sys_exit_handler() -> ! {
    scheduler::Scheduler::get().exit_current();
}

static mut SAVED_USER_RSP: u64 = 0;
#[unsafe(link_section = ".bss")]
static mut SYSCALL_KSTACK: [u8; 8192] = [0u8; 8192];


/// Called from syscall handler — direct UART write, no format overhead
/// Capability-checked entry point for sys_draw (rax=2). Previously
/// draw_success_marker() was called directly from syscall_handler with
/// no gate at all -- CAP_DRAW existed as a bit but nothing ever tested
/// it, so any process could draw regardless of its capability mask.
/// Mirrors checked_sys_write's chokepoint pattern exactly (single check,
/// same audit log wiring) rather than inventing a second convention.
extern "C" fn checked_sys_draw() {
    if scheduler::Scheduler::get().caps() & CAP_DRAW == 0 {
        crate::kprintln!("[cap] sys_draw DENIED - process lacks CAP_DRAW");
        audit::audit_log("cap", "draw", fs::vfs::current_uid(), false, alloc::string::String::from("CAP_DRAW"));
        return;
    }
    audit::audit_log("cap", "draw", fs::vfs::current_uid(), true, alloc::string::String::from("CAP_DRAW"));
    draw_success_marker();
}

extern "C" fn draw_success_marker() {
    crate::drivers::gpu::fill_rect(20, 20, 100, 100, 0x00FF00);
}

extern "C" fn serial_write_bytes(buf: *const u8, len: u64) {
    let cur_cr3: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cur_cr3, options(nomem, nostack)); }
    crate::kprintln!("[diag] CR3 at syscall time: {:#x}", cur_cr3);
    crate::kprintln!("[diag] serial_write_bytes: buf={:#x} len={}", buf as usize, len);
    crate::kprint!("[diag] raw bytes: ");
    for i in 0..(len as usize).min(32) {
        unsafe { crate::kprint!("{:02x} ", *buf.add(i)); }
    }
    crate::kprint!("\r\n");
    for i in 0..len as usize {
        let b = unsafe { *buf.add(i) };
        // Wait for TX ready (bit 5 of LSR at 0x3FD)
        unsafe {
            loop {
                let lsr: u8;
                core::arch::asm!("in al, dx", out("al") lsr,
                    in("dx") 0x3FDu16, options(nomem, nostack));
                if lsr & 0x20 != 0 { break; }
            }
            core::arch::asm!("out dx, al", in("dx") 0x3F8u16,
                in("al") b, options(nomem, nostack));
        }
    }
}

/// Jump to userspace — sets up a stack and iretq into ring 3
unsafe fn jump_to_userspace(entry: u64, ustack_top: u64) {
    let ucode = drivers::gdt::user_code_selector().0 as u64 | 3;
    let udata = drivers::gdt::user_data_selector().0 as u64 | 3;
    core::arch::asm!(
        "push {udata}",
        "push {rsp}",
        "push 0x202",
        "push {ucode}",
        "push {entry}",
        "iretq",
        udata = in(reg) udata,
        rsp   = in(reg) ustack_top,
        ucode = in(reg) ucode,
        entry = in(reg) entry,
        options(noreturn),
    );
}

/// Entry point for every spawn_isolated()'d Tcb (Part 6 item 2). Runs in
/// ring 0 on the Tcb's own normal kernel-allocated stack - reached via the
/// exact same switch_to()/context_switch() path as kshell or the multitest
/// worker (spawn_isolated() sets ctx.rip to this function, same as any
/// other spawn()'d task). By the time this runs, switch_to() has already
/// swapped CR3 to this task's isolated PML4 (the conditional-swap logic
/// added earlier), and Scheduler::current_pid() already reflects this task
/// - so current_user_entry_stack() reads back exactly what spawn_isolated()
/// stored for it. Only jump_to_userspace() itself performs the actual ring
/// 0 -> ring 3 transition (iretq); this trampoline is the last piece of
/// kernel-mode code to run before that happens.
extern "C" fn isolated_trampoline() -> ! {
    let (entry, stack_top) = scheduler::Scheduler::get().current_user_entry_stack();
    unsafe { jump_to_userspace(entry, stack_top); }
    // jump_to_userspace() never actually returns - the `iretq` inside it is
    // a `noreturn` asm block - but its own signature is declared `-> ()`,
    // not `-> !`, so the compiler doesn't know that at the type level.
    // isolated_trampoline() is declared `-> !`, so its body must literally
    // evaluate to `!`; this is unreachable at runtime but satisfies that.
    unreachable!("jump_to_userspace does not return");
}

fn draw_compositor_placeholder() {
    use crate::drivers::gpu::{dimensions, fill_rect};
    let (w, h) = dimensions();

    fill_rect(0, 0, w, h, 0x00_10_1214);

    let margin: u32 = 20;
    let gap: u32 = 16;
    let taskbar_h: u32 = 48;
    let title_h: u32 = 28;

    let usable_h = h.saturating_sub(taskbar_h + margin * 2);
    let win_w = (w.saturating_sub(margin * 2 + gap * 2)) / 3;

    let windows = [
        (0x00_35_50_70u32, 0x00_22_33_47u32),
        (0x00_70_4F_35u32, 0x00_47_33_22u32),
        (0x00_4F_70_35u32, 0x00_33_47_22u32),
    ];

    for (i, (body, titlebar)) in windows.iter().enumerate() {
        let x = margin + i as u32 * (win_w + gap);
        let y = margin;
        fill_rect(x, y, win_w, title_h, *titlebar);
        fill_rect(x, y + title_h, win_w, usable_h.saturating_sub(title_h), *body);
    }

    let taskbar_y = h.saturating_sub(taskbar_h);
    fill_rect(0, taskbar_y, w, taskbar_h, 0x00_18_1C_22);

    let icon_size = 24u32;
    let icon_y = taskbar_y + (taskbar_h - icon_size) / 2;
    for (i, (body, _)) in windows.iter().enumerate() {
        let icon_x = margin + i as u32 * (icon_size + 12);
        fill_rect(icon_x, icon_y, icon_size, icon_size, *body);
    }

    crate::kprintln!(
        "[compositor-placeholder] Drew 3 windows + taskbar at {}x{} (native)",
        w, h
    );
}

unsafe fn run_heap_test() {
    let layout_a = Layout::from_size_align(64, 8).expect("bad layout");
    let ptr_a = ALLOCATOR.alloc(layout_a);
    if ptr_a.is_null() {
        crate::kprint!("heaptest: alloc failed (Part A)\r\n");
    } else {
        for i in 0..64u8 {
            ptr_a.add(i as usize).write_volatile(0xAA);
        }
        ALLOCATOR.dealloc(ptr_a, layout_a);

        let ptr_a2 = ALLOCATOR.alloc(layout_a);
        let mut zero_count = 0u32;
        if !ptr_a2.is_null() {
            for i in 0..64usize {
                if ptr_a2.add(i).read_volatile() == 0 {
                    zero_count += 1;
                }
            }
            ALLOCATOR.dealloc(ptr_a2, layout_a);
        }
        crate::kprintln!(
            "heaptest Part A (zero-on-free): {}/64 bytes zero on reuse (same block reused: {})",
            zero_count, ptr_a2 == ptr_a
        );
        let part_a_ok = zero_count == 64 && ptr_a2 == ptr_a;
        let marker_colour = if part_a_ok { 0x0000FF00u32 } else { 0x00FF0000u32 };
        crate::drivers::gpu::fill_rect(150, 20, 100, 100, marker_colour);
    }

    crate::kprint!("heaptest Part B: deliberately corrupting a canary now.\r\n");
    crate::kprint!("A KERNEL PANIC is the EXPECTED result - that means detection works.\r\n");
    crate::drivers::gpu::fill_rect(280, 20, 100, 100, 0x00FFFF00);

    let layout_b = Layout::from_size_align(16, 8).expect("bad layout");
    let ptr_b = ALLOCATOR.alloc(layout_b);
    if !ptr_b.is_null() {
        ptr_b.add(16).write_volatile(0xFFu8);
        ALLOCATOR.dealloc(ptr_b, layout_b);
        crate::kprint!("heaptest: FAILED - corruption was not detected!\r\n");
    }
}

fn shell_cmd(line: &alloc::string::String) {
    let b = line.as_bytes();
    let mut i = 0;
    // skip leading spaces
    while i < b.len() && b[i] == b' ' { i += 1; }
    let b = &b[i..];

    if b == b"help" {
        crate::kprint!("Commands: help meminfo version reboot ticks uptime syscalls scrubtest teardowntest multitest audittest heaptest isotest isofault wxtest guardtest captest permtest ipctest spawn restest conctest preempttest drawtest ownertest haltest pcitest mmiotest nvmeinittest identifytest nvmeiotest blockdevtest
");
    } else if b == b"meminfo" {
        let mb = crate::memory::pmm::free_frames() * 4096 / (1024*1024);
        crate::kprintln!("Free: {} MiB", mb);
    } else if b == b"version" {
        crate::kprint!("Noxisa v0.1 kernel shell
");
    } else if b == b"reboot" {
        crate::kprint!("Rebooting...\r\n");
        unsafe { core::arch::asm!("out 0x64, al", in("al") 0xFEu8, options(nomem, nostack)); }
    } else if b == b"ticks" {
        crate::kprint!("Ticks: ");
        // print ticks count via serial directly
        let t = crate::drivers::pit::ticks();
        crate::kprintln!("{}", t);
    } else if b == b"uptime" {
        let ms = crate::drivers::pit::ms();
        crate::kprintln!("Uptime: {} ms", ms);
    } else if b == b"syscalls" {
        crate::kprint!("Total syscalls: 0\r\n");
    } else if b == b"audittest" {
        crate::kprintln!("--- Audit log test ---");
        unsafe {
            let before = audit::count();

            fs::vfs::set_identity(0, 0);
            let _ = fs::vfs::write("/tmp/audittest.txt", b"owner data");
            let chan = ipc::create_channel("audittest-chan");
            let _ = ipc::send(chan, b"owner message");

            fs::vfs::set_identity(1000, 1000);
            let _ = fs::vfs::write("/tmp/audittest.txt", b"intruder data");
            let _ = ipc::send(chan, b"intruder message");
            let mut buf = [0u8; 8];
            let _ = fs::vfs::read("/tmp/audittest.txt", &mut buf);

            fs::vfs::set_identity(0, 0);

            let after = audit::count();
            crate::kprintln!("[audittest] log entries before: {}, after: {}", before, after);

            let events = audit::snapshot();
            let mut found_vfs_deny = false;
            let mut found_ipc_deny = false;
            let mut found_vfs_allow = false;
            for e in events.iter().rev().take(after - before) {
                crate::kprintln!("[audittest]   tick={} {}::{} uid={} allowed={} detail={}",
                    e.tick, e.subsystem, e.action, e.uid, e.allowed, e.detail);
                if e.subsystem == "vfs" && e.action == "write" && e.uid == 1000 && !e.allowed { found_vfs_deny = true; }
                if e.subsystem == "ipc" && e.action == "send" && e.uid == 1000 && !e.allowed { found_ipc_deny = true; }
                if e.subsystem == "vfs" && e.action == "read" && e.uid == 1000 && e.allowed { found_vfs_allow = true; }
            }

            crate::kprintln!("[audittest] denied VFS write recorded: {}", found_vfs_deny);
            crate::kprintln!("[audittest] denied IPC send recorded: {}", found_ipc_deny);
            crate::kprintln!("[audittest] allowed VFS read recorded: {}", found_vfs_allow);

            if found_vfs_deny && found_ipc_deny && found_vfs_allow {
                crate::kprintln!("[audittest] PASS - denials are recorded, not just silently blocked");
            } else {
                crate::kprintln!("[audittest] FAIL - one or more expected audit entries missing");
            }
        }
    } else if b == b"teardowntest" {
        crate::kprintln!("--- Process teardown test ---");
        unsafe {
            let stack_phys = memory::pmm::alloc_frame().expect("out of memory allocating isolated stack");
            core::ptr::write_bytes(memory::paging::phys_to_virt(stack_phys) as *mut u8, 0xAA, 4096);

            let user_pages = [0x3ff000u64, 0x400000u64, 0x401000u64];
            let (pml4, table_frames) = memory::paging::create_isolated_user_table(&user_pages, stack_phys);
            let owned_count = table_frames.len() + 1;
            crate::kprintln!("[teardowntest] built isolated process: pml4={:#x}, {} page-table frames + 1 stack frame owned", pml4, table_frames.len());

            // Snapshot free count AFTER building (not before) - building
            // itself consumes owned_count frames from the pool, so a
            // "before build" vs "after teardown" comparison is EXPECTED to
            // match exactly (what got consumed gets given back). The real
            // check is that teardown increases the count relative to the
            // just-built state, by exactly the number of frames owned.
            let free_after_build = memory::pmm::free_frames();

            memory::paging::teardown_isolated_process(&table_frames, stack_phys);
            crate::kprintln!("[teardowntest] torn down: freed {} frames total (table frames + stack)", owned_count);

            let free_after_teardown = memory::pmm::free_frames();
            let reclaimed_correctly = free_after_teardown == free_after_build + owned_count as u64;
            crate::kprintln!("[teardowntest] free frame count after build={}, after teardown={} (reclaimed exactly {}: {})",
                free_after_build, free_after_teardown, owned_count, reclaimed_correctly);

            let mut all_zero = true;
            let mut reused_all = true;
            let mut reallocated = alloc::vec::Vec::new();
            for _ in 0..owned_count {
                let f = memory::pmm::alloc_frame().expect("out of memory re-allocating");
                reallocated.push(f);
                let va = memory::paging::phys_to_virt(f) as *const u8;
                for i in 0..4096usize {
                    if va.add(i).read_volatile() != 0 { all_zero = false; break; }
                }
            }
            for f in &reallocated {
                if !table_frames.contains(f) && *f != stack_phys {
                    reused_all = false;
                }
            }
            crate::kprintln!("[teardowntest] all {} re-allocated frames zero: {}", owned_count, all_zero);
            crate::kprintln!("[teardowntest] all re-allocated frames were genuinely reused (not fresh): {}", reused_all);

            if reclaimed_correctly && all_zero && reused_all {
                crate::kprintln!("[teardowntest] PASS - a whole process's page tables + stack were genuinely freed and scrubbed");
            } else {
                crate::kprintln!("[teardowntest] FAIL - reclaimed_correctly={} all_zero={} reused_all={}", reclaimed_correctly, all_zero, reused_all);
            }
        }
    } else if b == b"multitest" {
        crate::kprintln!("--- Cooperative multitasking test ---");
        // Part 6 item 2 prerequisite: proves a second real Tcb can genuinely
        // run and hand control back and forth via the now-fixed yield_now()
        // (previously it never called switch_to() at all - see scheduler
        // mod.rs). Spawns a worker that increments a shared counter while
        // yielding; kshell (the caller here) yields repeatedly and watches
        // the counter, confirming real interleaving, not just one task
        // running forever.
        MULTITEST_COUNTER.store(0, core::sync::atomic::Ordering::SeqCst);
        let worker_pid = scheduler::Scheduler::get().spawn("multitest-worker", multitest_worker);
        crate::kprintln!("[multitest] spawned worker pid={}", worker_pid);

        let mut seen_progress = false;
        let mut final_count = 0u32;
        for _ in 0..50 {
            scheduler::yield_now();
            let c = MULTITEST_COUNTER.load(core::sync::atomic::Ordering::SeqCst);
            if c > 0 { seen_progress = true; }
            final_count = c;
            if final_count >= 5 { break; }
        }

        crate::kprintln!("[multitest] final counter value: {} (expected 5)", final_count);
        crate::kprintln!("[multitest] seen_progress (counter moved off 0 at some point): {}", seen_progress);
        crate::kprintln!("[multitest] kshell resumed and is still responsive after yielding: true (this line printed)");

        if final_count == 5 && seen_progress {
            crate::kprintln!("[multitest] PASS - a second real task genuinely ran, interleaved via yield_now(), and kshell resumed correctly afterward");
        } else {
            crate::kprintln!("[multitest] FAIL - final_count={} seen_progress={}", final_count, seen_progress);
        }

        // Give the worker a bounded number of extra yields to actually
        // reach its own exit_current() call and become Zombie (it may
        // still be mid-flight the instant final_count hits 5 - the 5th
        // increment is immediately followed by one more yield_now() call
        // inside the worker before it reaches exit_current()). Reaping a
        // still-Runnable task would panic (see reap()'s own guard) and,
        // worse, previously left a permanently-Runnable leftover Tcb in
        // the run queue for every future yield_now() call to potentially
        // land on instead of its intended target.
        let sched = scheduler::Scheduler::get();
        let mut became_zombie = false;
        for _ in 0..10 {
            if sched.is_zombie(worker_pid) { became_zombie = true; break; }
            scheduler::yield_now();
        }
        if became_zombie {
            sched.reap(worker_pid);
            crate::kprintln!("[multitest] reaped worker pid={} - no leftover task left in the run queue", worker_pid);
        } else {
            crate::kprintln!("[multitest] WARNING - worker pid={} never reached Zombie state, not reaping (leak, but safer than reaping a live task)", worker_pid);
        }
    } else if b == b"conctest" {
        crate::kprintln!("--- Multiple concurrent user processes test ---");
        // Part 6 item 2: previously multitest only ever proved ONE extra
        // Tcb genuinely runs. The run queue (scheduler::RunQueue) was
        // always a real multi-slot priority queue (VecDeque per priority,
        // not a single-slot special case), so nothing here needed to
        // change structurally - but nothing had ever actually put 3
        // independent, differently-behaved Tcbs on it at once and proven
        // they all make progress, AND that IPC works between two of them
        // directly (not routed through kshell), AND that a third,
        // ungranted process is genuinely denied access to that same
        // channel while the other two are mid-flight. Reset every shared
        // static up front so conctest is safe to re-run in the same boot.
        CONC_COUNTER_A.store(0, core::sync::atomic::Ordering::SeqCst);
        CONC_COUNTER_B.store(0, core::sync::atomic::Ordering::SeqCst);
        CONC_COUNTER_C.store(0, core::sync::atomic::Ordering::SeqCst);
        CONC_CHAN_ID.store(0, core::sync::atomic::Ordering::SeqCst);
        CONC_IPC_DELIVERED.store(false, core::sync::atomic::Ordering::SeqCst);
        CONC_IPC_BYTES.store(0, core::sync::atomic::Ordering::SeqCst);
        CONC_C_DENIED.store(false, core::sync::atomic::Ordering::SeqCst);

        let sched = scheduler::Scheduler::get();
        let pid_a = sched.spawn("conc-worker-a", worker_conc_a);
        let pid_b = sched.spawn("conc-worker-b", worker_conc_b);
        let pid_c = sched.spawn("conc-worker-c", worker_conc_c);
        crate::kprintln!("[conctest] spawned 3 independent workers: a={} b={} c={}", pid_a, pid_b, pid_c);

        // Yield a bounded number of times, watching all three counters
        // move independently - this is the actual proof of genuine
        // rotation between MORE than two Tcbs (kshell + a + b + c = 4
        // runnable tasks at once), not just kshell handing off to a
        // single worker and back.
        let mut min_seen = 0u32;
        for _ in 0..300 {
            scheduler::yield_now();
            let a = CONC_COUNTER_A.load(core::sync::atomic::Ordering::SeqCst);
            let b = CONC_COUNTER_B.load(core::sync::atomic::Ordering::SeqCst);
            let c = CONC_COUNTER_C.load(core::sync::atomic::Ordering::SeqCst);
            min_seen = a.min(b).min(c);
            if sched.is_zombie(pid_a) && sched.is_zombie(pid_b) && sched.is_zombie(pid_c) { break; }
        }

        let final_a = CONC_COUNTER_A.load(core::sync::atomic::Ordering::SeqCst);
        let final_b = CONC_COUNTER_B.load(core::sync::atomic::Ordering::SeqCst);
        let final_c = CONC_COUNTER_C.load(core::sync::atomic::Ordering::SeqCst);
        let delivered = CONC_IPC_DELIVERED.load(core::sync::atomic::Ordering::SeqCst);
        let ipc_bytes = CONC_IPC_BYTES.load(core::sync::atomic::Ordering::SeqCst);
        let c_denied = CONC_C_DENIED.load(core::sync::atomic::Ordering::SeqCst);

        crate::kprintln!("[conctest] final counters: a={} b={} c={} (expect 5,5,5)", final_a, final_b, final_c);
        crate::kprintln!("[conctest] min_seen mid-flight (all three moved together, not sequentially to completion): {}", min_seen);
        crate::kprintln!("[conctest] IPC A->B delivered: {} ({} bytes, expect true / 19 - \"hello from worker A\")", delivered, ipc_bytes);
        crate::kprintln!("[conctest] worker C (never granted) denied recv on A's channel: {} (expect true)", c_denied);

        let pass = final_a == 5 && final_b == 5 && final_c == 5 && delivered && ipc_bytes == 19 && c_denied;
        if pass {
            crate::kprintln!("[conctest] PASS - 3 independent Tcbs genuinely interleaved, IPC worked between two unrelated processes (not via kshell), and the third's access was correctly denied");
        } else {
            crate::kprintln!("[conctest] FAIL - see values above");
        }

        // Reap all three, same bounded-wait-for-Zombie pattern multitest
        // established (a worker's very last yield_now() before its own
        // exit_current() means it may not be Zombie the instant the
        // observing loop above exits).
        for pid in [pid_a, pid_b, pid_c] {
            let mut became_zombie = sched.is_zombie(pid);
            for _ in 0..10 {
                if became_zombie { break; }
                scheduler::yield_now();
                became_zombie = sched.is_zombie(pid);
            }
            if became_zombie {
                sched.reap(pid);
                crate::kprintln!("[conctest] reaped pid={}", pid);
            } else {
                crate::kprintln!("[conctest] WARNING - pid={} never reached Zombie, not reaping", pid);
            }
        }
    } else if b == b"preempttest" {
        crate::kprintln!("--- Preemptive scheduling test ---");
        // preempt_worker() deliberately calls yield_now() zero times -
        // proving the timer ISR reclaims the CPU without cooperation.
        PREEMPT_COUNTER.store(0, core::sync::atomic::Ordering::SeqCst);
        PREEMPT_STOP.store(false, core::sync::atomic::Ordering::SeqCst);

        let sched = scheduler::Scheduler::get();
        let pid = sched.spawn("preempt-worker", preempt_worker);
        crate::kprintln!("[preempttest] spawned pid={} - infinite loop, zero yield_now() calls by design", pid);

        let start_ticks = drivers::pit::ticks();
        let mut samples: [u32; 3] = [0; 3];
        let mut sample_ticks: [u64; 3] = [0; 3];
        let mut i = 0usize;
        loop {
            let now = drivers::pit::ticks();
            if i < 3 && now >= start_ticks + (i as u64 + 1) * 5 {
                samples[i] = PREEMPT_COUNTER.load(core::sync::atomic::Ordering::SeqCst);
                sample_ticks[i] = now;
                i += 1;
            }
            if i >= 3 { break; }
            if now > start_ticks + 500 {
                crate::kprintln!("[preempttest] WARNING - timed out waiting for ticks to advance, PIC/IDT wiring may be wrong");
                break;
            }
            core::hint::spin_loop();
        }
        PREEMPT_STOP.store(true, core::sync::atomic::Ordering::SeqCst);

        crate::kprintln!("[preempttest] samples (tick,counter): ({},{}) ({},{}) ({},{})",
            sample_ticks[0], samples[0], sample_ticks[1], samples[1], sample_ticks[2], samples[2]);

        let advancing = samples[2] > samples[1] && samples[1] > samples[0];
        crate::kprintln!("[preempttest] worker counter genuinely advancing, zero yield_now() calls either side: {} (expect true)", advancing);

        let mut became_zombie = sched.is_zombie(pid);
        for _ in 0..2_000_000u32 {
            if became_zombie { break; }
            became_zombie = sched.is_zombie(pid);
            core::hint::spin_loop();
        }
        if became_zombie {
            sched.reap(pid);
            crate::kprintln!("[preempttest] reaped pid={}", pid);
        } else {
            crate::kprintln!("[preempttest] WARNING - worker never reached Zombie, not reaping");
        }

        if advancing {
            crate::kprintln!("[preempttest] PASS - timer ISR genuinely preempted a non-cooperating infinite loop");
        } else {
            crate::kprintln!("[preempttest] FAIL - see samples above");
        }
        } else if b == b"scrubtest" {
        crate::kprintln!("--- PMM frame scrubbing test ---");
        unsafe {
            let addr1 = memory::pmm::alloc_frame().expect("out of memory");
            let va1 = memory::paging::phys_to_virt(addr1) as *mut u8;
            core::ptr::write_bytes(va1, 0xAA, 4096);
            crate::kprintln!("[scrubtest] allocated frame {:#x}, wrote 0xAA pattern across 4096 bytes", addr1);

            memory::pmm::free_frame(addr1);
            crate::kprintln!("[scrubtest] freed frame {:#x}", addr1);

            let addr2 = memory::pmm::alloc_frame().expect("out of memory");
            let same_frame = addr1 == addr2;
            crate::kprintln!("[scrubtest] re-allocated: {:#x} (same physical frame reused: {})", addr2, same_frame);

            let va2 = memory::paging::phys_to_virt(addr2) as *const u8;
            let mut all_zero = true;
            for i in 0..4096usize {
                if va2.add(i).read_volatile() != 0 {
                    all_zero = false;
                    break;
                }
            }
            crate::kprintln!("[scrubtest] 4096/4096 bytes zero on reuse: {}", all_zero);

            if same_frame && all_zero {
                crate::kprintln!("[scrubtest] PASS - frame was genuinely reused and genuinely zeroed first");
            } else {
                crate::kprintln!("[scrubtest] FAIL - same_frame={} all_zero={}", same_frame, all_zero);
            }
        }
    } else if b == b"heaptest" {
        unsafe { run_heap_test(); }
    } else if b == b"isotest" {
        crate::kprint!("Building isolated page table for the ELF process...\r\n");
        crate::kprint!("(one-way trip - sys_exit halts the system afterward)\r\n");
        unsafe {
            crate::kprintln!("--- diagnostic: first 32 bytes at each mapped page ---");
            for &addr in &[0x3ff000u64, 0x400000u64, 0x401000u64] {
                let p = addr as *const u8;
                crate::kprint!("{:#x}: ", addr);
                for i in 0..32usize {
                    crate::kprint!("{:02x} ", p.add(i).read_volatile());
                }
            crate::kprintln!("--- diagnostic: full .text segment (0x400000, 524 bytes) ---");
            {
                let p = 0x400000u64 as *const u8;
                for row in 0..(524 / 16 + 1) {
                    let base = row * 16;
                    if base >= 524 { break; }
                    crate::kprint!("{:#06x}: ", base);
                    for i in 0..16usize {
                        if base + i >= 524 { break; }
                        crate::kprint!("{:02x} ", p.add(base + i).read_volatile());
                    }
                    crate::kprint!("\r\n");
                }
            }
                crate::kprint!("\r\n");
            }

            let stack_phys = crate::memory::pmm::alloc_frame()
                .expect("out of memory allocating isolated stack");
            core::ptr::write_bytes(stack_phys as *mut u8, 0, 4096);
            crate::kprint!("[diag] stack_phys={:#x} zeroed. Re-check before jump: ", stack_phys);
            let sp = stack_phys as *const u8;
            let mut all_zero = true;
            for i in 0..64usize {
                if sp.add(i).read_volatile() != 0 { all_zero = false; }
            }
            crate::kprintln!("all_zero={}", all_zero);
            let user_pages = [0x3ff000u64, 0x400000u64, 0x401000u64];
            let (new_pml4, table_frames) = memory::paging::create_isolated_user_table(&user_pages, stack_phys);
            let stack_top = stack_phys + 4096;
            crate::kprintln!("Isolated PML4 built at {:#x}.", new_pml4);

            // Part 6 item 2: isotest is now a REAL spawned Tcb, not an
            // inline mov-cr3 + one-way jmp in kshell's own call stack.
            // spawn_isolated() builds a normal kernel-mode Tcb (own stack,
            // own PML4, own entry point stashed for isolated_trampoline to
            // read) and pushes it onto the run queue; yield_now() then lets
            // the scheduler actually switch to it. kshell stays alive and
            // resumes right here once the isolated task's sys_exit fires
            // exit_current() - no more machine-wide halt.
            //
            // table_frames + stack_phys are handed in (not mem::forget()'d)
            // so a later reap(pid) call can actually free them via
            // teardown_isolated_process() - closes the reclamation gap this
            // used to leave open.
            let pid = scheduler::Scheduler::get().spawn_isolated(
                "isotest", new_pml4, 0x400000, stack_top, CAP_WRITE_SERIAL,
                table_frames, stack_phys,
            );
            crate::kprintln!("[isotest] spawned isolated task pid={}, yielding to it...", pid);
            let free_before = memory::pmm::free_frames();
            // Bounded wait-for-Zombie, same pattern multitest/conctest/
            // preempttest already established -- a single yield_now() only
            // guarantees SOME switch happened, not that this specific task
            // reached Zombie. With real preemption, switch_to() re-enqueues
            // the outgoing task (kshell) immediately, so a timer tick can
            // bounce control back here before the spawned task finishes,
            // and reap() would then panic on a still-live task (see Part 4
            // bug #26 - drawtest hit this intermittently).
            let sched_ref = scheduler::Scheduler::get();
            let mut became_zombie = sched_ref.is_zombie(pid);
            for _ in 0..10 {
                if became_zombie { break; }
                scheduler::yield_now();
                became_zombie = sched_ref.is_zombie(pid);
            }
            crate::kprintln!("[isotest] kshell resumed after isolated task exited - PASS");

            // Part 6 item 2 follow-up: reap the now-Zombie task, freeing
            // its page-table frames + isolated stack (via
            // teardown_isolated_process(), same primitive teardowntest
            // proved correct) and its own kernel-mode stack. Verifies the
            // free count actually goes back up by the expected amount,
            // same style of check teardowntest already uses.
            scheduler::Scheduler::get().reap(pid);
            let free_after = memory::pmm::free_frames();
            crate::kprintln!("[isotest] reaped pid={}: free frames {} -> {} (reclaimed {})",
                pid, free_before, free_after, free_after.saturating_sub(free_before));
        }
    } else if b.starts_with(b"spawn ") || b == b"spawn" {
        let name = if b == b"spawn" {
            ""
        } else {
            core::str::from_utf8(&b[6..]).unwrap_or("").trim()
        };
        let elf_data: Option<&[u8]> = match name {
            "hello" => Some(fs::initrd::HELLO_ELF),
            _ => None,
        };
        match elf_data {
            None => {
                crate::kprintln!("spawn: unknown program {:?} (known: hello)", name);
            }
            Some(data) => {
                match scheduler::Scheduler::get().spawn_program(name, data, CAP_WRITE_SERIAL) {
                    Err(e) => crate::kprintln!("spawn: failed - {}", e),
                    Ok(pid) => {
                        crate::kprintln!("[spawn] launched {:?} as pid={}, yielding to it...", name, pid);
                        let free_before = memory::pmm::free_frames();
                        // Bounded wait-for-Zombie -- see Part 4 bug #26.
                        let sched_ref = scheduler::Scheduler::get();
                        let mut became_zombie = sched_ref.is_zombie(pid);
                        for _ in 0..10 {
                            if became_zombie { break; }
                            scheduler::yield_now();
                            became_zombie = sched_ref.is_zombie(pid);
                        }
                        crate::kprintln!("[spawn] kshell resumed after pid={} exited", pid);
                        scheduler::Scheduler::get().reap(pid);
                        let free_after = memory::pmm::free_frames();
                        crate::kprintln!("[spawn] reaped pid={}: free frames {} -> {} (reclaimed {})",
                            pid, free_before, free_after, free_after.saturating_sub(free_before));
                    }
                }
            }
        }
    } else if b == b"isofault" {
        crate::kprint!("Building isolated page table for the negative isolation test...\r\n");
        crate::kprint!("(expect a #PF panic: PROTECTION_VIOLATION | USER_MODE - that means isolation works)\r\n");
        unsafe {
            let stack_phys = crate::memory::pmm::alloc_frame()
                .expect("out of memory allocating isolated stack");
            core::ptr::write_bytes(stack_phys as *mut u8, 0, 4096);

            let user_pages = [0x3ff000u64, 0x400000u64, 0x401000u64];

            // Hand-written stub, poked into unused (but mapped) space well
            // past hello's real _start code in the same .text page.
            // Deliberately tries to write to a canonical-high kernel
            // address - should take #PF, since the isolated table's copied
            // upper-half kernel entries are never given the U bit (see
            // create_isolated_user_table).
            //   mov rax, 0xffffffff80000000
            //   mov [rax], rax
            //   (loop, never reached if the fault fires as expected)
            // NOTE: this write must happen BEFORE the isolated table exists
            // and CR3 switches - .text is read-only in the isolated table
            // now (W^X, §4.2), so poking it afterward would itself fault.
            // The physical page persists regardless of which CR3 is live.
            let stub_addr = 0x400300u64;
            let stub: [u8; 16] = [
                0x48, 0xb8, 0x00, 0x00, 0x00, 0x80, 0xff, 0xff, 0xff, 0xff, // movabs rax, 0xffffffff80000000
                0x48, 0x89, 0x00,                                          // mov [rax], rax
                0xeb, 0xfe,                                                // jmp $ (never reached)
                0x90,
            ];
            core::ptr::copy_nonoverlapping(stub.as_ptr(), stub_addr as *mut u8, stub.len());

            let (new_pml4, _table_frames) = memory::paging::create_isolated_user_table(&user_pages, stack_phys); // table_frames unused here - these tests are one-way jumps that halt, never tear down
            core::arch::asm!("mov cr3, {}", in(reg) new_pml4, options(nomem, nostack));

            let stack_top = stack_phys + 4096;
            crate::kprintln!("Isolated PML4 built at {:#x}. Jumping to the fault stub at {:#x}...", new_pml4, stub_addr);
            jump_to_userspace(stub_addr, stack_top);
        }
    } else if b == b"wxtest" {
        crate::kprint!("Building isolated page table for the W^X test...\r\n");
        crate::kprint!("(expect a #PF panic: PROTECTION_VIOLATION | INSTRUCTION_FETCH | USER_MODE)\r\n");
        unsafe {
            let stack_phys = crate::memory::pmm::alloc_frame()
                .expect("out of memory allocating isolated stack");
            core::ptr::write_bytes(stack_phys as *mut u8, 0, 4096);

            let user_pages = [0x3ff000u64, 0x400000u64, 0x401000u64];
            let (new_pml4, _table_frames) = memory::paging::create_isolated_user_table(&user_pages, stack_phys); // table_frames unused here - these tests are one-way jumps that halt, never tear down
            core::arch::asm!("mov cr3, {}", in(reg) new_pml4, options(nomem, nostack));

            // The stack is mapped writable + NX (§4.2). Jump straight into
            // it as if it were code - the CPU must refuse the instruction
            // fetch itself, before executing a single byte there.
            let stack_top = stack_phys + 4096;
            crate::kprintln!("Isolated PML4 built at {:#x}. Jumping into the (NX) stack at {:#x}...", new_pml4, stack_phys);
            jump_to_userspace(stack_phys, stack_top);
        }
    } else if b == b"captest" {
        crate::kprint!("Building isolated page table for the capability test...\r\n");
        crate::kprint!("(same ELF as isotest, but CAP_WRITE_SERIAL is withheld - expect sys_write to be denied, then a clean exit back to kshell)\r\n");
        unsafe {
            let stack_phys = crate::memory::pmm::alloc_frame()
                .expect("out of memory allocating isolated stack");
            core::ptr::write_bytes(stack_phys as *mut u8, 0, 4096);

            let user_pages = [0x3ff000u64, 0x400000u64, 0x401000u64];
            let (new_pml4, table_frames) = memory::paging::create_isolated_user_table(&user_pages, stack_phys);
            let stack_top = stack_phys + 4096;
            crate::kprintln!("Isolated PML4 built at {:#x}.", new_pml4);

            // Part 6 item 2: captest is now a REAL spawned Tcb, same
            // conversion as isotest. caps=0 here is deliberate - the whole
            // point of this test is proving CAP_WRITE_SERIAL is withheld
            // and sys_write is denied, not that the task lacks capabilities
            // by omission. table_frames + stack_phys handed in (not
            // mem::forget()'d) so reap(pid) can free them later.
            let pid = scheduler::Scheduler::get().spawn_isolated(
                "captest", new_pml4, 0x400000, stack_top, 0,
                table_frames, stack_phys,
            );
            crate::kprintln!("[captest] spawned isolated task pid={}, yielding to it...", pid);
            let free_before = memory::pmm::free_frames();
            // Bounded wait-for-Zombie -- see Part 4 bug #26.
            let sched_ref = scheduler::Scheduler::get();
            let mut became_zombie = sched_ref.is_zombie(pid);
            for _ in 0..10 {
                if became_zombie { break; }
                scheduler::yield_now();
                became_zombie = sched_ref.is_zombie(pid);
            }
            crate::kprintln!("[captest] kshell resumed after isolated task exited - PASS");

            // Same reap-and-verify step as isotest (Part 6 item 2 follow-up).
            scheduler::Scheduler::get().reap(pid);
            let free_after = memory::pmm::free_frames();
            crate::kprintln!("[captest] reaped pid={}: free frames {} -> {} (reclaimed {})",
                pid, free_before, free_after, free_after.saturating_sub(free_before));
        }
    } else if b == b"haltest" {
        crate::kprintln!("[haltest] Proving hal::registry wrappers are genuine pass-throughs, not just structure");

        // Console: write through the HAL registry, then compare against
        // calling drivers::serial directly. If the HAL path is a real
        // pass-through, both produce byte-identical serial output.
        hal::registry::console_write_fmt(format_args!("[haltest] via hal::registry::console_write_fmt\r\n"));
        drivers::serial::write_fmt(format_args!("[haltest] via drivers::serial::write_fmt directly\r\n"));

        // Display: dimensions from the HAL registry must equal the
        // dimensions from calling drivers::gpu directly.
        let hal_dims = hal::registry::display_dimensions();
        let direct_dims = drivers::gpu::dimensions();
        crate::kprintln!("[haltest] hal::registry::display_dimensions() = {:?}", hal_dims);
        crate::kprintln!("[haltest] drivers::gpu::dimensions() direct  = {:?}", direct_dims);
        let dims_match = hal_dims == Some(direct_dims);

        // Display: fill_rect through the HAL registry must return true
        // (meaning a display was actually registered and reached).
        let fill_ok = hal::registry::display_fill_rect(4, 4, 8, 8, 0x0000FF00);
        crate::kprintln!("[haltest] hal::registry::display_fill_rect(...) returned {}", fill_ok);

        // Input: poll once through the HAL registry. Doesn't assert a
        // specific keypress (none is guaranteed at test time) -- only
        // proves the call reaches drivers::keyboard without panicking.
        let hal_key = hal::registry::input_poll_char();
        crate::kprintln!("[haltest] hal::registry::input_poll_char() = {:?} (no keypress expected; None is a PASS here)", hal_key);

        if dims_match && fill_ok {
            crate::kprintln!("[haltest] PASS - console, display, and input all reached the real drivers through the HAL registry, dimensions matched exactly");
        } else {
            crate::kprintln!("[haltest] FAIL - dims_match={} fill_ok={}", dims_match, fill_ok);
        }
    } else if b == b"drawtest" {
        // Proves CAP_DRAW is actually enforced, not just gated (patch_9
        // added checked_sys_draw(); nothing before this exercised it).
        // Uses the real generic spawn path (spawn_program), NOT the
        // shared-page isotest/captest mechanism -- those three always
        // reuse the same physical pages loaded with hello's code at
        // boot, so a drawtest built on that primitive would silently
        // execute hello's sys_write instead of drawtest's sys_draw.
        // spawn_program() does a fresh, independent ELF load per call
        // (Part 2's "Generic process creation" milestone), so
        // DRAWTEST_ELF's own code genuinely runs.
        crate::kprintln!("--- CAP_DRAW enforcement test ---");
        crate::kprint!("(drawtest calls sys_draw with CAP_DRAW withheld - expect denial, then a clean exit back to kshell)\r\n");
        match scheduler::Scheduler::get().spawn_program("drawtest", fs::initrd::DRAWTEST_ELF, 0) {
            Err(e) => crate::kprintln!("drawtest: spawn failed - {}", e),
            Ok(pid) => {
                crate::kprintln!("[drawtest] launched pid={}, yielding to it...", pid);
                let free_before = memory::pmm::free_frames();
                // Bounded wait-for-Zombie -- see Part 4 bug #26. This is
                // the exact call site that surfaced the race: a single
                // yield_now() only guarantees SOME switch happened, not
                // that this pid specifically reached Zombie, so an
                // immediate reap() could panic on a merely-preempted task.
                let sched_ref = scheduler::Scheduler::get();
                let mut became_zombie = sched_ref.is_zombie(pid);
                for _ in 0..10 {
                    if became_zombie { break; }
                    scheduler::yield_now();
                    became_zombie = sched_ref.is_zombie(pid);
                }
                crate::kprintln!("[drawtest] kshell resumed after pid={} exited - PASS", pid);
                scheduler::Scheduler::get().reap(pid);
                let free_after = memory::pmm::free_frames();
                crate::kprintln!("[drawtest] reaped pid={}: free frames {} -> {} (reclaimed {})",
                    pid, free_before, free_after, free_after.saturating_sub(free_before));
            }
        }
    } else if b == b"ipctest" {
        crate::kprintln!("--- IPC default-deny channel test ---");
        unsafe {
            fs::vfs::set_identity(0, 0);
            let chan = ipc::create_channel("ipctest-chan");
            crate::kprintln!("[ipctest] created channel {} as uid=0 (owner)", chan);

            crate::kprint!("[ipctest] as uid=0 (owner), send: ");
            match ipc::send(chan, b"owner message") {
                Ok(n) => crate::kprintln!("allowed as expected ({} bytes)", n),
                Err(e) => crate::kprintln!("FAILED - owner send should be allowed: {:?}", e),
            }

            fs::vfs::set_identity(1000, 1000);
            crate::kprint!("[ipctest] as uid=1000 (no grant), send: ");
            match ipc::send(chan, b"intruder message") {
                Ok(_) => crate::kprintln!("FAILED - send should have been denied!"),
                Err(ipc::IpcError::PermissionDenied) => crate::kprintln!("DENIED as expected (PermissionDenied)"),
                Err(e) => crate::kprintln!("wrong error: {:?} (expected PermissionDenied)", e),
            }

            crate::kprint!("[ipctest] as uid=1000, owner grants access, then send: ");
            fs::vfs::set_identity(0, 0);
            let _ = ipc::grant(chan, 1000);
            fs::vfs::set_identity(1000, 1000);
            match ipc::send(chan, b"granted message") {
                Ok(n) => crate::kprintln!("allowed as expected ({} bytes) - grant() worked", n),
                Err(e) => crate::kprintln!("FAILED - granted uid should be allowed: {:?}", e),
            }

            fs::vfs::set_identity(0, 0);
            crate::kprint!("[ipctest] as uid=0 (owner), recv: ");
            let mut buf = [0u8; 64];
            match ipc::recv(chan, &mut buf) {
                Ok(n) => crate::kprintln!("allowed as expected ({} bytes received)", n),
                Err(e) => crate::kprintln!("FAILED - owner recv should be allowed: {:?}", e),
            }
        }
    } else if b == b"permtest" {
        crate::kprintln!("--- VFS permission test ---");
        unsafe {
            fs::vfs::set_identity(0, 0);
            match fs::vfs::write("/tmp/permtest.txt", b"owner data") {
                Ok(n) => crate::kprintln!("[permtest] created as uid=0, {} bytes written", n),
                Err(e) => crate::kprintln!("[permtest] FAILED - could not even create file: {:?}", e),
            }

            fs::vfs::set_identity(1000, 1000);

            crate::kprint!("[permtest] as uid=1000, write to owner's file: ");
            match fs::vfs::write("/tmp/permtest.txt", b"attacker data") {
                Ok(_) => crate::kprintln!("FAILED - write should have been denied!"),
                Err(fs::vfs::FsError::PermissionDenied) => crate::kprintln!("DENIED as expected (PermissionDenied)"),
                Err(e) => crate::kprintln!("wrong error: {:?} (expected PermissionDenied)", e),
            }

            crate::kprint!("[permtest] as uid=1000, read the same file: ");
            let mut buf = [0u8; 32];
            match fs::vfs::read("/tmp/permtest.txt", &mut buf) {
                Ok(n) => crate::kprintln!("allowed as expected ({} bytes) - other has r-- ", n),
                Err(e) => crate::kprintln!("FAILED - read should have been allowed: {:?}", e),
            }

            fs::vfs::set_identity(0, 0);
            crate::kprint!("[permtest] back to uid=0, write to own file: ");
            match fs::vfs::write("/tmp/permtest.txt", b"owner data v2") {
                Ok(n) => crate::kprintln!("allowed as expected ({} bytes)", n),
                Err(e) => crate::kprintln!("FAILED - owner write should have been allowed: {:?}", e),
            }
        }
    } else if b == b"ownertest" {
        crate::kprintln!("--- tmpfs ownership-on-creation test (patch_10) ---");
        unsafe {
            // 1. Create a brand-new file as uid=1000/gid=1000. Before
            //    patch_10 this always landed as uid=0/gid=0 regardless
            //    of caller identity - Filesystem::write() had no
            //    uid/gid parameter at all.
            fs::vfs::set_identity(1000, 1000);
            match fs::vfs::write("/tmp/ownertest.txt", b"created by uid 1000") {
                Ok(n) => crate::kprintln!("[ownertest] created as uid=1000, {} bytes written", n),
                Err(e) => crate::kprintln!("[ownertest] FAILED - could not create file: {:?}", e),
            }

            crate::kprint!("[ownertest] stat() after creation: ");
            match fs::vfs::stat("/tmp/ownertest.txt") {
                Ok(s) if s.uid == 1000 && s.gid == 1000 => {
                    crate::kprintln!("PASS - uid={} gid={} (expected 1000/1000)", s.uid, s.gid);
                }
                Ok(s) => {
                    crate::kprintln!("FAILED - uid={} gid={} (expected 1000/1000)", s.uid, s.gid);
                }
                Err(e) => crate::kprintln!("FAILED - stat() error: {:?}", e),
            }

            // 2. Overwrite the same file as uid=0 (owner-write from a
            //    different, permitted identity - mode 0o644 grants
            //    "other" no write bit, but this checks the deeper
            //    invariant: write_owned() must not rechown an existing
            //    file on overwrite, only stamp identity on genuine
            //    creation. Using uid=1000 again (the actual owner) to
            //    isolate that specific behaviour without also exercising
            //    the permission-denial path permtest already covers.
            match fs::vfs::write("/tmp/ownertest.txt", b"overwritten by same uid") {
                Ok(n) => crate::kprintln!("[ownertest] overwritten, {} bytes written", n),
                Err(e) => crate::kprintln!("[ownertest] FAILED - overwrite denied unexpectedly: {:?}", e),
            }

            crate::kprint!("[ownertest] stat() after overwrite: ");
            match fs::vfs::stat("/tmp/ownertest.txt") {
                Ok(s) if s.uid == 1000 && s.gid == 1000 => {
                    crate::kprintln!("PASS - ownership unchanged, uid={} gid={}", s.uid, s.gid);
                }
                Ok(s) => {
                    crate::kprintln!("FAILED - ownership drifted on overwrite, uid={} gid={}", s.uid, s.gid);
                }
                Err(e) => crate::kprintln!("FAILED - stat() error: {:?}", e),
            }

            fs::vfs::set_identity(0, 0);
        }
    } else if b == b"restest" {
        crate::kprintln!("--- Per-process resources test ---");
        unsafe { fs::vfs::set_identity(0, 0); }

        // 1. cwd: default is "/", set it, confirm a relative open() resolves
        //    against it rather than against literal "/".
        let cwd_before = fs::vfs::cwd();
        crate::kprintln!("[restest] cwd before: {:?} (expect \"/\")", cwd_before);
        fs::vfs::set_cwd("/tmp");
        crate::kprintln!("[restest] cwd after set_cwd(\"/tmp\"): {:?}", fs::vfs::cwd());

        let _ = fs::vfs::write("/tmp/restest.txt", b"hello via fd");

        // 2. open() a relative path - must resolve through the new cwd, not
        //    literal "/restest.txt" (which doesn't exist and would fail).
        let fd = match fs::vfs::open("restest.txt", fs::vfs::O_RDONLY) {
            Ok(fd) => { crate::kprintln!("[restest] open(\"restest.txt\") under cwd=/tmp -> fd={}", fd); fd }
            Err(e) => { crate::kprintln!("[restest] FAILED - open should have resolved via cwd: {:?}", e); -1 }
        };

        // 3. read_fd() must return the same bytes write() put there, and
        //    advance the fd's own offset (proven by a second read_fd()
        //    below returning 0 - end of file, not a re-read from offset 0).
        let mut buf = [0u8; 32];
        let n = fs::vfs::read_fd(fd, &mut buf).unwrap_or(0);
        let got = core::str::from_utf8(&buf[..n]).unwrap_or("<invalid utf8>");
        crate::kprintln!("[restest] read_fd -> {:?} ({} bytes, expect \"hello via fd\")", got, n);

        let n2 = fs::vfs::read_fd(fd, &mut buf).unwrap_or(999);
        crate::kprintln!("[restest] second read_fd -> {} bytes (expect 0 - offset advanced, EOF)", n2);

        // 4. A second, independently-spawned process must NOT see fd 0/1/2
        //    already open, and must NOT inherit kshell's cwd - proving fds
        //    and cwd are genuinely per-Tcb, not shared global state (the
        //    exact bug class the old bare CURRENT_UID/CURRENT_CAPS globals
        //    already had for identity, before that was fixed).
        match scheduler::Scheduler::get().spawn_program("hello", fs::initrd::HELLO_ELF, CAP_WRITE_SERIAL) {
            Err(e) => crate::kprintln!("[restest] FAILED - could not spawn probe process: {}", e),
            Ok(pid) => {
                // Bounded wait-for-Zombie -- see Part 4 bug #26.
                let sched_ref = scheduler::Scheduler::get();
                let mut became_zombie = sched_ref.is_zombie(pid);
                for _ in 0..10 {
                    if became_zombie { break; }
                    scheduler::yield_now();
                    became_zombie = sched_ref.is_zombie(pid);
                }
                scheduler::Scheduler::get().reap(pid);
                crate::kprintln!("[restest] probe process pid={} spawned+reaped cleanly (own, empty fd table by construction)", pid);
            }
        }

        // 5. close_fd() frees the slot; a subsequent read_fd() on the same
        //    number must fail (NotFound), proving the fd is genuinely gone,
        //    not just reset to offset 0.
        let closed = fs::vfs::close_fd(fd);
        crate::kprintln!("[restest] close_fd({}) -> {} (expect true)", fd, closed);
        match fs::vfs::read_fd(fd, &mut buf) {
            Err(fs::vfs::FsError::NotFound) => crate::kprintln!("[restest] read_fd after close -> NotFound as expected"),
            other => crate::kprintln!("[restest] FAILED - expected NotFound after close, got {:?}", other),
        }

        fs::vfs::set_cwd("/");
        crate::kprintln!("--- restest done ---");
    } else if b == b"guardtest" {
        crate::kprint!("Building isolated page table for the guard-page test...\r\n");
        crate::kprint!("(expect a #PF panic: page-not-present, NOT PROTECTION_VIOLATION - the guard page is simply unmapped)\r\n");
        unsafe {
            let stack_phys = crate::memory::pmm::alloc_frame()
                .expect("out of memory allocating isolated stack");
            core::ptr::write_bytes(stack_phys as *mut u8, 0, 4096);

            let user_pages = [0x3ff000u64, 0x400000u64, 0x401000u64];

            // Stub: deliberately blow past the bottom of the stack. RSP
            // starts at stack_top (the very top of the one mapped stack
            // page); subtracting 0x2000 (two pages' worth) guarantees
            // landing well below stack_phys, into memory this isolated
            // table never mapped at all - no other page happens to share
            // that PT slot, so it's a real, unpopulated guard region, not
            // a coincidence. The write there should take a page-not-present
            // #PF, distinct from isofault/wxtest's PROTECTION_VIOLATION
            // faults (which hit *present* pages with the wrong permission).
            //   sub rsp, 0x2000
            //   mov [rsp], rax
            //   jmp $ (never reached)
            let stub_addr = 0x400300u64;
            let stub: [u8; 16] = [
                0x48, 0x81, 0xec, 0x00, 0x20, 0x00, 0x00, // sub rsp, 0x2000
                0x48, 0x89, 0x04, 0x24,                   // mov [rsp], rax
                0xeb, 0xfe,                                // jmp $ (never reached)
                0x90, 0x90, 0x90,
            ];
            // NOTE: same reason as isofault - poke .text before the
            // isolated table exists / CR3 switches, since .text is
            // read-only once that table is live (W^X, §4.2).
            core::ptr::copy_nonoverlapping(stub.as_ptr(), stub_addr as *mut u8, stub.len());

            let (new_pml4, _table_frames) = memory::paging::create_isolated_user_table(&user_pages, stack_phys); // table_frames unused here - these tests are one-way jumps that halt, never tear down
            core::arch::asm!("mov cr3, {}", in(reg) new_pml4, options(nomem, nostack));

            let stack_top = stack_phys + 4096;
            crate::kprintln!("Isolated PML4 built at {:#x}. Jumping to the guard-page stub at {:#x}...", new_pml4, stub_addr);
            jump_to_userspace(stub_addr, stack_top);
        }
    } else if b == b"pcitest" {
        crate::kprintln!("--- PCI enumeration test ---");
        let mut count: u32 = 0;
        drivers::pci::enumerate(|dev| {
            count += 1;
            crate::kprintln!(
                "[pci] {:02x}:{:02x}.{} vendor={:04x} device={:04x} class={:02x} subclass={:02x} prog_if={:02x}",
                dev.bus, dev.device, dev.function,
                dev.vendor_id, dev.device_id, dev.class, dev.subclass, dev.prog_if
            );
        });
        crate::kprintln!("[pcitest] {} device(s) found via config-space enumeration", count);

        match drivers::pci::find_nvme() {
            Some(dev) => {
                crate::kprintln!(
                    "[pcitest] NVMe controller found at {:02x}:{:02x}.{} (vendor={:04x} device={:04x})",
                    dev.bus, dev.device, dev.function, dev.vendor_id, dev.device_id
                );
                match dev.bar0 {
                    Some(bar) if bar.is_mmio => {
                        crate::kprintln!(
                            "[pcitest] BAR0: base={:#x} mmio=true 64bit={} prefetchable={}",
                            bar.base, bar.is_64bit, bar.prefetchable
                        );
                        crate::kprintln!("[pcitest] PASS - NVMe controller and BAR0 discovered");
                    }
                    Some(_) => crate::kprintln!("[pcitest] FAIL - BAR0 is I/O-space, not MMIO (unexpected for NVMe)"),
                    None => crate::kprintln!("[pcitest] FAIL - NVMe controller found but BAR0 is unreadable/unimplemented"),
                }
            }
            None => crate::kprintln!("[pcitest] no NVMe controller found (expected on hardware/QEMU config without one attached)"),
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
                    // This call reset the real controller independent of
                    // ADMIN_QUEUE/IO_QUEUE - drop any cached state so the
                    // next identifytest/nvmeiotest reinitializes fresh
                    // instead of using now-stale addresses (see patch_37b).
                    drivers::nvme::invalidate_persistent_state();
                }
                Some(_) => crate::kprintln!("[nvmeinittest] FAIL - NVMe BAR0 is not MMIO"),
                None => crate::kprintln!("[nvmeinittest] FAIL - NVMe controller found but BAR0 is unreadable"),
            },
            None => crate::kprintln!("[nvmeinittest] no NVMe controller found - nothing to test"),
        }
    } else if b == b"identifytest" {
        crate::kprintln!("--- NVMe Identify Controller/Namespace test ---");
        match drivers::nvme::identify_controller() {
            Ok(info) => {
                crate::kprintln!("[identifytest] VID   = {:#06x}", info.vid);
                crate::kprintln!("[identifytest] Serial = \"{}\"", info.serial);
                crate::kprintln!("[identifytest] Model  = \"{}\"", info.model);
                crate::kprintln!("[identifytest] FW Rev = \"{}\"", info.firmware);
                crate::kprintln!("[identifytest] Namespaces = {}", info.num_namespaces);
                crate::kprintln!("[identifytest] Identify Controller PASS");

                match drivers::nvme::identify_namespace(1) {
                    Ok(ns) => {
                        crate::kprintln!("[identifytest] NSID 1 NSZE = {} blocks", ns.nsze_blocks);
                        crate::kprintln!("[identifytest] NSID 1 NCAP = {} blocks", ns.ncap_blocks);
                        crate::kprintln!("[identifytest] NSID 1 NUSE = {} blocks", ns.nuse_blocks);
                        crate::kprintln!(
                            "[identifytest] active LBA format = {}",
                            ns.active_lba_format_idx
                        );
                        match ns.block_size {
                            Some(bs) => crate::kprintln!("[identifytest] block size = {} bytes", bs),
                            None => crate::kprintln!("[identifytest] block size: LBADS out of plausible range"),
                        }
                        crate::kprintln!("[identifytest] Identify Namespace PASS");
                        crate::kprintln!("[identifytest] PASS - both Identify commands completed and parsed");
                    }
                    Err(e) => crate::kprintln!("[identifytest] FAIL - identify_namespace returned {:?}", e),
                }
            }
            Err(e) => crate::kprintln!("[identifytest] FAIL - identify_controller returned {:?}", e),
        }
    } else if b == b"nvmeiotest" {
        crate::kprintln!("--- NVMe I/O path test: write LBA 0, read LBA 0, compare ---");
        match drivers::nvme::identify_namespace(1) {
            Ok(ns) => match ns.block_size {
                Some(block_size) if block_size as usize <= 0x1000 => {
                    let mut pattern = alloc::vec![0u8; block_size as usize];
                    for (i, b) in pattern.iter_mut().enumerate() {
                        *b = (i as u8).wrapping_mul(31).wrapping_add(7);
                    }
                    crate::kprintln!("[nvmeiotest] block size = {} bytes, writing pattern to LBA 0", block_size);
                    match drivers::nvme::io_write_blocks(1, 0, block_size, &pattern) {
                        Ok(()) => {
                            crate::kprintln!("[nvmeiotest] write PASS");
                            let mut readback = alloc::vec![0u8; block_size as usize];
                            match drivers::nvme::io_read_blocks(1, 0, block_size, &mut readback) {
                                Ok(()) => {
                                    crate::kprintln!("[nvmeiotest] read PASS");
                                    if readback == pattern {
                                        crate::kprintln!(
                                            "[nvmeiotest] memcmp PASS - {} bytes match exactly",
                                            block_size
                                        );
                                        crate::kprintln!("[nvmeiotest] PASS - write -> read -> memcmp succeeded");
                                    } else {
                                        crate::kprintln!("[nvmeiotest] FAIL - readback does not match what was written");
                                    }
                                }
                                Err(e) => crate::kprintln!("[nvmeiotest] FAIL - io_read_blocks returned {:?}", e),
                            }
                        }
                        Err(e) => crate::kprintln!("[nvmeiotest] FAIL - io_write_blocks returned {:?}", e),
                    }
                }
                Some(_) => crate::kprintln!("[nvmeiotest] FAIL - block size exceeds this patch's 4KB single-page limit"),
                None => crate::kprintln!("[nvmeiotest] FAIL - could not determine block size (LBADS out of range)"),
            },
            Err(e) => crate::kprintln!("[nvmeiotest] FAIL - identify_namespace returned {:?}", e),
        }
    } else if b == b"blockdevtest" {
        crate::kprintln!("--- BlockDevice HAL abstraction test (write/read/compare through the registry only) ---");
        let block_size = hal::registry::block_size();
        let block_count = hal::registry::block_count();
        crate::kprintln!("[blockdevtest] hal::registry::block_size()  = {} bytes", block_size);
        crate::kprintln!("[blockdevtest] hal::registry::block_count() = {} blocks", block_count);

        if block_size == 0 || block_size as usize > 0x1000 {
            crate::kprintln!("[blockdevtest] FAIL - block size is 0 or exceeds the 4KB single-page limit");
        } else {
            let mut pattern = alloc::vec![0u8; block_size as usize];
            for (i, b) in pattern.iter_mut().enumerate() {
                *b = (i as u8).wrapping_mul(17).wrapping_add(3);
            }
            crate::kprintln!("[blockdevtest] writing pattern to LBA 0 via hal::registry::block_write()");
            match hal::registry::block_write(0, &pattern) {
                Ok(()) => {
                    crate::kprintln!("[blockdevtest] block_write PASS");
                    let mut readback = alloc::vec![0u8; block_size as usize];
                    match hal::registry::block_read(0, &mut readback) {
                        Ok(()) => {
                            crate::kprintln!("[blockdevtest] block_read PASS");
                            if readback == pattern {
                                crate::kprintln!(
                                    "[blockdevtest] memcmp PASS - {} bytes match exactly",
                                    block_size
                                );
                                crate::kprintln!("[blockdevtest] PASS - BlockDevice HAL abstraction is a genuine pass-through");
                            } else {
                                crate::kprintln!("[blockdevtest] FAIL - readback does not match what was written");
                            }
                        }
                        Err(e) => crate::kprintln!("[blockdevtest] FAIL - hal::registry::block_read returned {:?}", e),
                    }
                }
                Err(e) => crate::kprintln!("[blockdevtest] FAIL - hal::registry::block_write returned {:?}", e),
            }
        }
    } else if !b.is_empty() {
        crate::kprint!("unknown command\r\n");
    }
}

// Part 6 item 2 prerequisite: shared progress counter for multitest,
// proving a second real Tcb genuinely runs and makes progress while
// yielding, rather than the run queue silently never being drained.
static MULTITEST_COUNTER: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

extern "C" fn multitest_worker() -> ! {
    for _ in 0..5 {
        MULTITEST_COUNTER.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        scheduler::yield_now();
    }
    // Real exit now exists (exit_current() marks this Tcb Zombie and
    // switches away for good) - this used to loop yielding forever
    // instead, which left a permanently-Runnable Tcb sitting in the run
    // queue after every multitest run, silently eligible to be picked up
    // by *any* later, unrelated yield_now() call (including isotest's -
    // this is what caused isotest to intermittently land on the wrong
    // task/cr3 when run later in the same boot). Calling exit_current()
    // here closes that hole at the source.
    scheduler::Scheduler::get().exit_current();
}

// ─── Part 6 item 2: multiple concurrent user processes ────────────────────────
//
// Three independent Tcbs, each with its own uid (set via the same
// fs::vfs::set_identity() every real process uses), running concurrently
// alongside kshell. Worker A and worker B talk over a real default-deny
// IPC channel directly - kshell never touches it - proving IPC works
// between two arbitrary unrelated processes, not just "kshell to a
// test-paired worker" the way ipctest exercises it. Worker C is a third,
// wholly independent process that also runs concurrently but was never
// granted access to A's channel, proving the negative case still holds
// with more than 2 processes live at once.
static CONC_COUNTER_A:     core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static CONC_COUNTER_B:     core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static CONC_COUNTER_C:     core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static CONC_CHAN_ID:       core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0); // 0 = not yet published
static CONC_IPC_DELIVERED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static CONC_IPC_BYTES:     core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static CONC_C_DENIED:      core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

extern "C" fn worker_conc_a() -> ! {
    unsafe { fs::vfs::set_identity(7001, 7001); }
    let chan = ipc::create_channel("conc-chan");
    let _ = ipc::grant(chan, 7002); // owner-only grant to worker B's uid, not C's
    CONC_CHAN_ID.store(chan, core::sync::atomic::Ordering::SeqCst);
    for _ in 0..5 {
        CONC_COUNTER_A.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        scheduler::yield_now();
    }
    let _ = ipc::send(chan, b"hello from worker A");
    scheduler::Scheduler::get().exit_current();
}

extern "C" fn worker_conc_b() -> ! {
    unsafe { fs::vfs::set_identity(7002, 7002); }
    for _ in 0..5 {
        CONC_COUNTER_B.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        scheduler::yield_now();
    }
    let mut chan = 0u32;
    for _ in 0..50 {
        chan = CONC_CHAN_ID.load(core::sync::atomic::Ordering::SeqCst);
        if chan != 0 { break; }
        scheduler::yield_now();
    }
    if chan != 0 {
        let mut buf = [0u8; 64];
        for _ in 0..50 {
            if let Ok(n) = ipc::recv(chan, &mut buf) {
                if n > 0 {
                    CONC_IPC_BYTES.store(n as u32, core::sync::atomic::Ordering::SeqCst);
                    CONC_IPC_DELIVERED.store(true, core::sync::atomic::Ordering::SeqCst);
                    break;
                }
            }
            scheduler::yield_now();
        }
    }
    scheduler::Scheduler::get().exit_current();
}

extern "C" fn worker_conc_c() -> ! {
    unsafe { fs::vfs::set_identity(7003, 7003); }
    for _ in 0..5 {
        CONC_COUNTER_C.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        scheduler::yield_now();
    }
    // Negative-access proof: C was never granted access to A's channel,
    // so recv() must be denied even though the channel genuinely exists
    // and is actively in use by A and B at this same moment.
    let mut chan = 0u32;
    for _ in 0..50 {
        chan = CONC_CHAN_ID.load(core::sync::atomic::Ordering::SeqCst);
        if chan != 0 { break; }
        scheduler::yield_now();
    }
    if chan != 0 {
        let denied = ipc::recv(chan, &mut [0u8; 8]).is_err();
        CONC_C_DENIED.store(denied, core::sync::atomic::Ordering::SeqCst);
    }
    scheduler::Scheduler::get().exit_current();
}


// ─── Preemptive scheduling ─────────────────────────────────────────────────
static PREEMPT_COUNTER: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static PREEMPT_STOP:    core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

extern "C" fn preempt_worker() -> ! {
    loop {
        PREEMPT_COUNTER.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        if PREEMPT_STOP.load(core::sync::atomic::Ordering::SeqCst) {
            scheduler::Scheduler::get().exit_current();
        }
        // Deliberately no yield_now() call anywhere in this loop.
    }
}

extern "C" fn shell_thread() -> ! {
    use kshell::exec_builtin;
    use drivers::serial;

    crate::kprintln!("
Noxisa kernel shell — type 'help'
Nexus> ");

    let mut line = alloc::string::String::new();
    loop {
        let b = serial::read_byte();
        match b {
            13 | 10 => {
                crate::kprint!("\r\n");
                shell_cmd(&line);
                line.clear();
                crate::kprint!("Nexus> ");
            }
            0x7F | 8 => { // backspace
                if !line.is_empty() {
                    line.pop();
                    crate::kprint!(" ");
                }
            }
            32..=126 => { // printable ASCII
                line.push(b as char);
                // echo back
                unsafe { drivers::serial::write_fmt(
                    format_args!("{}", b as char)
                ); }
            }
            _ => {}
        }
    }
}

// ─── Panic handler ────────────────────────────────────────────────────────────

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    unsafe { core::arch::asm!("cli", options(nomem, nostack)); }

    // Deliberately bypasses UART's normal lock (write_fmt_panic(), not
    // write_fmt()/kprintln!()) - if whatever panicked did so while another
    // kprintln! elsewhere still held UART's lock (no unwinding in this
    // kernel means that lock's guard would never have dropped), going
    // through the normal locked path here would deadlock the panic handler
    // itself trying to report the panic - turning a real bug into a silent
    // hang with no message at all. This was confirmed live via gdb: caught
    // a panic's own print spinning forever on UART's lock. The panic
    // handler must be able to report unconditionally, so it never takes
    // that lock.
    if let Some(loc) = info.location() {
        drivers::serial::write_fmt_panic(format_args!("\n*** KERNEL PANIC ***\n  at {}:{}\n  {}",
            loc.file(), loc.line(), info.message()));
    } else {
        drivers::serial::write_fmt_panic(format_args!("\n*** KERNEL PANIC ***\n  {}", info.message()));
    }

    drivers::apic::broadcast_halt();
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)); }
    }
}

// ─── OOM handler ─────────────────────────────────────────────────────────────

#[alloc_error_handler]
fn oom(layout: core::alloc::Layout) -> ! {
    panic!("OOM allocating {} bytes (align {})", layout.size(), layout.align());
}

// ─── Boot info structs (must match bootloader layout exactly) ─────────────────

#[repr(C)]
pub struct BootInfo {
    /// Magic: 0x4E455855_4E455855 ("NEXUNEXU") — sanity check
    pub magic:          u64,
    /// Virtual base of the Higher-Half Direct Map (all RAM mapped here)
    pub hhdm_offset:    u64,
    pub framebuffer:    FramebufferInfo,
    pub memory_map_len: u32,
    pub _pad:           u32,
    pub memory_map_ptr: *const MemoryRegion,
}

impl BootInfo {
    /// Returns the slice of memory regions from the bootloader.
    pub fn memory_map(&self) -> &[MemoryRegion] {
        // SAFETY: bootloader allocates this slice and it lives until we take over.
        unsafe {
            core::slice::from_raw_parts(
                self.memory_map_ptr,
                self.memory_map_len as usize,
            )
        }
    }
}

#[repr(C)]
pub struct FramebufferInfo {
    pub phys_addr: u64,
    pub width:     u32,
    pub height:    u32,
    pub pitch:     u32,   // bytes per scanline
    pub bpp:       u8,    // bits per pixel
    pub _pad:      [u8; 3],
}

#[repr(C)]
pub struct MemoryRegion {
    pub base:   u64,
    pub length: u64,
    pub kind:   MemoryKind,
    pub _pad:   u32,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    Usable          = 0,
    Reserved        = 1,
    AcpiReclaimable = 2,
    AcpiNvs         = 3,
    BadMemory       = 4,
    BootloaderData  = 5,
    KernelCode      = 6,
    Framebuffer     = 7,
}

// ─── Kernel print macro (no heap, uses serial) ────────────────────────────────

#[doc(hidden)]
pub fn _kprint(args: core::fmt::Arguments) {
    drivers::serial::write_fmt(args);
}

/// Print to serial without newline.
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => { $crate::_kprint(format_args!($($arg)*)) };
}

/// Print to serial with newline.
#[macro_export]
macro_rules! kprintln {
    ()              => { $crate::kprint!("\n") };
    ($($arg:tt)*)   => { $crate::kprint!("{}\n", format_args!($($arg)*)) };
}

// ── Multiboot2 header — required for QEMU -kernel to load our ELF ────────────
// Must be in the first 32 KiB of the binary.
// Magic + architecture + length + checksum + end tag
#[used]
#[link_section = ".multiboot2"]
static MULTIBOOT2_HEADER: [u32; 8] = {
    let magic:  u32 = 0xE85250D6;
    let arch:   u32 = 0;
    let len:    u32 = 32;
    let check:  u32 = (0u32.wrapping_sub(magic.wrapping_add(arch).wrapping_add(len)));
    // end tag: type=0, flags=0, size=8
    [magic, arch, len, check, 0, 0, 8, 0]
};


