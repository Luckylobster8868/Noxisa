//! Scheduler — O(1) pick_next via priority bitmask
//!
//! 8 priority levels (0 = realtime, 4 = default, 7 = idle).
//! Context switch saves only the 7 SysV ABI callee-saved registers + RSP + RIP.
//! Uses `spin` primitives throughout (no_std, no OS).

extern crate alloc;

use alloc::{boxed::Box, collections::VecDeque, string::String, vec::Vec};
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use spin::{Mutex, RwLock};

// ─── Constants ────────────────────────────────────────────────────────────────

const NPRIO:          usize = 8;
const STACK_ORDER:    usize = 4;      // 2^4 pages = 64 KiB per thread
const DEFAULT_PRIO:   u8    = 4;

// ─── Task state ───────────────────────────────────────────────────────────────

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Runnable = 0,
    Running  = 1,
    Blocked  = 2,
    Zombie   = 3,
}

impl State {
    fn load(atom: &AtomicU8) -> Self {
        match atom.load(Ordering::Acquire) {
            0 => Self::Runnable,
            1 => Self::Running,
            2 => Self::Blocked,
            _ => Self::Zombie,
        }
    }

    fn store(self, atom: &AtomicU8) {
        atom.store(self as u8, Ordering::Release);
    }
}

// ─── CPU context (callee-saved, System-V AMD64 ABI) ───────────────────────────

/// Saved register state for a task not currently running.
/// Layout must match the offsets used in `context_switch`.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct CpuCtx {
    pub r15:    u64,   // offset  0
    pub r14:    u64,   // offset  8
    pub r13:    u64,   // offset 16
    pub r12:    u64,   // offset 24
    pub rbp:    u64,   // offset 32
    pub rbx:    u64,   // offset 40
    pub rsp:    u64,   // offset 48
    pub rip:    u64,   // offset 56
}

// ─── Task Control Block ───────────────────────────────────────────────────────

pub type Pid    = u32;
pub type TaskFn = extern "C" fn() -> !;

/// Cache-line aligned to avoid false sharing across CPUs.
#[repr(C, align(64))]
pub struct Tcb {
    pub pid:      Pid,
    pub state:    AtomicU8,
    pub priority: u8,
    pub _pad:     [u8; 2],
    pub vruntime: AtomicU64,
    pub ctx:      CpuCtx,

    // Stack info
    pub stack_phys: usize,
    pub stack_virt: usize,

    // Per-process security identity (Part 6 item 2) - replaces the former
    // bare globals CURRENT_CAPS (main.rs) and CURRENT_UID/CURRENT_GID
    // (fs/vfs.rs). Every real chokepoint now reads/writes these through
    // Scheduler::caps()/uid()/gid()/set_caps()/set_identity() instead of a
    // single shared static.
    pub caps: u64,   // capability bitmask, deny-all (0) by default
    pub uid:  u32,
    pub gid:  u32,

    // Per-Tcb page table root (Part 6 item 2, isolated-test conversion).
    // Every task spawned via spawn() shares the kernel's own PML4 (read once
    // at spawn time from the live CR3 - all existing tasks run at boot with
    // the kernel's table already active, so this is always correct for them).
    // Tasks built via spawn_isolated() instead store the PML4 produced by
    // create_isolated_user_table(). switch_to() only issues `mov cr3` when
    // this actually differs between outgoing and incoming task, so
    // same-address-space switches (kshell <-> the multitest worker, both
    // kernel-space) are completely unaffected.
    pub cr3: u64,

    // Where isolated_trampoline() should iretq into once it runs as this
    // Tcb (Part 6 item 2). 0/0 for every normal spawn()'d kernel task -
    // only meaningful for tasks built via spawn_isolated(), whose ctx.rip
    // points at isolated_trampoline rather than directly at user code (the
    // trampoline itself still runs in ring 0, on this Tcb's own kernel
    // stack, exactly like any other spawn()'d task's first switch - only
    // once it executes does it perform the ring-3 transition via iretq).
    pub user_entry:      u64,
    pub user_stack_top:  u64,

    // What reap() needs to free for an isolated task once it's Zombie
    // (Part 6 item 2 follow-up: the reclamation gap left open when isotest/
    // captest were converted to spawn_isolated() - table_frames was being
    // mem::forget()'d rather than freed). None/0 for every normal spawn()'d
    // kernel task. isolated_user_stack_phys is the BASE address of the
    // isolated task's ring-3 stack page (not user_stack_top - those differ
    // by one page; teardown_isolated_process() wants the base, matching
    // exactly how teardowntest already calls it).
    pub isolated_table_frames:    Option<Vec<u64>>,
    pub isolated_user_stack_phys: u64,

    // Name for debugging (fixed-size, no heap)
    name: [u8; 32],

