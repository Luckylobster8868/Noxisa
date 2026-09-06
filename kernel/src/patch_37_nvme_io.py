#!/usr/bin/env python3
"""
patch_37_nvme_io.py — Phase B.1, step 5: I/O Submission/Completion
                       Queues + Read/Write NVM commands.

Run from kernel/src, same convention as prior patches.

Command field layouts below were cross-checked against Redox's own
nvmed command-builder functions (storage/nvmed/src/nvme/cmd.rs) before
writing any code:

  Create I/O Completion Queue (admin opcode 0x05):
    CDW10 = (SIZE << 16) | QID          - SIZE is 0's-based (entries-1)
    CDW11 = PC | (IEN << 1) | (IV << 16) - PC=1 (contiguous), IEN=0
                                            (polling only, no MSI-X in
                                            this patch), IV unused
    PRP1  = physical address of CQ memory

  Create I/O Submission Queue (admin opcode 0x01):
    CDW10 = (SIZE << 16) | QID          - SIZE is 0's-based
    CDW11 = (CQID << 16) | PC            - PC=1
    CDW12 = 0 (NVM Set ID - unused, base spec)
    PRP1  = physical address of SQ memory

  Read (I/O opcode 0x02) / Write (I/O opcode 0x01):
    CDW10 = SLBA[31:0]
    CDW11 = SLBA[63:32]
    CDW12 = NLB (0's-based number of logical blocks) in bits 15:0
    PRP1  = physical address of the data buffer

The "SIZE"/"NLB" 0's-based convention was confirmed not just from the
spec but from Redox's own call sites (mod.rs's create_io_completion_queue
computes `raw_len = actual_len.checked_sub(1)` before calling the
command builder - i.e. the size passed into the command is already
entries-1), removing any off-by-one ambiguity.

Scope, per explicit instruction: transfers are restricted to <= 4096
bytes (a single page), using PRP1 alone - no PRP list / multi-page
support is implemented in this patch. This is a deliberate constraint
to avoid a half-baked PRP-list implementation, not an oversight.

Persistent ownership: a second singleton, IoQueueState, following the
exact same Mutex<Option<T>> pattern as ADMIN_QUEUE (patch_36) and
hal/registry.rs. ensure_io_queue() is a no-op if IO_QUEUE is already
Some - it does not recreate the I/O queues on repeated calls. It
calls ensure_admin_queue() first (I/O queue creation commands are
submitted THROUGH the admin queue), reusing that function unchanged.

Command submission for the I/O queue uses a new submit_io_and_wait(),
structurally similar to patch_36's submit_admin_and_wait() but
operating on IoQueueState's own tail/head/phase/doorbells - written
as a separate function rather than refactoring submit_admin_and_wait()
into a shared generic helper, to avoid any risk of touching or
regressing the already-verified admin command path. This is the same
kind of small, deliberate duplication already present in this
codebase (e.g. patch_35's separate disable-wait and enable-wait
loops, rather than one shared generic waiter).

What this does:
  1. drivers/nvme.rs:
     - IoQueueState + static IO_QUEUE: Mutex<Option<..>>
     - ensure_io_queue(nsid) -> Result<(), NvmeInitError>
     - build_create_io_cq_cmd() / build_create_io_sq_cmd()
     - build_rw_cmd() - shared builder for both Read and Write
     - submit_io_and_wait()
     - io_write_blocks(nsid, lba, block_size, data) -> Result<(), NvmeInitError>
     - io_read_blocks(nsid, lba, block_size, out) -> Result<(), NvmeInitError>
     - New NvmeInitError variant: InvalidTransferSize
  2. main.rs: adds "nvmeiotest" to help's command list, and a
     nvmeiotest dispatch arm that calls identify_namespace(1) for the
     real block size, writes a known pattern to LBA 0, reads it back,
     and compares byte-for-byte.

Explicitly NOT in this patch: BlockDevice/BlockError (patch_38),
multi-page PRP lists, MSI-X/interrupts, filesystem integration,
partition handling, networking, GUI.
"""
import sys

NVME_RS_PATH = "drivers/nvme.rs"
MAIN_RS_PATH = "main.rs"

# ---------------------------------------------------------------------------
# 1. drivers/nvme.rs
# ---------------------------------------------------------------------------

INITERR_OLD = """    /// The controller reported a command failure. sct/sc are the raw
    /// Status Code Type / Status Code from the completion, per the
    /// NVMe Base Specification's status code tables.
    CommandFailed { sct: u8, sc: u8 },
}"""

INITERR_NEW = """    /// The controller reported a command failure. sct/sc are the raw
    /// Status Code Type / Status Code from the completion, per the
    /// NVMe Base Specification's status code tables.
    CommandFailed { sct: u8, sc: u8 },
    /// The requested read/write transfer size is zero, not a whole
    /// multiple of the namespace's block size, or exceeds the single
    /// 4KB page this patch supports (no PRP list implemented yet).
    InvalidTransferSize,
}"""

NVME_TAIL_OLD = """    Ok(IdentifyNamespaceInfo {
        nsze_blocks,
        ncap_blocks,
        nuse_blocks,
        active_lba_format_idx: active_idx,
        block_size,
    })
}"""

NVME_TAIL_NEW = """    Ok(IdentifyNamespaceInfo {
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
}"""

# ---------------------------------------------------------------------------
# 2. main.rs — help string + nvmeiotest dispatch arm
# ---------------------------------------------------------------------------

HELP_OLD = 'preempttest drawtest ownertest haltest pcitest mmiotest nvmeinittest identifytest\n");'
HELP_NEW = 'preempttest drawtest ownertest haltest pcitest mmiotest nvmeinittest identifytest nvmeiotest\n");'

DISPATCH_OLD = """            Err(e) => crate::kprintln!("[identifytest] FAIL - identify_controller returned {:?}", e),
        }
    } else if !b.is_empty() {"""

DISPATCH_NEW = """            Err(e) => crate::kprintln!("[identifytest] FAIL - identify_controller returned {:?}", e),
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
        print(f"[patch_37] {label} match count in {path}: {count}, ABORT: expected exactly 1 match.")
        sys.exit(1)
    write(path, src.replace(old, new, 1))
    print(f"[patch_37] patched {path} ({label})")


def main():
    patch_inplace(NVME_RS_PATH, INITERR_OLD, INITERR_NEW, "NvmeInitError::InvalidTransferSize")
    patch_inplace(NVME_RS_PATH, NVME_TAIL_OLD, NVME_TAIL_NEW, "I/O queue + read/write")
    patch_inplace(MAIN_RS_PATH, HELP_OLD, HELP_NEW, "help string")
    patch_inplace(MAIN_RS_PATH, DISPATCH_OLD, DISPATCH_NEW, "nvmeiotest dispatch arm")

    print("[patch_37] OK — I/O queue creation + read/write + nvmeiotest added.")
    print("[patch_37] Next: cargo build, then build-iso.sh + QEMU (with -device nvme attached) and run 'nvmeiotest'.")


if __name__ == "__main__":
    main()
