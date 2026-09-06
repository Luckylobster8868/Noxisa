#!/usr/bin/env python3
"""
patch_36_nvme_identify.py — Phase B.1, step 4: persistent Admin SQ/CQ
                             ownership + Identify Controller/Namespace.

Run from kernel/src, same convention as prior patches.

Field-offset facts below were cross-checked against multiple
independent sources (Linux nvme_id_ctrl/nvme_id_ns, Windows
NVME_IDENTIFY_CONTROLLER_DATA/NVME_IDENTIFY_NAMESPACE_DATA, and the
Redox nvmed reference bundle's identify.rs) before writing any code.
One real discrepancy was caught in the process: Redox's
IdentifyControllerData declares `model_no: [u8; 48]`, which does not
match its own padding constant (_4k_pad: [u8; 4096-72] assumes a
72-byte header, which only works if model_no is 40 bytes, not 48 -
2+2+20+48+8=80 != 72). Every other source (Linux, Windows, and the
struct's own implied total size) agrees on 40 bytes. This patch uses
the spec-correct 40-byte model_no field, not Redox's inconsistent
value - Redox and the spec are used as references to check against,
not copied blindly.

Confirmed field offsets used below (byte offsets into the 4096-byte
Identify data structure):
  Identify Controller:
    VID   0..2   (u16 LE) - PCI vendor ID
    SN    4..24  (20 bytes ASCII, space-padded) - serial number
    MN    24..64 (40 bytes ASCII, space-padded) - model number
    FR    64..72 (8 bytes ASCII) - firmware revision
    NN    516..520 (u32 LE) - number of namespaces
  Identify Namespace:
    NSZE  0..8   (u64 LE) - namespace size, logical blocks
    NCAP  8..16  (u64 LE) - namespace capacity, logical blocks
    NUSE  16..24 (u64 LE) - namespace utilization, logical blocks
    NLBAF 25     (u8) - number of LBA formats - 1
    FLBAS 26     (u8, bits 3:0) - active LBA format index
    LBAF[] starts at offset 128, 4 bytes each (confirmed via Redox's
    own running byte-offset comments in identify.rs, cross-checked
    against Linux's struct nvme_id_ns field summation):
      bits 15:0  MS    (metadata size)
      bits 23:16 LBADS (LBA data size, log2 - block size = 1<<LBADS)
      bits 25:24 RP    (relative performance)

Completion Queue Entry status field (DWORD3) bit layout, verified
against 5 independent sources (Microsoft's own bitfield union,
illumos/openzfs nvme_reg.h, gigaherz/nvmewin, FreeBSD's
nvme_completion_is_error macro, and a worked real-world SMART-log
decode example) before this patch was written:
  bits 15:0  CID (command identifier, echoes what was submitted)
  bit  16    P   (phase tag)
  bits 24:17 SC  (status code)
  bits 27:25 SCT (status code type)
  bit  30    M   (more)
  bit  31    DNR (do not retry)
  Success == (SC == 0 && SCT == 0), matching FreeBSD's own
  nvme_completion_is_error() check.

Persistent ownership design (per the "smallest persistent ownership
mechanism, do not duplicate initialization unnecessarily" brief):
a single `static ADMIN_QUEUE: Mutex<Option<AdminQueueState>>`,
following the exact same singleton pattern already used by
hal/registry.rs (Mutex<Option<T>>). ensure_admin_queue() is a no-op
if ADMIN_QUEUE is already Some — it does NOT call init_admin_queues()
again on a second identifytest run in the same boot. It reuses
init_admin_queues() from patch_35 unchanged (single source of truth
for the disable/program/enable sequence) rather than re-implementing
any part of it.

One necessary, in-scope correction to existing code: map_mmio() was
previously only mapping 0x1000 (4KB) of BAR0 in mmiotest/nvmeinittest,
which covers the register block (CAP..ACQ, ending at 0x38) but NOT
the admin doorbell registers, which live at offset 0x1000 onward
(doorbell offset formula: 0x1000 + (2*qid+dir)*(4<<CAP.DSTRD) - admin
SQ tail doorbell is at 0x1000, admin CQ head doorbell at
0x1000 + (4<<DSTRD)). Submitting a command requires ringing the SQ
doorbell, so this patch's persistent-init path maps 0x2000 (8KB) to
safely cover both the register block and the admin doorbells with
headroom. mmiotest/nvmeinittest's existing 0x1000 mappings are left
untouched since they never ring doorbells.

What this does:
  1. drivers/nvme.rs:
     - AdminQueueState struct + static ADMIN_QUEUE: Mutex<Option<..>>
     - ensure_admin_queue() -> Result<(), NvmeInitError>
     - build_identify_cmd() - explicit byte-offset SQE construction
       (no repr(packed) struct + reference, avoiding unaligned-
       reference UB entirely)
     - submit_admin_and_wait() - writes the command into the next SQ
       slot, rings the SQ doorbell, bounded-polls the CQ slot's phase
       bit (CAP.TO-derived timeout, same pattern as patch_35's RDY
       waits), validates CID match and SC/SCT == 0, rings the CQ
       doorbell, returns the completion's DWORD0.
     - IdentifyControllerInfo / IdentifyNamespaceInfo - parsed,
       owned result structs (alloc::string::String for text fields,
       already available - the boot log already proves alloc/Vec/Box
       work in this kernel).
     - identify_controller() / identify_namespace(nsid) - allocate a
       fresh 4KB PRP1 buffer per call (ordinary RAM via
       pmm::alloc_contiguous_frames + phys_to_virt, ordinary reads -
       not MMIO), submit, parse.
     - New NvmeInitError variants: NoController, BarNotMmio,
       CommandTimeout, UnexpectedCid, CommandFailed { sct, sc }.
  2. main.rs: adds "identifytest" to help's command list, and an
     identifytest dispatch arm that calls identify_controller() then
     identify_namespace(1) and prints the parsed fields.

Explicitly NOT in this patch: I/O queues, BlockDevice/BlockError,
MSI-X, interrupts, networking, filesystem integration, GUI. No
Redox userspace daemon/executor architecture is used anywhere here -
Redox's identify.rs was read only as a field-offset cross-check.
"""
import sys

