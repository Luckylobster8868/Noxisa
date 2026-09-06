//! NVMe register representation (Phase B.1, patch_34)
//!
//! Register offsets and a minimal volatile-access wrapper over the
//! MMIO region mapped by memory::paging::map_mmio(). This patch only
//! covers register *layout* plus a read-only smoke test (mmiotest) —
//! no admin queue setup, no CC/CSTS enable sequencing yet. Those come
//! in patch_35 per the approved Phase B.1 progression.

use spin::Mutex;

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
#[derive(Clone, Copy)]
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
    unsafe fn write32(&self, off: u64, val: u32) {
        unsafe { core::ptr::write_volatile((self.base + off) as *mut u32, val) }
    }

    #[inline]
    unsafe fn write64(&self, off: u64, val: u64) {
        unsafe { core::ptr::write_volatile((self.base + off) as *mut u64, val) }
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

/// Errors bringing up the admin queues / enabling the controller.
/// Deliberately local to this module rather than a project-wide
/// BlockError — that type (and the BlockDevice trait it belongs to)
/// is introduced in patch_38 once there's an actual block I/O path
/// to attach it to; nothing here should anticipate that shape yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvmeInitError {
    /// CAP.MPSMIN requires a host memory page size larger than the
    /// 4KB pages this kernel's PMM currently hands out. Supporting
    /// that would mean multi-frame-aligned queue allocation, out of
    /// scope for this patch.
    PageSizeUnsupported,
    /// The requested admin queue depth exceeds CAP.MQES (the
    /// controller's maximum supported queue size).
    QueueDepthUnsupported,
    /// Physically contiguous frame allocation for SQ or CQ memory
    /// failed (PMM out of memory).
    AllocFailed,
    /// CSTS.RDY did not clear to 0 within the CAP.TO-derived timeout
    /// after CC.EN was cleared.
    DisableTimeout,
    /// CSTS.RDY did not set to 1 within the CAP.TO-derived timeout
    /// after CC.EN was set.
    EnableTimeout,
    /// CSTS.CFS (Controller Fatal Status) was set after enabling.
    ControllerFatalStatus,
    /// No NVMe controller was found on the PCI bus.
    NoController,
    /// The NVMe controller's BAR0 is not an MMIO BAR (unexpected -
    /// NVMe controllers never use I/O-space BARs per spec, but this
    /// is checked rather than assumed).
    BarNotMmio,
    /// A submitted admin command's completion did not appear within
    /// the CAP.TO-derived timeout.
    CommandTimeout,
    /// A completion's CID did not match the command that was
    /// submitted - would indicate a queue-management bug (out of
    /// scope to recover from here; treated as fatal for this call).
    UnexpectedCid,
    /// The controller reported a command failure. sct/sc are the raw
    /// Status Code Type / Status Code from the completion, per the
    /// NVMe Base Specification's status code tables.
    CommandFailed { sct: u8, sc: u8 },
    /// The requested read/write transfer size is zero, not a whole
    /// multiple of the namespace's block size, or exceeds the single
    /// 4KB page this patch supports (no PRP list implemented yet).
    InvalidTransferSize,
}

/// Physical + virtual addresses and depth of the allocated Admin
/// Submission/Completion Queues. Just the addresses for now — no
/// head/tail tracking or command submission yet (that lands with
/// Identify Controller/Namespace in patch_36).
#[derive(Debug, Clone, Copy)]
pub struct AdminQueues {
    pub sq_phys: u64,
    pub cq_phys: u64,
    pub sq_virt: u64,
    pub cq_virt: u64,
    pub depth: u16,
}

/// Fixed by the NVMe Base Specification for the standard NVM command
/// set — NOT read from CAP. Admin queue entries are always 64 bytes
/// (submission) / 16 bytes (completion), regardless of what
/// CC.IOSQES/IOCQES end up set to for I/O queues later.
const ADMIN_SQE_SIZE: u64 = 64;
const ADMIN_CQE_SIZE: u64 = 16;

/// CC.CSS value for the NVM command set. Only used after confirming
/// CAP.CSS bit 0 (absolute bit 37) is actually set by the caller's
/// controller — see init_admin_queues().
const CC_CSS_NVM: u32 = 0;
/// CC.AMS value for round-robin arbitration — unconditionally
/// guaranteed supported per spec regardless of CAP.AMS, so this is
/// not a fact CAP needs to confirm.
const CC_AMS_ROUND_ROBIN: u32 = 0;
/// CC.SHN value for "no shutdown notification" — correct while
/// bringing the controller up, not shutting it down.
const CC_SHN_NONE: u32 = 0;
/// Spec-mandated standard entry-size exponents for the NVM command
/// set (2^6 = 64 bytes SQE, 2^4 = 16 bytes CQE) — fixed values, not
/// CAP-derived, matching Linux's NVME_NVM_IOSQES/IOCQES constants.
const CC_IOSQES_STANDARD: u32 = 6;
const CC_IOCQES_STANDARD: u32 = 4;