    // ── Per-process resources (Part 6 item 1) ──────────────────────────────
    // Now that spawn_program() is generic (Part 6 item "Generic process
    // creation", DONE), a spawned process needs its own working directory
    // and its own fd table rather than sharing kshell's identity globals'
    // former shape (a single bare state). Each field lives per-Tcb, same
    // "storage moves to the Tcb, vfs.rs/callers just wrap an accessor"
    // pattern already used for caps/uid/gid above - not a second,
    // divergent per-process state mechanism.
    pub cwd:         String,
    pub open_files:  Vec<Option<crate::fs::vfs::OpenFile>>,
    pub args:        Vec<String>,
}

impl Tcb {
    pub fn name(&self) -> &str {
        let n = self.name.iter().position(|&b| b == 0).unwrap_or(32);
        core::str::from_utf8(&self.name[..n]).unwrap_or("?")
    }
}

// ─── Priority run queue ───────────────────────────────────────────────────────

struct RunQueue {
    q:    [VecDeque<Pid>; NPRIO],
    mask: u8,   // bitmask — bit i set ↔ q[i] non-empty
}

impl RunQueue {
    fn new() -> Self {
        const EMPTY: VecDeque<Pid> = VecDeque::new();
        Self { q: [EMPTY; NPRIO], mask: 0 }
    }

    /// O(1) enqueue.
    fn push(&mut self, pid: Pid, prio: u8) {
        let p = (prio as usize).min(NPRIO - 1);
        self.q[p].push_back(pid);
        self.mask |= 1 << p;
    }

    /// O(1) dequeue — BSF via `trailing_zeros`.
    fn pop(&mut self) -> Option<Pid> {
        if self.mask == 0 { return None; }
        let p = self.mask.trailing_zeros() as usize;
        let pid = self.q[p].pop_front()?;
        if self.q[p].is_empty() { self.mask &= !(1 << p); }
        Some(pid)
    }
}

// ─── Global scheduler ─────────────────────────────────────────────────────────

/// The scheduler is a singleton living for the lifetime of the kernel.
pub struct Scheduler {
    tasks:    RwLock<Vec<Option<Box<Tcb>>>>,
    rq:       Mutex<RunQueue>,
    current:  AtomicU64,   // current PID (0 = none yet)
    next_pid: AtomicU64,
}

static SCHED: spin::Once<Scheduler> = spin::Once::new();