NVME_RS_PATH = "drivers/nvme.rs"
MAIN_RS_PATH = "main.rs"

# ---------------------------------------------------------------------------
# 1. drivers/nvme.rs
# ---------------------------------------------------------------------------

# 1a. Add `use spin::Mutex;` right after the module doc comment block.
IMPORT_OLD = """//! in patch_35 per the approved Phase B.1 progression.

/// NVMe register offsets"""

IMPORT_NEW = """//! in patch_35 per the approved Phase B.1 progression.

use spin::Mutex;

/// NVMe register offsets"""

# 1b. Give NvmeRegs a Clone/Copy derive so it can live inside the
# persistent AdminQueueState by value (it's just a newtype over a
# u64 base address - trivially Copy).
NVMEREGS_OLD = """/// Wraps an already-mapped NVMe MMIO base (a virtual address, as
/// returned by memory::paging::map_mmio()) with volatile register
/// accessors. Does not map anything itself — the caller must call
/// map_mmio() first and pass the returned virtual base in here.
pub struct NvmeRegs {"""

NVMEREGS_NEW = """/// Wraps an already-mapped NVMe MMIO base (a virtual address, as
/// returned by memory::paging::map_mmio()) with volatile register
/// accessors. Does not map anything itself — the caller must call
/// map_mmio() first and pass the returned virtual base in here.
#[derive(Clone, Copy)]
pub struct NvmeRegs {"""

# 1c. Extend NvmeInitError with the new variants needed for command
# submission/polling and controller discovery inside ensure_admin_queue().
INITERR_OLD = """    /// CSTS.CFS (Controller Fatal Status) was set after enabling.
    ControllerFatalStatus,
}"""

INITERR_NEW = """    /// CSTS.CFS (Controller Fatal Status) was set after enabling.
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
}"""

# 1d. Append everything else — persistent state, submission/polling,
# Identify Controller/Namespace — after the existing tail of the file
# (init_admin_queues()'s closing brace from patch_35).
NVME_TAIL_OLD = """    Ok(AdminQueues {
        sq_phys,
        cq_phys,
        sq_virt,
        cq_virt,
        depth,
    })
}"""

NVME_TAIL_NEW = """    Ok(AdminQueues {
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
}"""

# ---------------------------------------------------------------------------
# 2. main.rs — help string + identifytest dispatch arm
# ---------------------------------------------------------------------------

HELP_OLD = 'preempttest drawtest ownertest haltest pcitest mmiotest nvmeinittest\n");'
HELP_NEW = 'preempttest drawtest ownertest haltest pcitest mmiotest nvmeinittest identifytest\n");'

DISPATCH_OLD = """            None => crate::kprintln!("[nvmeinittest] no NVMe controller found - nothing to test"),
        }
    } else if !b.is_empty() {"""

DISPATCH_NEW = """            None => crate::kprintln!("[nvmeinittest] no NVMe controller found - nothing to test"),
        }
    } else if b == b"identifytest" {
        crate::kprintln!("--- NVMe Identify Controller/Namespace test ---");
        match drivers::nvme::identify_controller() {
            Ok(info) => {
                crate::kprintln!("[identifytest] VID   = {:#06x}", info.vid);
                crate::kprintln!("[identifytest] Serial = \\"{}\\"", info.serial);
                crate::kprintln!("[identifytest] Model  = \\"{}\\"", info.model);
                crate::kprintln!("[identifytest] FW Rev = \\"{}\\"", info.firmware);
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
    } else if !b.is_empty() {"""


def read(path):
    with open(path, "r", encoding="utf-8") as f:
        return f.read()


def write(path, content):
    with open(path, "w", encoding="utf-8") as f:
        f.write(content)


def patch_inplace(path, old, new, label):
    src = read(path)
    count = src.count(old)
    if count != 1:
        print(f"[patch_36] {label} match count in {path}: {count}, ABORT: expected exactly 1 match.")
        sys.exit(1)
    write(path, src.replace(old, new, 1))
    print(f"[patch_36] patched {path} ({label})")


def main():
    patch_inplace(NVME_RS_PATH, IMPORT_OLD, IMPORT_NEW, "use spin::Mutex;")
    patch_inplace(NVME_RS_PATH, NVMEREGS_OLD, NVMEREGS_NEW, "NvmeRegs derive(Clone, Copy)")
    patch_inplace(NVME_RS_PATH, INITERR_OLD, INITERR_NEW, "NvmeInitError new variants")
    patch_inplace(NVME_RS_PATH, NVME_TAIL_OLD, NVME_TAIL_NEW, "persistent state + Identify")
    patch_inplace(MAIN_RS_PATH, HELP_OLD, HELP_NEW, "help string")
    patch_inplace(MAIN_RS_PATH, DISPATCH_OLD, DISPATCH_NEW, "identifytest dispatch arm")

    print("[patch_36] OK — persistent admin queue + Identify Controller/Namespace added, identifytest wired up.")
    print("[patch_36] Next: cargo build, then build-iso.sh + QEMU (with -device nvme attached) and run 'identifytest'.")


if __name__ == "__main__":
    main()