/// Bring up the Admin Submission/Completion Queues and enable the
/// controller, following the NVMe Base Specification's controller
/// initialization sequence: disable -> wait RDY=0 -> program
/// AQA/ASQ/ACQ -> set CC (enable) -> wait RDY=1.
///
/// Every CC field CAP can determine (CSS, MPS) is read from the real
/// CAP value passed through `regs`, not assumed; AMS/SHN use
/// spec-guaranteed-safe defaults; IOSQES/IOCQES use the spec-mandated
/// standard sizes for the NVM command set (see constants above).
///
/// `regs` must already be backed by a real MMIO mapping (i.e. its
/// virtual base came from memory::paging::map_mmio()).
pub fn init_admin_queues(regs: &NvmeRegs, depth: u16) -> Result<AdminQueues, NvmeInitError> {
    let cap = regs.cap();
    let mqes = (cap & 0xFFFF) as u32; // 0's-based max queue size
    let mpsmin = ((cap >> 48) & 0xF) as u32;
    let to_500ms_units = ((cap >> 24) & 0xFF) as u64;
    let timeout_ms = core::cmp::max(to_500ms_units * 500, 500); // never less than 500ms

    if (depth as u32).saturating_sub(1) > mqes {
        return Err(NvmeInitError::QueueDepthUnsupported);
    }
    if mpsmin != 0 {
        return Err(NvmeInitError::PageSizeUnsupported);
    }

    // --- Allocate physically contiguous Admin SQ/CQ memory ---
    // These are ordinary RAM frames from the PMM, not MMIO —
    // phys_to_virt() is valid for them directly (unlike BAR0, which
    // needed map_mmio() because it's a PCI MMIO hole, not a memmap
    // region Limine's HHDM covers under base revision 3).
    let sq_needed = (depth as u64) * ADMIN_SQE_SIZE;
    let cq_needed = (depth as u64) * ADMIN_CQE_SIZE;
    let sq_pages = ((sq_needed + 0xFFF) / 0x1000).max(1) as usize;
    let cq_pages = ((cq_needed + 0xFFF) / 0x1000).max(1) as usize;

    let sq_phys = crate::memory::pmm::alloc_contiguous_frames(sq_pages)
        .ok_or(NvmeInitError::AllocFailed)?;
    let cq_phys = crate::memory::pmm::alloc_contiguous_frames(cq_pages)
        .ok_or(NvmeInitError::AllocFailed)?;

    let sq_virt = crate::memory::paging::phys_to_virt(sq_phys);
    let cq_virt = crate::memory::paging::phys_to_virt(cq_phys);
    unsafe {
        core::ptr::write_bytes(sq_virt as *mut u8, 0, sq_pages * 0x1000);
        core::ptr::write_bytes(cq_virt as *mut u8, 0, cq_pages * 0x1000);
    }

    // --- Disable the controller, wait for CSTS.RDY == 0 ---
    unsafe { regs.write32(offset::CC, 0) };
    let deadline_ms = crate::drivers::pit::ms() + timeout_ms;
    loop {
        if regs.status_raw() & 0x1 == 0 {
            break;
        }
        if crate::drivers::pit::ms() >= deadline_ms {
            return Err(NvmeInitError::DisableTimeout);
        }
        core::hint::spin_loop();
    }

    // --- Program AQA / ASQ / ACQ (only while EN == 0, per spec) ---
    let depth0 = (depth as u32).saturating_sub(1) & 0xFFF;
    let aqa: u32 = depth0 | (depth0 << 16);
    unsafe {
        regs.write32(offset::AQA, aqa);
        regs.write64(offset::ASQ, sq_phys);
        regs.write64(offset::ACQ, cq_phys);
    }

    // --- Construct CC from CAP-confirmed + spec-mandated fields ---
    let cc: u32 = 1 // EN
        | (CC_CSS_NVM << 4)
        | (mpsmin << 7) // MPS — CAP.MPSMIN already confirmed == 0 above
        | (CC_AMS_ROUND_ROBIN << 11)
        | (CC_SHN_NONE << 14)
        | (CC_IOSQES_STANDARD << 16)
        | (CC_IOCQES_STANDARD << 20);
    unsafe { regs.write32(offset::CC, cc) };

    // --- Wait for CSTS.RDY == 1 (bail immediately on CFS) ---
    let deadline_ms = crate::drivers::pit::ms() + timeout_ms;
    loop {
        let csts = regs.status_raw();
        if csts & 0x2 != 0 {
            return Err(NvmeInitError::ControllerFatalStatus);
        }
        if csts & 0x1 != 0 {
            break;
        }
        if crate::drivers::pit::ms() >= deadline_ms {
            return Err(NvmeInitError::EnableTimeout);
        }
        core::hint::spin_loop();
    }

    Ok(AdminQueues {
        sq_phys,
        cq_phys,
        sq_virt,
        cq_virt,
        depth,
    })
}