impl Scheduler {
    /// Return (or initialise) the global scheduler.
    pub fn get() -> &'static Self {
        SCHED.call_once(|| Scheduler {
            tasks:    RwLock::new(Vec::new()),
            rq:       Mutex::new(RunQueue::new()),
            current:  AtomicU64::new(0),
            next_pid: AtomicU64::new(1),
        })
    }

    /// Spawn a new kernel thread. Returns its PID.
    pub fn spawn(&self, name: &str, entry: TaskFn) -> Pid {
        let pid = self.next_pid.fetch_add(1, Ordering::Relaxed) as Pid;

        // Allocate 4 pages (16 KiB) for the kernel-mode stack as a single
        // PHYSICALLY CONTIGUOUS block (Part 4 bug #18 fix). This used to
        // call alloc_frame() four separate times and silently discard
        // three of the four returned addresses, assuming (wrongly) that
        // each call returns the physically-next frame - true only while
        // the allocator's bump-pointer path was the only path ever taken
        // (early boot, nothing yet freed). Once any isolated task had
        // ever been torn down and the free-list held scattered entries,
        // that assumption broke silently: reap()'s matching
        // base+i*PAGE_SIZE free loop could then free physical frames
        // that were never actually reserved for this stack at all -
        // sometimes frames still legitimately owned by something else
        // entirely, producing a real double-free (caught by the PMM's
        // free_frame() double-free guard once it existed).
        let stack_phys = crate::memory::pmm::alloc_contiguous_frames(4)
            .expect("OOM: kernel thread stack (contiguous)");
        let stack_virt = stack_phys +
            crate::memory::paging::hhdm_offset();
        let stack_top = (stack_virt + 4 * crate::memory::pmm::PAGE_SIZE) as usize;

        // Prepare the initial stack so the first context switch jumps to `entry`.
        let rsp = init_stack(stack_top, entry);

        // Read the live CR3 - every spawn() caller today runs with the
        // kernel's own page table active (kshell at boot, multitest's
        // worker spawned from within kshell), so this is always the
        // kernel's PML4 for every task spawn() creates. spawn_isolated()
        // (not yet written) will set a different value instead.
        let cr3_now: u64;
        unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3_now, options(nomem, nostack)); }

        let mut name_arr = [0u8; 32];
        let nb = name.as_bytes();
        let n  = nb.len().min(31);
        name_arr[..n].copy_from_slice(&nb[..n]);

        let tcb = Box::new(Tcb {
            pid,
            state:      AtomicU8::new(State::Runnable as u8),
            priority:   DEFAULT_PRIO,
            _pad:       [0; 2],
            vruntime:   AtomicU64::new(0),
            ctx:        CpuCtx {
                rsp:    rsp as u64,
                rip:    entry as u64,
                ..CpuCtx::default()
            },
            stack_phys: stack_phys as usize,
            stack_virt: stack_virt as usize,
            caps:       0,      // deny-all by default, same as former CURRENT_CAPS init
            uid:        1000,   // same default as former CURRENT_UID init
            gid:        1000,   // same default as former CURRENT_GID init
            cr3:        cr3_now,
            user_entry: 0,
            user_stack_top: 0,
            isolated_table_frames: None,
            isolated_user_stack_phys: 0,
            name:       name_arr,
            cwd:        String::from("/"),
            open_files: Vec::new(),
            args:       Vec::new(),
        });

        {
            let mut tasks = self.tasks.write();
            if tasks.len() <= pid as usize {
                tasks.resize_with(pid as usize + 1, || None);
            }
            tasks[pid as usize] = Some(tcb);
        }

        self.rq.lock().push(pid, DEFAULT_PRIO);
        pid
    }

    /// Scheduler loop — never returns.
    pub fn run(&self) -> ! {
        loop {
            // IMPORTANT: pop() must be its own statement. `if let Some(x) =
            // self.rq.lock().pop() { self.switch_to(x); }` keeps the
            // MutexGuard temporary alive across the whole if-let block
            // (Rust's scrutinee temporary-lifetime rule) - including the
            // switch_to() call. switch_to()'s first-ever invocation does a
            // raw noreturn jmp and never comes back to drop that guard, so
            // `rq` was being left locked FOREVER the moment run() first
            // handed off to kshell. Every later self.rq.lock() anywhere
            // (spawn()'s tail push, yield_now()'s pop) then spun forever on
            // a lock that could never be released - this was the real cause
            // of multitest hanging, not anything in spawn() itself.
            let next = self.rq.lock().pop();
            if let Some(pid) = next {
                self.switch_to(pid);
            } else {
                // No runnable tasks — wait for the next timer interrupt.
                unsafe { core::arch::asm!("hlt", options(nomem, nostack)); }
            }
        }
    }

    fn switch_to(&self, next_pid: Pid) {
        let prev_pid = self.current.swap(next_pid as u64, Ordering::AcqRel) as Pid;
        if prev_pid == next_pid { return; }

        // Lock tasks as briefly as possible to extract raw context pointers.
        let (prev_ctx, next_ctx, next_cr3) = {
            let tasks = self.tasks.read();

            let prev_ctx: *mut CpuCtx = if prev_pid != 0 {
                match tasks.get(prev_pid as usize).and_then(|s| s.as_deref()) {
                    Some(t) => {
                        if State::load(&t.state) == State::Running {
                            State::Runnable.store(&t.state);
                            // Re-enqueue the outgoing task RIGHT NOW, not
                            // after context_switch() returns below. The old
                            // "after it returns" placement only works when
                            // switch_to() is called exclusively from run()'s
                            // own top-level loop. Once yield_now() calls
                            // switch_to() from deep inside a task's own call
                            // stack (multitest), this exact pid would never
                            // be pushed onto rq until it's resumed - and it
                            // can only be resumed by something popping it
                            // FROM rq. That's a genuine deadlock: never
                            // enqueued until resumed, never resumed since
                            // never enqueued. Confirmed by multitest hanging
                            // completely on first real use. Enqueuing here,
                            // before the switch, breaks the cycle.
                            self.rq.lock().push(prev_pid, t.priority);
                        }
                        &t.ctx as *const CpuCtx as *mut CpuCtx
                    }
                    None => core::ptr::null_mut(),
                }
            } else {
                core::ptr::null_mut()
            };

            let (next_ctx, next_cr3): (*const CpuCtx, u64) = match tasks
                .get(next_pid as usize)
                .and_then(|s| s.as_deref())
            {
                Some(t) => {
                    State::Running.store(&t.state);
                    (&t.ctx as *const CpuCtx, t.cr3)
                }
                None => return,
            };

            (prev_ctx, next_ctx, next_cr3)
        };

        // Swap CR3 only if the incoming task actually uses a different page
        // table. Every task today (kshell, the multitest worker) shares the
        // kernel's own PML4, so this comparison is false and `mov cr3` never
        // executes for any currently-existing code path - this is purely
        // prerequisite plumbing for isolated tasks (Part 6 item 2) that
        // don't exist yet. Reading CR3 here rather than trusting a stale
        // "current" cr3 field avoids needing to track the outgoing task's
        // cr3 separately - the live register is always the ground truth for
        // what's active right now.
        let cur_cr3: u64;
        unsafe { core::arch::asm!("mov {}, cr3", out(reg) cur_cr3, options(nomem, nostack)); }
        if cur_cr3 != next_cr3 {
            unsafe { core::arch::asm!("mov cr3, {}", in(reg) next_cr3, options(nomem, nostack)); }
        }

        // Perform the register swap.
        // SAFETY: both pointers point to valid, aligned CpuCtx within Tcb objects
        // that live for the kernel's lifetime.
        unsafe { context_switch(prev_ctx, next_ctx); }

        // No tail re-enqueue anymore - moved above, before the switch (see
        // comment there). Leaving a second push here would double-enqueue
        // the outgoing task once this call eventually resumes.
    }

    /// Spawn a real, isolated Tcb (Part 6 item 2) - the actual conversion
    /// of isotest/captest/etc. from "inline mov-cr3 + one-way jmp in
    /// kshell's own call stack" to a genuine second process the scheduler
    /// can run, yield to, and (once sys_exit fires) switch away from
    /// without halting the machine.
    ///
    /// Deliberately reuses spawn() wholesale rather than duplicating its
    /// stack-allocation/Tcb-construction logic: spawn_isolated() calls
    /// spawn() with entry = isolated_trampoline (a small ring-0 stub
    /// defined in main.rs, next to jump_to_userspace), gets back a fully
    /// normal kernel-mode Tcb exactly like kshell or the multitest worker,
    /// then patches in the three isolation-specific fields (cr3, user_entry,
    /// user_stack_top) and the caller-supplied capability mask. This keeps
    /// every existing spawn() invariant (stack allocation, name, uid/gid
    /// defaults) working unchanged and isolated tasks going through the
    /// exact same run-queue/switch_to()/CR3-swap machinery already proven
    /// safe by the regression sweep.
    ///
    /// `cr3` is the PML4 from create_isolated_user_table(). `user_entry`/
    /// `user_stack_top` are where isolated_trampoline() should iretq once
    /// it actually runs as this Tcb - not used until then, since the first
    /// switch_to() lands on isolated_trampoline in ring 0 on this Tcb's own
    /// (normal, kernel-allocated) stack, same as any other spawn()'d task.
    pub fn spawn_isolated(
        &self,
        name: &str,
        cr3: u64,
        user_entry: u64,
        user_stack_top: u64,
        caps: u64,
        table_frames: Vec<u64>,
        user_stack_phys: u64,
    ) -> Pid {
        let pid = self.spawn(name, crate::isolated_trampoline);

        let mut tasks = self.tasks.write();
        if let Some(Some(t)) = tasks.get_mut(pid as usize) {
            t.cr3 = cr3;
            t.user_entry = user_entry;
            t.user_stack_top = user_stack_top;
            t.caps = caps;
            t.isolated_table_frames = Some(table_frames);
            t.isolated_user_stack_phys = user_stack_phys;
        }
        pid
    }

    pub fn spawn_program(&self, name: &str, elf_data: &[u8], caps: u64) -> Result<Pid, &'static str> {
        unsafe {
            let (entry, pages) = crate::fs::elf::load_fresh(elf_data)
                .map_err(|_| "elf load failed")?;

            let stack_phys = crate::memory::pmm::alloc_frame()
                .ok_or("out of memory allocating stack")?;
            core::ptr::write_bytes(
                crate::memory::paging::phys_to_virt(stack_phys) as *mut u8, 0, 4096,
            );
            let stack_top = stack_phys + 4096;

            let (new_pml4, mut table_frames) =
                crate::memory::paging::create_isolated_user_table_fresh(&pages, stack_phys);

            for &(_, paddr, _, _) in &pages {
                table_frames.push(paddr);
            }

            // Print back the entry page's actual PTE flags - real evidence
            // the per-segment W^X flags landed in the page table, not just
            // that the call compiled. Expect writable=false executable=true
            // for a normal code entry point, now that this is enforced
            // rather than every page being writable+executable.
            if let Some((w, x)) = crate::memory::paging::debug_read_pte_flags(new_pml4, entry) {
                crate::kprintln!("[spawn] entry page {:#x}: writable={} executable={}", entry, w, x);
            }

            let pid = self.spawn_isolated(
                name, new_pml4, entry, stack_top, caps, table_frames, stack_phys,
            );
            Ok(pid)
        }
    }

    /// Free everything a now-Zombie isolated task owns: its page-table
    /// frames + isolated user stack (via the same
    /// memory::paging::teardown_isolated_process() primitive teardowntest
    /// already proved correct), plus its own kernel-mode stack (the one
    /// spawn() allocated for it to run isolated_trampoline on). Part 6 item
    /// 2 follow-up - closes the reclamation gap left open when isotest/
    /// captest were converted to spawn_isolated() (table_frames was being
    /// mem::forget()'d rather than freed).
    ///
    /// Panics if called on a pid that isn't actually Zombie - reaping a
    /// still-running or still-runnable task would free memory out from
    /// under it. Removes the Tcb's slot afterward (set to None) rather than
    /// leaving an inert entry sitting in `tasks` forever.
    pub fn reap(&self, pid: Pid) {
        let mut tasks = self.tasks.write();
        let tcb = match tasks.get_mut(pid as usize).and_then(|s| s.take()) {
            Some(t) => t,
            None => panic!("reap: pid {} has no Tcb", pid),
        };
        drop(tasks);

        if State::load(&tcb.state) != State::Zombie {
            panic!("reap: pid {} is not Zombie - refusing to free a live task's memory", pid);
        }

        if let Some(table_frames) = &tcb.isolated_table_frames {
            unsafe {
                crate::memory::paging::teardown_isolated_process(
                    table_frames, tcb.isolated_user_stack_phys,
                );
            }
        }

        // Free this Tcb's own kernel-mode stack (4 pages, same count
        // spawn() allocates for every task).
        let base = tcb.stack_phys as u64;
        for i in 0..4u64 {
            crate::memory::pmm::free_frame(base + i * crate::memory::pmm::PAGE_SIZE);
        }
    }

    /// Read the current task's user_entry/user_stack_top - called by
    /// isolated_trampoline() once it starts running as a spawn_isolated()'d
    /// Tcb, to find out where to iretq into.
    pub fn current_user_entry_stack(&self) -> (u64, u64) {
        let pid = self.current_pid();
        let tasks = self.tasks.read();
        match tasks.get(pid as usize).and_then(|s| s.as_deref()) {
            Some(t) => (t.user_entry, t.user_stack_top),
            None => panic!("current_user_entry_stack: current pid has no Tcb"),
        }
    }

    /// Terminate the CURRENT task and switch away - never returns.
    ///
    /// Part 6 item 2 (isolated-test conversion prerequisite): sys_exit used
    /// to be `cli; hlt` forever in the raw syscall handler, which was fine
    /// only because isotest/captest/etc. ran inline in kshell's own call
    /// stack with no separate Tcb - halting the CPU was indistinguishable
    /// from "the test finished" since nothing else was ever going to run
    /// again anyway. Once a test becomes a real spawn_isolated() task, that
    /// stops being true: kshell (pid=1) is a separate, still-alive Tcb that
    /// must keep running after the isolated task exits. This is the
    /// minimal-correct version per design principle #6 (smallest testable
    /// step): it marks the exiting task Zombie (never re-enqueued - unlike
    /// switch_to()'s normal Runnable demotion) and switches to the next
    /// runnable task instead of halting. It deliberately does NOT reclaim
    /// the exiting task's stack/frames or remove its Tcb slot yet - that
    /// reaping step is a separate, not-yet-written follow-up (same "known
    /// gap, carried forward honestly" shape as Part 2's earlier teardown
    /// gap), now that scrubtest/teardowntest already proved the underlying
    /// free/scrub primitive works. A Zombie Tcb currently just sits inert
    /// in `tasks`, taking up its slot and never running again.
    pub fn exit_current(&self) -> ! {
        let pid = self.current_pid();

        if pid != 0 {
            let tasks = self.tasks.read();
            if let Some(Some(t)) = tasks.get(pid as usize) {
                State::Zombie.store(&t.state);
            }
        }

        // Same guard-lifetime rule as run()/yield_now(): pop() must be its
        // own statement so the MutexGuard drops before we ever transfer
        // control away and don't come back to this stack frame.
        let next = self.rq.lock().pop();

        let next_ctx: *const CpuCtx = match next {
            Some(next_pid) => {
                self.current.store(next_pid as u64, Ordering::Release);
                let tasks = self.tasks.read();
                match tasks.get(next_pid as usize).and_then(|s| s.as_deref()) {
                    Some(t) => {
                        State::Running.store(&t.state);
                        let next_cr3 = t.cr3;
                        let ctx = &t.ctx as *const CpuCtx;
                        drop(tasks);
                        let cur_cr3: u64;
                        unsafe { core::arch::asm!("mov {}, cr3", out(reg) cur_cr3, options(nomem, nostack)); }
                        if cur_cr3 != next_cr3 {
                            unsafe { core::arch::asm!("mov cr3, {}", in(reg) next_cr3, options(nomem, nostack)); }
                        }
                        ctx
                    }
                    None => panic!("exit_current: next_pid popped from rq has no Tcb"),
                }
            }
            // Nothing else runnable is a genuine kernel-fatal condition here
            // (unlike yield_now()'s degenerate "sole task yields to itself"
            // case) - the exiting task is never coming back, so if there is
            // truly nothing else to run, the kernel has no forward progress
            // left. kshell being spawned at boot means this should be
            // unreachable in practice.
            None => panic!("exit_current: no runnable task left to switch to"),
        };

        // prev = null: this task's registers are never saved - it is
        // Zombie and will never be resumed, so there is nothing worth
        // preserving. This is exactly the "first switch" fast path
        // context_switch() already supports, just reused for the opposite
        // (last switch) reason.
        unsafe { context_switch(core::ptr::null_mut(), next_ctx); }
        unreachable!("context_switch does not return");
    }

    // ─── Per-process security identity accessors (Part 6 item 2) ─────────
    // Replace the former bare globals CURRENT_CAPS/CURRENT_UID/CURRENT_GID.
    // All fall back to the same defaults those globals used to have
    // (caps=0 deny-all, uid=gid=1000) if there's no current task yet (e.g.
    // very early boot, pid=0).

    /// PID of whatever task is currently executing (0 = none yet).
    pub fn current_pid(&self) -> Pid {
        self.current.load(Ordering::Relaxed) as Pid
    }

    /// Whether `pid`'s Tcb currently reports State::Zombie. Used by callers
    /// that spawn a plain (non-isolated) worker and need to wait for it to
    /// genuinely finish - via its own exit_current() call - before it's
    /// safe to reap() it. Returns false if the pid has no Tcb (already
    /// reaped, or never existed), same as "not a zombie you can reap".
    pub fn is_zombie(&self, pid: Pid) -> bool {
        self.tasks.read().get(pid as usize).and_then(|s| s.as_deref())
            .map(|t| State::load(&t.state) == State::Zombie)
            .unwrap_or(false)
    }

    pub fn caps(&self) -> u64 {
        let pid = self.current_pid();
        self.tasks.read().get(pid as usize).and_then(|s| s.as_deref())
            .map(|t| t.caps).unwrap_or(0)
    }

    pub fn set_caps(&self, caps: u64) {
        let pid = self.current_pid();
        if let Some(Some(t)) = self.tasks.write().get_mut(pid as usize) {
            t.caps = caps;
        }
    }

    pub fn uid(&self) -> u32 {
        let pid = self.current_pid();
        self.tasks.read().get(pid as usize).and_then(|s| s.as_deref())
            .map(|t| t.uid).unwrap_or(1000)
    }

    pub fn gid(&self) -> u32 {
        let pid = self.current_pid();
        self.tasks.read().get(pid as usize).and_then(|s| s.as_deref())
            .map(|t| t.gid).unwrap_or(1000)
    }

    pub fn set_identity(&self, uid: u32, gid: u32) {
        let pid = self.current_pid();
        if let Some(Some(t)) = self.tasks.write().get_mut(pid as usize) {
            t.uid = uid;
            t.gid = gid;
        }
    }

    // ── Per-process resources accessors (Part 6 item 1) ─────────────────────
    // Same shape throughout as caps()/uid()/gid() above: look up the
    // current Tcb by current_pid(), read or write the one field, fall back
    // to a sane default if there's no current task yet (mirrors every
    // existing accessor's fallback behaviour).

    pub fn cwd(&self) -> String {
        let pid = self.current_pid();
        self.tasks.read().get(pid as usize).and_then(|s| s.as_deref())
            .map(|t| t.cwd.clone()).unwrap_or_else(|| String::from("/"))
    }

    pub fn set_cwd(&self, path: &str) {
        let pid = self.current_pid();
        if let Some(Some(t)) = self.tasks.write().get_mut(pid as usize) {
            t.cwd = String::from(path);
        }
    }

    pub fn args(&self) -> Vec<String> {
        let pid = self.current_pid();
        self.tasks.read().get(pid as usize).and_then(|s| s.as_deref())
            .map(|t| t.args.clone()).unwrap_or_default()
    }

    pub fn set_args(&self, args: Vec<String>) {
        let pid = self.current_pid();
        if let Some(Some(t)) = self.tasks.write().get_mut(pid as usize) {
            t.args = args;
        }
    }

    /// Allocate a fd for `of` on the current Tcb: reuses the first `None`
    /// slot (a closed fd) if one exists, otherwise appends a new slot.
    /// Real Unix fd-reuse behaviour (lowest-available-number), not just
    /// monotonically growing - matters once open/close/open cycles happen
    /// repeatedly on a long-lived process rather than once per test run.
    pub fn alloc_fd(&self, of: crate::fs::vfs::OpenFile) -> i32 {
        let pid = self.current_pid();
        let mut tasks = self.tasks.write();
        let t = match tasks.get_mut(pid as usize).and_then(|s| s.as_deref_mut()) {
            Some(t) => t,
            None => return -1,   // no current task - degenerate case, same
                                  // "no-op fallback" shape as the other
                                  // accessors above rather than a panic.
        };
        if let Some(slot) = t.open_files.iter().position(|s| s.is_none()) {
            t.open_files[slot] = Some(of);
            slot as i32
        } else {
            t.open_files.push(Some(of));
            (t.open_files.len() - 1) as i32
        }
    }

    /// Snapshot of an open fd's (path, offset, flags), or None if `fd`
    /// isn't currently open on the caller's Tcb.
    pub fn fd_info(&self, fd: i32) -> Option<(String, usize, u8)> {
        if fd < 0 { return None; }
        let pid = self.current_pid();
        self.tasks.read().get(pid as usize).and_then(|s| s.as_deref())
            .and_then(|t| t.open_files.get(fd as usize))
            .and_then(|slot| slot.as_ref())
            .map(|of| (of.path.clone(), of.offset, of.flags))
    }

    pub fn set_fd_offset(&self, fd: i32, offset: usize) {
        if fd < 0 { return; }
        let pid = self.current_pid();
        if let Some(Some(t)) = self.tasks.write().get_mut(pid as usize) {
            if let Some(Some(of)) = t.open_files.get_mut(fd as usize) {
                of.offset = offset;
            }
        }
    }

    /// Close `fd`, freeing its slot for reuse by a later alloc_fd(). Returns
    /// false for an already-closed or out-of-range fd (double-close is not
    /// treated as an error at this layer - callers that care can check the
    /// return value).
    pub fn close_fd(&self, fd: i32) -> bool {
        if fd < 0 { return false; }
        let pid = self.current_pid();
        if let Some(Some(t)) = self.tasks.write().get_mut(pid as usize) {
            if let Some(slot) = t.open_files.get_mut(fd as usize) {
                if slot.is_some() {
                    *slot = None;
                    return true;
                }
            }
        }
        false
    }

    /// Number of currently-open fds on the caller's Tcb - used by restest
    /// to prove a freshly spawned process's fd table starts genuinely
    /// empty, not inherited from whichever process spawned it.
    pub fn open_fd_count(&self) -> usize {
        let pid = self.current_pid();
        self.tasks.read().get(pid as usize).and_then(|s| s.as_deref())
            .map(|t| t.open_files.iter().filter(|s| s.is_some()).count())
            .unwrap_or(0)
    }
    /// Called from the timer ISR (irq0_timer) to attempt a preemptive
    /// context switch. Deliberately non-blocking end to end: every lock
    /// touched uses try_lock/try_read, and on ANY contention this tick is
    /// skipped entirely (no state is mutated) rather than spinning. The CPU
    /// entered via an interrupt gate, so RFLAGS.IF is 0 for the whole
    /// duration of this call - a blocking .lock() here could spin forever
    /// on a lock that can only be released by code that will never run
    /// again on this core. try_lock() failing just means "skip this 10ms
    /// tick" - harmless, the next tick tries again.
    pub fn try_preempt(&self) {
        let Some(tasks_guard) = self.tasks.try_read() else { return };
        drop(tasks_guard);

        let Some(mut rq_guard) = self.rq.try_lock() else { return };
        let next = rq_guard.pop();
        drop(rq_guard);

        if let Some(next_pid) = next {
            self.switch_to(next_pid);
        }
    }

}