/// Persistent handle to a live, already-initialized Admin SQ/CQ pair,
/// including everything needed to submit further admin commands
/// (doorbell addresses, tail/head tracking, phase bit, next command
/// ID). Created once by ensure_admin_queue() and reused thereafter -
/// see that function's doc comment.
struct AdminQueueState {
    regs: NvmeRegs,
    sq_virt: u64,
    cq_virt: u64,
    sq_tail: u16,
    cq_head: u16,
    phase: bool,
    depth: u16,
    sq_doorbell: u64,
    cq_doorbell: u64,
    next_cid: u16,
}

/// Singleton admin queue state, following the same Mutex<Option<T>>
/// pattern already used by hal/registry.rs for its device wrappers.
static ADMIN_QUEUE: Mutex<Option<AdminQueueState>> = Mutex::new(None);

/// Ensure the Admin SQ/CQ have been created and the controller is
/// enabled, initializing them on first use only. If ADMIN_QUEUE is
/// already Some, this returns Ok(()) immediately without touching
/// the controller again - re-running init_admin_queues() on a live
/// controller would reset it and orphan any in-flight state, so this
/// must not happen on repeated calls within the same boot.
///
/// Maps 0x2000 (8KB) of BAR0, not mmiotest/nvmeinittest's 0x1000 -
/// submitting commands requires ringing the SQ/CQ doorbells, which
/// live at MMIO offset 0x1000 onward (see module doc comment), past
/// what the register-only mapping covered.
fn ensure_admin_queue() -> Result<(), NvmeInitError> {
    let mut guard = ADMIN_QUEUE.lock();
    if guard.is_some() {
        return Ok(());
    }

    let dev = crate::drivers::pci::find_nvme().ok_or(NvmeInitError::NoController)?;
    let bar = dev.bar0.ok_or(NvmeInitError::BarNotMmio)?;
    if !bar.is_mmio {
        return Err(NvmeInitError::BarNotMmio);
    }

    let virt = unsafe { crate::memory::paging::map_mmio(bar.base, 0x2000) };
    let regs = unsafe { NvmeRegs::new(virt) };
    let aq = init_admin_queues(&regs, 64)?;

    let dstrd = regs.doorbell_stride();
    let sq_doorbell = virt + 0x1000;
    let cq_doorbell = virt + 0x1000 + ((4u64) << dstrd);

    *guard = Some(AdminQueueState {
        regs,
        sq_virt: aq.sq_virt,
        cq_virt: aq.cq_virt,
        sq_tail: 0,
        cq_head: 0,
        phase: true, // per spec: first round of completions posts with P=1
        depth: aq.depth,
        sq_doorbell,
        cq_doorbell,
        next_cid: 0,
    });
    Ok(())
}

/// Build a raw 64-byte Identify (opcode 0x06) admin command. Field
/// layout is the NVMe Base Specification's Common Command Format,
/// cross-checked against Redox's nvmed NvmeCmd struct field order
/// (opcode/flags/cid/nsid/reserved/mptr/dptr[2]/cdw10../cdw15).
/// Written via explicit byte offsets rather than a repr(packed)
/// struct + reference, which avoids ever taking a reference to an
/// unaligned field (undefined behaviour in Rust).
///
///   offset 0:  opcode (u8) = 0x06 (Identify)
///   offset 1:  flags (u8) = 0 (no fused op, PRP-based PSDT)
///   offset 2:  cid (u16)
///   offset 4:  nsid (u32)
///   offset 8:  reserved (u64) = 0
///   offset 16: mptr (u64) = 0 (no metadata)
///   offset 24: prp1 (u64) - physical address of the 4KB result buffer
///   offset 32: prp2 (u64) = 0 (result is exactly one 4KB page - a
///              single PRP entry is sufficient per spec, no PRP list
///              or second PRP entry needed)
///   offset 40: cdw10 (u32) = CNS (0x01 = Identify Controller,
///              0x00 = Identify Namespace for the namespace in NSID)
///   offset 44..64: cdw11..cdw15 = 0 (unused for CNS 0x00/0x01)
fn build_identify_cmd(cid: u16, nsid: u32, cns: u32, prp1: u64) -> [u8; 64] {
    let mut buf = [0u8; 64];
    buf[0] = 0x06;
    buf[2..4].copy_from_slice(&cid.to_le_bytes());
    buf[4..8].copy_from_slice(&nsid.to_le_bytes());
    buf[24..32].copy_from_slice(&prp1.to_le_bytes());
    buf[40..44].copy_from_slice(&cns.to_le_bytes());
    buf
}

/// Submit a raw 64-byte admin command and bounded-poll for its
/// completion. Advances sq_tail/cq_head and rings both doorbells.
/// Returns the completion's DWORD0 (command-specific) on success.
fn submit_admin_and_wait(
    state: &mut AdminQueueState,
    cmd_bytes: &[u8; 64],
) -> Result<u32, NvmeInitError> {
    let cid = state.next_cid;
    state.next_cid = state.next_cid.wrapping_add(1);

    debug_assert_eq!(
        u16::from_le_bytes([cmd_bytes[2], cmd_bytes[3]]),
        cid,
        "caller must build the command with this cid before calling"
    );

    let slot = (state.sq_virt + (state.sq_tail as u64) * 64) as *mut u8;
    unsafe { core::ptr::copy_nonoverlapping(cmd_bytes.as_ptr(), slot, 64) };

    state.sq_tail = (state.sq_tail + 1) % state.depth;
    unsafe { core::ptr::write_volatile(state.sq_doorbell as *mut u32, state.sq_tail as u32) };

    let cap = state.regs.cap();
    let to_500ms_units = ((cap >> 24) & 0xFF) as u64;
    let timeout_ms = core::cmp::max(to_500ms_units * 500, 500);
    let deadline_ms = crate::drivers::pit::ms() + timeout_ms;

    loop {
        let entry_base = (state.cq_virt + (state.cq_head as u64) * 16) as *const u8;
        let dw3 = unsafe { core::ptr::read_volatile(entry_base.add(12) as *const u32) };
        let phase_bit = (dw3 >> 16) & 0x1 != 0;

        if phase_bit == state.phase {
            let dw0 = unsafe { core::ptr::read_volatile(entry_base as *const u32) };
            let cid_recv = (dw3 & 0xFFFF) as u16;
            let sc = ((dw3 >> 17) & 0xFF) as u8;
            let sct = ((dw3 >> 25) & 0x7) as u8;

            state.cq_head = (state.cq_head + 1) % state.depth;
            if state.cq_head == 0 {
                state.phase = !state.phase;
            }
            unsafe {
                core::ptr::write_volatile(state.cq_doorbell as *mut u32, state.cq_head as u32)
            };

            if cid_recv != cid {
                return Err(NvmeInitError::UnexpectedCid);
            }
            if sc != 0 || sct != 0 {
                return Err(NvmeInitError::CommandFailed { sct, sc });
            }
            return Ok(dw0);
        }

        if crate::drivers::pit::ms() >= deadline_ms {
            return Err(NvmeInitError::CommandTimeout);
        }
        core::hint::spin_loop();
    }
}

/// Parsed subset of the Identify Controller data structure (see
/// module doc comment for the verified byte offsets used).
#[derive(Debug, Clone)]
pub struct IdentifyControllerInfo {
    pub vid: u16,
    pub serial: alloc::string::String,
    pub model: alloc::string::String,
    pub firmware: alloc::string::String,
    pub num_namespaces: u32,
}

/// Parsed subset of the Identify Namespace data structure (see
/// module doc comment for the verified byte offsets used).
#[derive(Debug, Clone, Copy)]
pub struct IdentifyNamespaceInfo {
    pub nsze_blocks: u64,
    pub ncap_blocks: u64,
    pub nuse_blocks: u64,
    pub active_lba_format_idx: u8,
    /// Decoded block size in bytes from the active LBA format's
    /// LBADS field (1 << LBADS), if LBADS falls in a sane range
    /// (9..=31, i.e. 512 bytes .. 2GiB). None if the value looks
    /// implausible rather than trusting it blindly.
    pub block_size: Option<u64>,
}