/// Yield the CPU from the current kernel thread back to the scheduler.
///
/// Part 6 item 2 (multitasking prerequisite): this used to only flip a
/// state atomic and rely on "the timer interrupt or explicit yield
/// returns to the scheduler loop" - but there is no real preemption yet
/// (nothing unmasks the PIC/issues sti - see Part 2's known gaps) and
/// switch_to() was, before this fix, ONLY ever called from run()'s own
/// loop - meaning this function never actually switched anything. A
/// second spawned task would sit on the run queue forever, since nothing
/// ever popped it. Fixed by directly popping the run queue and calling
/// switch_to() here, reusing the same already-proven context_switch
/// machinery run() uses - switch_to() itself already demotes the caller
/// from Running back to Runnable and re-enqueues it once this call
/// eventually returns (i.e. once some future switch_to() resumes this
/// exact pid again), so no manual state bookkeeping is needed here.
pub fn yield_now() {
    let sched = Scheduler::get();
    // Same guard-lifetime fix as run(): pop() must be its own statement so
    // the MutexGuard drops before switch_to() is called, not after (it may
    // never come back to drop it, if this call never returns to this
    // caller until much later - see run()'s comment for the full story).
    let next = sched.rq.lock().pop();
    if let Some(next_pid) = next {
        sched.switch_to(next_pid);
    }
    // Nothing else runnable - legitimate degenerate case (sole runnable
    // task yielding to itself): just return, caller continues normally.
}