fn ascii_field_to_string(bytes: &[u8]) -> alloc::string::String {
    // Built via String::from(&str) rather than .to_string() - the
    // ToString trait isn't in scope in this no_std crate (it's not
    // part of core's prelude, only std's), and From<&str> for String
    // is available directly since alloc::string::String is already
    // used throughout this codebase (e.g. audit.rs).
    let text: alloc::string::String = bytes
        .iter()
        .map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' })
        .collect();
    alloc::string::String::from(text.trim())
}

/// Issue Identify Controller (CNS=0x01) and parse the fields listed
/// in this module's doc comment. Calls ensure_admin_queue() first -
/// safe to call repeatedly; only initializes the controller once.
pub fn identify_controller() -> Result<IdentifyControllerInfo, NvmeInitError> {
    ensure_admin_queue()?;
    let mut guard = ADMIN_QUEUE.lock();
    let state = guard.as_mut().expect("ensure_admin_queue() just succeeded");

    let buf_phys =
        crate::memory::pmm::alloc_contiguous_frames(1).ok_or(NvmeInitError::AllocFailed)?;
    let buf_virt = crate::memory::paging::phys_to_virt(buf_phys);
    unsafe { core::ptr::write_bytes(buf_virt as *mut u8, 0, 0x1000) };

    let cmd = build_identify_cmd(state.next_cid, 0, 0x01, buf_phys);
    submit_admin_and_wait(state, &cmd)?;

    // Ordinary RAM, not MMIO - no volatile/barrier needed beyond what
    // the rest of this kernel already assumes for DMA-written memory
    // (same simplification as the admin queue memory in patch_35).
    let buf: &[u8] = unsafe { core::slice::from_raw_parts(buf_virt as *const u8, 0x1000) };

    let vid = u16::from_le_bytes([buf[0], buf[1]]);
    let serial = ascii_field_to_string(&buf[4..24]);
    let model = ascii_field_to_string(&buf[24..64]);
    let firmware = ascii_field_to_string(&buf[64..72]);
    let num_namespaces = u32::from_le_bytes([buf[516], buf[517], buf[518], buf[519]]);

    Ok(IdentifyControllerInfo {
        vid,
        serial,
        model,
        firmware,
        num_namespaces,
    })
}

/// Issue Identify Namespace (CNS=0x00) for the given NSID and parse
/// the fields listed in this module's doc comment. Calls
/// ensure_admin_queue() first - safe to call repeatedly.
pub fn identify_namespace(nsid: u32) -> Result<IdentifyNamespaceInfo, NvmeInitError> {
    ensure_admin_queue()?;
    let mut guard = ADMIN_QUEUE.lock();
    let state = guard.as_mut().expect("ensure_admin_queue() just succeeded");

    let buf_phys =
        crate::memory::pmm::alloc_contiguous_frames(1).ok_or(NvmeInitError::AllocFailed)?;
    let buf_virt = crate::memory::paging::phys_to_virt(buf_phys);
    unsafe { core::ptr::write_bytes(buf_virt as *mut u8, 0, 0x1000) };

    let cmd = build_identify_cmd(state.next_cid, nsid, 0x00, buf_phys);
    submit_admin_and_wait(state, &cmd)?;

    let buf: &[u8] = unsafe { core::slice::from_raw_parts(buf_virt as *const u8, 0x1000) };

    let nsze_blocks = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let ncap_blocks = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    let nuse_blocks = u64::from_le_bytes(buf[16..24].try_into().unwrap());
    let flbas = buf[26];
    let active_idx = flbas & 0xF;

    let lbaf_off = 128 + (active_idx as usize) * 4;
    let lbaf_raw = u32::from_le_bytes(buf[lbaf_off..lbaf_off + 4].try_into().unwrap());
    let lbads = ((lbaf_raw >> 16) & 0xFF) as u8;
    let block_size = if (9..=31).contains(&lbads) {
        Some(1u64 << lbads)
    } else {
        None
    };

    Ok(IdentifyNamespaceInfo {
        nsze_blocks,
        ncap_blocks,
        nuse_blocks,
        active_lba_format_idx: active_idx,
        block_size,
    })
}

/// Persistent handle to a live I/O Submission/Completion Queue pair
/// (queue ID 1), created once by ensure_io_queue() and reused
/// thereafter. Structurally mirrors AdminQueueState but is tracked
/// separately since it's a different queue with its own tail/head/
/// phase/doorbells.
struct IoQueueState {
    sq_virt: u64,
    cq_virt: u64,
    sq_tail: u16,
    cq_head: u16,
    phase: bool,
    depth: u16,
    sq_doorbell: u64,
    cq_doorbell: u64,
    next_cid: u16,
    nsid: u32,
}