/// Block until any child thread exits (simplified PID-1 wait loop).
pub fn wait_any() {
    unsafe { core::arch::asm!("hlt", options(nomem, nostack)); }
}

// ─── Stack setup ──────────────────────────────────────────────────────────────

/// Write `entry` as a fake return address so the first `context_switch` into
/// this task will effectively do `jmp entry`.
fn init_stack(stack_top: usize, entry: TaskFn) -> usize {
    // Align to 16 bytes, then subtract 8 for the return address.
    let rsp = (stack_top & !0xF) - 8;
    // SAFETY: rsp is within the allocated stack.
    unsafe { (rsp as *mut u64).write(entry as u64); }
    rsp
}

// ─── Context switch (x86_64 assembly) ─────────────────────────────────────────

/// Save callee-saved registers into `*prev` and restore from `*next`.
/// If `prev` is null (first switch) we just restore.
///
/// # Safety
/// Both pointers must point to valid, aligned `CpuCtx` values.
unsafe fn context_switch(prev: *mut CpuCtx, next: *const CpuCtx) {
    if prev.is_null() {
        // First switch: no context to save — just jump into `next`.
        //
        // No out() clobber declarations here - `options(noreturn)` forbids
        // ANY output operands at all. This is fine specifically because
        // both real callers of this branch (run()'s very first switch,
        // and exit_current()) never return to this exact call site, so a
        // clobbered caller-side register can never be observed afterward -
        // no correctness issue, unlike the "normal switch" branch below.
        unsafe {
            core::arch::asm!(
                "mov rsp, qword ptr [{n} + 48]",
                // sti immediately before the jmp, not after: x86 defers
                // interrupt recognition until after the NEXT instruction
                // following sti (the "one instruction shadow"), so the
                // jmp itself still completes atomically before any
                // pending IRQ is taken. Without this, a task first
                // entered here while control was inside the timer ISR
                // would start running with RFLAGS.IF=0 permanently - the
                // only path that ever restores IF=1 is the ORIGINAL
                // interrupted task's own eventual real iretq, which
                // every OTHER task reached via this raw jmp never takes.
                "sti",
                "jmp qword ptr [{n} + 56]",
                n = in(reg) next,
                options(nostack, noreturn),
            );
        }
    }

    // Normal switch: save current, restore next.
    //
    // CRITICAL FIX: r15/r14/r13/r12/rbp/rbx are written via hard-coded
    // register names in the restore sequence below, not through named
    // asm! operands - Rust's inline-asm contract requires those to be
    // declared as clobbers or the compiler is free to keep a live Rust
    // value in any of them across this call, trusting they survive
    // untouched (exactly as it would for an ordinary function call
    // preserving callee-saved registers). Without that, this asm silently
    // stomped whatever the compiler had parked there - confirmed live via
    // gdb + addr2line: a #PF at address 0x0 inside core::fmt::write,
    // immediately after a yield_now() resume, reading a clobbered rbx as
    // a base pointer. It never surfaced before because no prior call
    // site's surrounding code had enough live locals to pressure the
    // compiler into using one of these six registers across the call -
    // isotest's larger local set (pid, stack_phys, stack_top, new_pml4,
    // etc.) finally did.
    //
    // r15/r14/r13/r12 can be declared as ordinary out("reg") _ clobbers.
    // rbx/rbp CANNOT - LLVM hard-forbids using either as an inline-asm
    // clobber (rbp is the frame-pointer register; rbx is reserved for
    // LLVM's own internal codegen use). The standard, correct technique
    // real kernels use instead: push them onto the outgoing stack right
    // before saving *prev (so the saved rsp correctly points just past
    // them), then pop them back immediately after the "55:" resume label.
    // Each task's own earlier-pushed values sit on *its own* stack,
    // popped back exactly when it resumes - the CpuCtx.rbp/.rbx struct
    // fields are still written/read too (kept for symmetry/debugging),
    // but the push/pop pair is what actually satisfies LLVM's requirement
    // and is the authoritative restore for any task resuming a second time
    // or later (a brand-new task's very first resume lands directly at its
    // entry function, not at "55:", so the struct-field mov is what sets
    // its initial rbp/rbx in that case - never popped, which is fine,
    // exactly like any fresh thread's registers being unspecified at
    // startup).
    unsafe {
        core::arch::asm!(
            "push rbp",
            "push rbx",
            // ── Save to *prev ──────────────────────────────────────────
            "mov  qword ptr [{p}     ], r15",
            "mov  qword ptr [{p} +  8], r14",
            "mov  qword ptr [{p} + 16], r13",
            "mov  qword ptr [{p} + 24], r12",
            "mov  qword ptr [{p} + 32], rbp",
            "mov  qword ptr [{p} + 40], rbx",
            "mov  qword ptr [{p} + 48], rsp",
            "lea  rax, [rip + 55f]",
            "mov  qword ptr [{p} + 56], rax",
            // ── Restore from *next ──────────────────────────────────────
            "mov  r15, qword ptr [{n}     ]",
            "mov  r14, qword ptr [{n} +  8]",
            "mov  r13, qword ptr [{n} + 16]",
            "mov  r12, qword ptr [{n} + 24]",
            "mov  rbp, qword ptr [{n} + 32]",
            "mov  rbx, qword ptr [{n} + 40]",
            "mov  rsp, qword ptr [{n} + 48]",
            // Same reasoning as the first-switch branch above: sti right
            // before jmp so ANY task resumed here (not just fresh ones)
            // starts back up with interrupts enabled, regardless of
            // whether the code that called context_switch() got here
            // cooperatively (IF was already 1) or from inside the timer
            // ISR (IF was 0 the whole time we've been in interrupt
            // context). Redundant but harmless in the cooperative case;
            // load-bearing in the preemptive one.
            "sti",
            "jmp  qword ptr [{n} + 56]",
            "55:",
            "pop  rbx",
            "pop  rbp",
            p   = in(reg) prev,
            n   = in(reg) next,
            out("rax") _,
            out("r15") _, out("r14") _, out("r13") _, out("r12") _,
        );
    }
}