/// Singleton I/O queue state, same Mutex<Option<T>> pattern as
/// ADMIN_QUEUE and hal/registry.rs.
static IO_QUEUE: Mutex<Option<IoQueueState>> = Mutex::new(None);

/// Clear both persistent singletons (ADMIN_QUEUE, IO_QUEUE) back to
/// None, forcing the next call that needs them to fully reinitialize
/// against the controller's actual current state.
///
/// This exists because nvmeinittest (see main.rs) deliberately calls
/// init_admin_queues() directly, independent of ensure_admin_queue(),
/// to prove that raw function works standalone. That call resets the
/// real controller and reprograms its admin queue addresses - if
/// ADMIN_QUEUE/IO_QUEUE were already populated from an earlier
/// identifytest/nvmeiotest call, they would otherwise keep pointing
/// at addresses the controller no longer recognizes, causing the
/// next command to time out. main.rs's nvmeinittest dispatch arm
/// calls this immediately after its raw init_admin_queues() call.
pub fn invalidate_persistent_state() {
    *ADMIN_QUEUE.lock() = None;
    *IO_QUEUE.lock() = None;
}

/// I/O queue ID used throughout this module. Only one I/O SQ/CQ pair
/// is created (QID 1) - this patch does not implement multiple I/O
/// queues.
const IO_QID: u16 = 1;
/// Depth (entries) of the I/O SQ/CQ. Small and fixed for this patch -
/// no need for anything larger while only ever one command is
/// outstanding at a time.
const IO_QUEUE_DEPTH: u16 = 16;

fn build_create_io_cq_cmd(cid: u16, qid: u16, size0based: u16, ptr: u64) -> [u8; 64] {
    let mut buf = [0u8; 64];
    buf[0] = 0x05; // admin opcode: Create I/O Completion Queue
    buf[2..4].copy_from_slice(&cid.to_le_bytes());
    buf[24..32].copy_from_slice(&ptr.to_le_bytes());
    let cdw10 = ((size0based as u32) << 16) | (qid as u32);
    buf[40..44].copy_from_slice(&cdw10.to_le_bytes());
    let cdw11: u32 = 0x1; // PC=1 (physically contiguous), IEN=0 (polling only)
    buf[44..48].copy_from_slice(&cdw11.to_le_bytes());
    buf
}

fn build_create_io_sq_cmd(cid: u16, qid: u16, size0based: u16, ptr: u64, cqid: u16) -> [u8; 64] {
    let mut buf = [0u8; 64];
    buf[0] = 0x01; // admin opcode: Create I/O Submission Queue
    buf[2..4].copy_from_slice(&cid.to_le_bytes());
    buf[24..32].copy_from_slice(&ptr.to_le_bytes());
    let cdw10 = ((size0based as u32) << 16) | (qid as u32);
    buf[40..44].copy_from_slice(&cdw10.to_le_bytes());
    let cdw11: u32 = ((cqid as u32) << 16) | 0x1; // CQID | PC=1
    buf[44..48].copy_from_slice(&cdw11.to_le_bytes());
    buf
}

/// Build a raw 64-byte Read (I/O opcode 0x02) or Write (I/O opcode
/// 0x01) command. `nlb0based` is the 0's-based block count (spec
/// convention - 0 means 1 block). Single PRP1 only, per this patch's
/// <=4KB transfer restriction.
fn build_rw_cmd(opcode: u8, cid: u16, nsid: u32, lba: u64, nlb0based: u16, prp1: u64) -> [u8; 64] {
    let mut buf = [0u8; 64];
    buf[0] = opcode;
    buf[2..4].copy_from_slice(&cid.to_le_bytes());
    buf[4..8].copy_from_slice(&nsid.to_le_bytes());
    buf[24..32].copy_from_slice(&prp1.to_le_bytes());
    buf[40..44].copy_from_slice(&(lba as u32).to_le_bytes());
    buf[44..48].copy_from_slice(&((lba >> 32) as u32).to_le_bytes());
    buf[48..52].copy_from_slice(&(nlb0based as u32).to_le_bytes());
    buf
}

/// Ensure the I/O Submission/Completion Queue pair (QID 1) has been
/// created, creating it on first use only. No-op if IO_QUEUE is
/// already Some. Calls ensure_admin_queue() first since queue
/// creation commands are submitted through the admin queue.
fn ensure_io_queue(nsid: u32) -> Result<(), NvmeInitError> {
    {
        let guard = IO_QUEUE.lock();
        if guard.is_some() {
            return Ok(());
        }
    }

    ensure_admin_queue()?;

    let sq_needed = (IO_QUEUE_DEPTH as u64) * ADMIN_SQE_SIZE;
    let cq_needed = (IO_QUEUE_DEPTH as u64) * ADMIN_CQE_SIZE;
    let sq_pages = ((sq_needed + 0xFFF) / 0x1000).max(1) as usize;
    let cq_pages = ((cq_needed + 0xFFF) / 0x1000).max(1) as usize;

    let sq_phys =
        crate::memory::pmm::alloc_contiguous_frames(sq_pages).ok_or(NvmeInitError::AllocFailed)?;
    let cq_phys =
        crate::memory::pmm::alloc_contiguous_frames(cq_pages).ok_or(NvmeInitError::AllocFailed)?;
    let sq_virt = crate::memory::paging::phys_to_virt(sq_phys);
    let cq_virt = crate::memory::paging::phys_to_virt(cq_phys);
    unsafe {
        core::ptr::write_bytes(sq_virt as *mut u8, 0, sq_pages * 0x1000);
        core::ptr::write_bytes(cq_virt as *mut u8, 0, cq_pages * 0x1000);
    }

    let (dstrd, bar0_virt) = {
        let mut admin_guard = ADMIN_QUEUE.lock();
        let admin_state = admin_guard
            .as_mut()
            .expect("ensure_admin_queue() just succeeded");

        let size0 = IO_QUEUE_DEPTH - 1;
        let cq_cmd = build_create_io_cq_cmd(admin_state.next_cid, IO_QID, size0, cq_phys);
        submit_admin_and_wait(admin_state, &cq_cmd)?;

        let sq_cmd =
            build_create_io_sq_cmd(admin_state.next_cid, IO_QID, size0, sq_phys, IO_QID);
        submit_admin_and_wait(admin_state, &sq_cmd)?;

        let dstrd = admin_state.regs.doorbell_stride();
        // sq_doorbell for QID 0 was computed as bar0_virt + 0x1000
        // (see ensure_admin_queue()) - recover bar0_virt from it
        // rather than storing it a second time.
        let bar0_virt = admin_state.sq_doorbell - 0x1000;
        (dstrd, bar0_virt)
    };

    let stride = 4u64 << dstrd;
    let sq_doorbell = bar0_virt + 0x1000 + (2 * (IO_QID as u64)) * stride;
    let cq_doorbell = bar0_virt + 0x1000 + (2 * (IO_QID as u64) + 1) * stride;

    let mut guard = IO_QUEUE.lock();
    *guard = Some(IoQueueState {
        sq_virt,
        cq_virt,
        sq_tail: 0,
        cq_head: 0,
        phase: true,
        depth: IO_QUEUE_DEPTH,
        sq_doorbell,
        cq_doorbell,
        next_cid: 0,
        nsid,
    });
    Ok(())
}

/// Submit a raw 64-byte I/O command and bounded-poll for its
/// completion. Structurally mirrors submit_admin_and_wait() but
/// operates on IoQueueState - see module doc comment for why this is
/// a separate function rather than a shared generic helper.
fn submit_io_and_wait(
    state: &mut IoQueueState,
    cmd_bytes: &[u8; 64],
    timeout_ms: u64,
) -> Result<u32, NvmeInitError> {
    let cid = state.next_cid;
    state.next_cid = state.next_cid.wrapping_add(1);

    debug_assert_eq!(
        u16::from_le_bytes([cmd_bytes[2], cmd_bytes[3]]),
        cid,
        "caller must build the command with this cid before calling"
    );

    let slot = (state.sq_virt + (state.sq_tail as u64) * 64) as *mut u8;
    unsafe { core::ptr::copy_nonoverlapping(cmd_bytes.as_ptr(), slot, 64) };

    state.sq_tail = (state.sq_tail + 1) % state.depth;
    unsafe { core::ptr::write_volatile(state.sq_doorbell as *mut u32, state.sq_tail as u32) };

    let deadline_ms = crate::drivers::pit::ms() + timeout_ms;

    loop {
        let entry_base = (state.cq_virt + (state.cq_head as u64) * 16) as *const u8;
        let dw3 = unsafe { core::ptr::read_volatile(entry_base.add(12) as *const u32) };
        let phase_bit = (dw3 >> 16) & 0x1 != 0;

        if phase_bit == state.phase {
            let dw0 = unsafe { core::ptr::read_volatile(entry_base as *const u32) };
            let cid_recv = (dw3 & 0xFFFF) as u16;
            let sc = ((dw3 >> 17) & 0xFF) as u8;
            let sct = ((dw3 >> 25) & 0x7) as u8;

            state.cq_head = (state.cq_head + 1) % state.depth;
            if state.cq_head == 0 {
                state.phase = !state.phase;
            }
            unsafe {
                core::ptr::write_volatile(state.cq_doorbell as *mut u32, state.cq_head as u32)
            };

            if cid_recv != cid {
                return Err(NvmeInitError::UnexpectedCid);
            }
            if sc != 0 || sct != 0 {
                return Err(NvmeInitError::CommandFailed { sct, sc });
            }
            return Ok(dw0);
        }

        if crate::drivers::pit::ms() >= deadline_ms {
            return Err(NvmeInitError::CommandTimeout);
        }
        core::hint::spin_loop();
    }
}

/// Timeout budget shared by I/O commands, derived from the admin
/// queue's CAP.TO the same way patch_35/36 derive it for admin
/// commands (CAP is a controller-wide register, not per-queue).
fn io_timeout_ms() -> u64 {
    let admin_guard = ADMIN_QUEUE.lock();
    let admin_state = admin_guard.as_ref().expect("ensure_admin_queue() already ran");
    let cap = admin_state.regs.cap();
    let to_500ms_units = ((cap >> 24) & 0xFF) as u64;
    core::cmp::max(to_500ms_units * 500, 500)
}

/// Write `data` to `lba` on namespace `nsid`. `data.len()` must be a
/// non-zero whole multiple of `block_size` and at most 4096 bytes
/// (single-page PRP1-only transfer - see module doc comment).
pub fn io_write_blocks(
    nsid: u32,
    lba: u64,
    block_size: u64,
    data: &[u8],
) -> Result<(), NvmeInitError> {
    ensure_io_queue(nsid)?;
    if data.is_empty() || block_size == 0 || data.len() as u64 % block_size != 0 || data.len() > 0x1000
    {
        return Err(NvmeInitError::InvalidTransferSize);
    }
    let nlb0based = ((data.len() as u64 / block_size) - 1) as u16;

    let buf_phys =
        crate::memory::pmm::alloc_contiguous_frames(1).ok_or(NvmeInitError::AllocFailed)?;
    let buf_virt = crate::memory::paging::phys_to_virt(buf_phys);
    unsafe {
        core::ptr::write_bytes(buf_virt as *mut u8, 0, 0x1000);
        core::ptr::copy_nonoverlapping(data.as_ptr(), buf_virt as *mut u8, data.len());
    }

    let timeout_ms = io_timeout_ms();
    let mut guard = IO_QUEUE.lock();
    let state = guard.as_mut().expect("ensure_io_queue() just succeeded");
    let cmd = build_rw_cmd(0x01, state.next_cid, nsid, lba, nlb0based, buf_phys);
    submit_io_and_wait(state, &cmd, timeout_ms)?;
    Ok(())
}

/// Read `out.len()` bytes from `lba` on namespace `nsid` into `out`.
/// `out.len()` must be a non-zero whole multiple of `block_size` and
/// at most 4096 bytes (single-page PRP1-only transfer).
pub fn io_read_blocks(
    nsid: u32,
    lba: u64,
    block_size: u64,
    out: &mut [u8],
) -> Result<(), NvmeInitError> {
    ensure_io_queue(nsid)?;
    if out.is_empty() || block_size == 0 || out.len() as u64 % block_size != 0 || out.len() > 0x1000
    {
        return Err(NvmeInitError::InvalidTransferSize);
    }
    let nlb0based = ((out.len() as u64 / block_size) - 1) as u16;

    let buf_phys =
        crate::memory::pmm::alloc_contiguous_frames(1).ok_or(NvmeInitError::AllocFailed)?;
    let buf_virt = crate::memory::paging::phys_to_virt(buf_phys);
    unsafe { core::ptr::write_bytes(buf_virt as *mut u8, 0, 0x1000) };

    let timeout_ms = io_timeout_ms();
    let mut guard = IO_QUEUE.lock();
    let state = guard.as_mut().expect("ensure_io_queue() just succeeded");
    let cmd = build_rw_cmd(0x02, state.next_cid, nsid, lba, nlb0based, buf_phys);
    submit_io_and_wait(state, &cmd, timeout_ms)?;
    drop(guard);

    unsafe {
        core::ptr::copy_nonoverlapping(buf_virt as *const u8, out.as_mut_ptr(), out.len());
    }
    Ok(())
}
