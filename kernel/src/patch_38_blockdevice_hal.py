#!/usr/bin/env python3
"""
patch_38_blockdevice_hal.py — Phase B.1, step 6: BlockDevice/BlockError
                               HAL abstraction over the proven NVMe I/O
                               path.

Run from kernel/src, same convention as prior patches.

This is the adapter/abstraction patch: it does NOT add any new NVMe
functionality (patch_37's I/O path is already fully proven) - it
wraps that already-working path behind a domain-agnostic BlockDevice
trait, following the exact same pattern already established by
hal/console.rs, hal/input.rs, hal/display.rs (a zero-sized adapter
struct forwarding to the real subsystem's free functions, registered
via hal::registry's Mutex<Option<&'static dyn Trait>> pattern). The
registry's own existing doc comment in hal/registry.rs already
anticipated this exact addition ("this is the shape that generalises
correctly once a genuinely different device class (NVMe/BlockDevice)
needs registering").

What this does:
  1. hal/block.rs (new):
     - BlockError enum: Timeout, InvalidLba, ControllerError, NotReady
       (the exact 4-variant shape from the original design doc).
     - BlockDevice: Sync trait: block_size(), block_count(),
       read_block(lba, buf), write_block(lba, buf) - the exact
       interface from the original design doc.
     - NvmeBlockDevice: a zero-sized adapter (same shape as
       SerialConsole/FramebufferDisplay) forwarding to
       drivers::nvme::io_read_blocks/io_write_blocks/identify_namespace.
       Namespace 1 is hardcoded (matches every other NVMe test so far
       in this codebase, which all target NSID 1).
     - Real LBA bounds-checking: read_block/write_block call
       identify_namespace(1) to get nsze_blocks and return
       BlockError::InvalidLba if lba >= nsze_blocks, BEFORE issuing
       any NVMe command. This is the actual point of doing the check
       at this layer rather than leaving it to the driver.
     - NvmeInitError -> BlockError mapping: CommandTimeout -> Timeout;
       NoController/BarNotMmio -> NotReady; everything else
       (AllocFailed, DisableTimeout, EnableTimeout,
       ControllerFatalStatus, UnexpectedCid, CommandFailed,
       InvalidTransferSize, PageSizeUnsupported,
       QueueDepthUnsupported) -> ControllerError. The upper layer only
       ever needs to understand 4 outcomes, never NVMe-specific detail.
     - block_size()/block_count() call identify_namespace(1) fresh
       each time, same as nvmeiotest already does - no caching
       introduced in this patch (deliberately deferred, not an
       oversight - namespace geometry becoming persistent/cached state
       is a later optimization once something actually hammers this
       layer, e.g. a filesystem).
  2. hal/registry.rs: adds a BLOCK slot (Mutex<Option<&'static dyn
     BlockDevice>>) and register_block()/block_size()/block_count()/
     block_read()/block_write() wrappers, matching the exact style of
     the existing console/display wrappers.
  3. hal/mod.rs: registers a static NvmeBlockDevice in hal::init(),
     alongside the existing console/input/display registrations. The
     slot stays None-able (an Option) if no NVMe controller is
     present - hal::init() itself does not probe for one; absence is
     handled naturally by the lazy per-call error path
     (NvmeInitError::NoController -> BlockError::NotReady) the first
     time something actually tries to use it, exactly like every
     other lazy-init path already in this codebase.
  4. main.rs: adds "blockdevtest" to help's command list, and a
     blockdevtest dispatch arm that re-proves patch_37's write->read->
     memcmp sequence, but exclusively through
     hal::registry::block_write()/block_read() - never calling
     drivers::nvme directly - proving the abstraction is a genuine
     pass-through rather than dead structure, the same spirit as
     patch_23's haltest.

Explicitly NOT in this patch: multiple BlockDevice implementations
(AHCI/virtio), namespace geometry caching, filesystem integration,
partition handling, MSI-X/interrupts, networking, GUI.
"""
import sys

HAL_BLOCK_PATH = "hal/block.rs"
HAL_MOD_PATH = "hal/mod.rs"
HAL_REGISTRY_PATH = "hal/registry.rs"
MAIN_RS_PATH = "main.rs"

# ---------------------------------------------------------------------------
# 1. hal/block.rs — new file
# ---------------------------------------------------------------------------

HAL_BLOCK_CONTENT = r"""//! Block-device HAL abstraction (Phase B.1, patch_38).
//!
//! This wraps the already-proven NVMe I/O path (drivers::nvme,
//! patch_37) behind a domain-agnostic interface, the same way
//! console.rs/input.rs/display.rs wrap serial/keyboard/gpu. Once this
//! exists, code above the HAL can be written against BlockDevice, not
//! against NVMe specifically - a future AHCI or virtio-blk driver
//! could register a second implementation without any caller-side
//! change.

/// Block-layer error type. Deliberately small and NVMe-agnostic -
/// callers above this layer should never need to understand an NVMe
/// status code; see NvmeBlockDevice's error mapping below for how
/// drivers::nvme::NvmeInitError collapses into these four outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    /// The underlying command did not complete within its timeout.
    Timeout,
    /// The requested LBA is outside the device's reported capacity.
    InvalidLba,
    /// The underlying controller reported a failure (covers
    /// everything from allocation failures to command-specific
    /// status errors - NVMe-specific detail is intentionally not
    /// exposed at this layer).
    ControllerError,
    /// No backing device is available (e.g. no NVMe controller was
    /// found on the bus).
    NotReady,
}

/// A device exposing storage as fixed-size, randomly addressable
/// blocks. NvmeBlockDevice below is the only implementor for now.
pub trait BlockDevice: Sync {
    fn block_size(&self) -> u32;
    fn block_count(&self) -> u64;
    fn read_block(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError>;
    fn write_block(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError>;
}

/// Zero-sized adapter over drivers::nvme - owns no state of its own,
/// same shape as SerialConsole/FramebufferDisplay. All real state
/// (controller registers, queues, namespace info) already lives in
/// drivers::nvme's own persistent singletons (patch_36/37); this
/// struct only translates BlockDevice calls into the already-proven
/// nvme:: functions and maps their errors down to BlockError.
///
/// Namespace 1 is hardcoded, matching every NVMe test in this
/// codebase so far (identifytest, nvmeiotest) - this patch does not
/// add multi-namespace support.
pub struct NvmeBlockDevice;

fn map_nvme_error(e: crate::drivers::nvme::NvmeInitError) -> BlockError {
    use crate::drivers::nvme::NvmeInitError;
    match e {
        NvmeInitError::CommandTimeout => BlockError::Timeout,
        NvmeInitError::NoController | NvmeInitError::BarNotMmio => BlockError::NotReady,
        NvmeInitError::AllocFailed
        | NvmeInitError::DisableTimeout
        | NvmeInitError::EnableTimeout
        | NvmeInitError::ControllerFatalStatus
        | NvmeInitError::UnexpectedCid
        | NvmeInitError::CommandFailed { .. }
        | NvmeInitError::InvalidTransferSize
        | NvmeInitError::PageSizeUnsupported
        | NvmeInitError::QueueDepthUnsupported => BlockError::ControllerError,
    }
}

const NSID: u32 = 1;

impl BlockDevice for NvmeBlockDevice {
    fn block_size(&self) -> u32 {
        match crate::drivers::nvme::identify_namespace(NSID) {
            Ok(ns) => ns.block_size.unwrap_or(0) as u32,
            Err(_) => 0,
        }
    }

    fn block_count(&self) -> u64 {
        match crate::drivers::nvme::identify_namespace(NSID) {
            Ok(ns) => ns.nsze_blocks,
            Err(_) => 0,
        }
    }

    fn read_block(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let ns = crate::drivers::nvme::identify_namespace(NSID).map_err(map_nvme_error)?;
        if lba >= ns.nsze_blocks {
            return Err(BlockError::InvalidLba);
        }
        let block_size = ns.block_size.ok_or(BlockError::ControllerError)?;
        crate::drivers::nvme::io_read_blocks(NSID, lba, block_size, buf).map_err(map_nvme_error)
    }

    fn write_block(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        let ns = crate::drivers::nvme::identify_namespace(NSID).map_err(map_nvme_error)?;
        if lba >= ns.nsze_blocks {
            return Err(BlockError::InvalidLba);
        }
        let block_size = ns.block_size.ok_or(BlockError::ControllerError)?;
        crate::drivers::nvme::io_write_blocks(NSID, lba, block_size, buf).map_err(map_nvme_error)
    }
}
"""

# ---------------------------------------------------------------------------
# 2. hal/registry.rs — add BLOCK slot + wrappers
# ---------------------------------------------------------------------------

REGISTRY_IMPORTS_OLD = """use super::console::Console;
use super::input::InputDevice;
use super::display::DisplayDevice;"""

REGISTRY_IMPORTS_NEW = """use super::console::Console;
use super::input::InputDevice;
use super::display::DisplayDevice;
use super::block::{BlockDevice, BlockError};"""

REGISTRY_SLOTS_OLD = """static CONSOLE: Mutex<Option<&'static dyn Console>> = Mutex::new(None);
static INPUT: Mutex<Option<&'static dyn InputDevice>> = Mutex::new(None);
static DISPLAY: Mutex<Option<&'static dyn DisplayDevice>> = Mutex::new(None);"""

REGISTRY_SLOTS_NEW = """static CONSOLE: Mutex<Option<&'static dyn Console>> = Mutex::new(None);
static INPUT: Mutex<Option<&'static dyn InputDevice>> = Mutex::new(None);
static DISPLAY: Mutex<Option<&'static dyn DisplayDevice>> = Mutex::new(None);
static BLOCK: Mutex<Option<&'static dyn BlockDevice>> = Mutex::new(None);"""

REGISTRY_REGFN_OLD = """pub fn register_display(dev: &'static dyn DisplayDevice) {
    *DISPLAY.lock() = Some(dev);
}"""

REGISTRY_REGFN_NEW = """pub fn register_display(dev: &'static dyn DisplayDevice) {
    *DISPLAY.lock() = Some(dev);
}

pub fn register_block(dev: &'static dyn BlockDevice) {
    *BLOCK.lock() = Some(dev);
}"""

REGISTRY_WRAPFN_OLD = """pub fn display_fill_rect(x: u32, y: u32, w: u32, h: u32, colour: u32) -> bool {
    if let Some(d) = *DISPLAY.lock() {
        d.fill_rect(x, y, w, h, colour);
        true
    } else {
        false
    }
}"""

REGISTRY_WRAPFN_NEW = """pub fn display_fill_rect(x: u32, y: u32, w: u32, h: u32, colour: u32) -> bool {
    if let Some(d) = *DISPLAY.lock() {
        d.fill_rect(x, y, w, h, colour);
        true
    } else {
        false
    }
}

/// Returns 0 if no block device is registered - matches how
/// display_dimensions()/input_poll_char() degrade gracefully via
/// Option rather than panicking when a device class is absent.
pub fn block_size() -> u32 {
    BLOCK.lock().map(|d| d.block_size()).unwrap_or(0)
}

pub fn block_count() -> u64 {
    BLOCK.lock().map(|d| d.block_count()).unwrap_or(0)
}

pub fn block_read(lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
    match *BLOCK.lock() {
        Some(d) => d.read_block(lba, buf),
        None => Err(BlockError::NotReady),
    }
}

pub fn block_write(lba: u64, buf: &[u8]) -> Result<(), BlockError> {
    match *BLOCK.lock() {
        Some(d) => d.write_block(lba, buf),
        None => Err(BlockError::NotReady),
    }
}"""

# ---------------------------------------------------------------------------
# 3. hal/mod.rs — register NvmeBlockDevice
# ---------------------------------------------------------------------------

HAL_MOD_OLD = """pub mod console;
pub mod input;
pub mod display;
pub mod registry;

use console::SerialConsole;
use input::Ps2Keyboard;
use display::FramebufferDisplay;

static SERIAL_CONSOLE: SerialConsole = SerialConsole;
static PS2_KEYBOARD: Ps2Keyboard = Ps2Keyboard;
static FB_DISPLAY: FramebufferDisplay = FramebufferDisplay;

/// Registers the existing Noxisa drivers against the HAL registry.
/// Must run after the underlying drivers (serial, keyboard, gpu) have
/// already been initialised directly by kernel_main_native -- this does
/// not initialise hardware itself, it only wraps what's already up.
pub fn init() {
    registry::register_console(&SERIAL_CONSOLE);
    registry::register_input(&PS2_KEYBOARD);
    registry::register_display(&FB_DISPLAY);
    crate::kprintln!("[boot] HAL ready - console/input/display registered");
}"""

HAL_MOD_NEW = """pub mod console;
pub mod input;
pub mod display;
pub mod block;
pub mod registry;

use console::SerialConsole;
use input::Ps2Keyboard;
use display::FramebufferDisplay;
use block::NvmeBlockDevice;

static SERIAL_CONSOLE: SerialConsole = SerialConsole;
static PS2_KEYBOARD: Ps2Keyboard = Ps2Keyboard;
static FB_DISPLAY: FramebufferDisplay = FramebufferDisplay;
static NVME_BLOCK: NvmeBlockDevice = NvmeBlockDevice;

/// Registers the existing Noxisa drivers against the HAL registry.
/// Must run after the underlying drivers (serial, keyboard, gpu) have
/// already been initialised directly by kernel_main_native -- this does
/// not initialise hardware itself, it only wraps what's already up.
///
/// NvmeBlockDevice is registered unconditionally, even if no NVMe
/// controller is actually present - it does no PCI probing itself.
/// Absence is handled lazily: the first real read_block()/write_block()
/// call will surface NvmeInitError::NoController, mapped to
/// BlockError::NotReady, exactly like every other lazy-init path in
/// this codebase (ensure_admin_queue() etc.). This keeps hal::init()
/// itself simple and matches the registry's Option-based design,
/// which is meant to degrade gracefully when a device class is absent.
pub fn init() {
    registry::register_console(&SERIAL_CONSOLE);
    registry::register_input(&PS2_KEYBOARD);
    registry::register_display(&FB_DISPLAY);
    registry::register_block(&NVME_BLOCK);
    crate::kprintln!("[boot] HAL ready - console/input/display/block registered");
}"""

# ---------------------------------------------------------------------------
# 4. main.rs — help string + blockdevtest dispatch arm
# ---------------------------------------------------------------------------

HELP_OLD = 'preempttest drawtest ownertest haltest pcitest mmiotest nvmeinittest identifytest nvmeiotest\n");'
HELP_NEW = 'preempttest drawtest ownertest haltest pcitest mmiotest nvmeinittest identifytest nvmeiotest blockdevtest\n");'

DISPATCH_OLD = """            Err(e) => crate::kprintln!("[nvmeiotest] FAIL - identify_namespace returned {:?}", e),
        }
    } else if !b.is_empty() {"""

DISPATCH_NEW = """            Err(e) => crate::kprintln!("[nvmeiotest] FAIL - identify_namespace returned {:?}", e),
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
        print(f"[patch_38] {label} match count in {path}: {count}, ABORT: expected exactly 1 match.")
        sys.exit(1)
    write(path, src.replace(old, new, 1))
    print(f"[patch_38] patched {path} ({label})")


def main():
    try:
        with open(HAL_BLOCK_PATH, "x", encoding="utf-8") as f:
            f.write(HAL_BLOCK_CONTENT)
        print(f"[patch_38] created {HAL_BLOCK_PATH}")
    except FileExistsError:
        print(f"[patch_38] ABORT: {HAL_BLOCK_PATH} already exists — refusing to overwrite.")
        sys.exit(1)

    patch_inplace(HAL_REGISTRY_PATH, REGISTRY_IMPORTS_OLD, REGISTRY_IMPORTS_NEW, "import BlockDevice/BlockError")
    patch_inplace(HAL_REGISTRY_PATH, REGISTRY_SLOTS_OLD, REGISTRY_SLOTS_NEW, "BLOCK slot")
    patch_inplace(HAL_REGISTRY_PATH, REGISTRY_REGFN_OLD, REGISTRY_REGFN_NEW, "register_block()")
    patch_inplace(HAL_REGISTRY_PATH, REGISTRY_WRAPFN_OLD, REGISTRY_WRAPFN_NEW, "block_size/count/read/write wrappers")
    patch_inplace(HAL_MOD_PATH, HAL_MOD_OLD, HAL_MOD_NEW, "register NvmeBlockDevice in hal::init()")
    patch_inplace(MAIN_RS_PATH, HELP_OLD, HELP_NEW, "help string")
    patch_inplace(MAIN_RS_PATH, DISPATCH_OLD, DISPATCH_NEW, "blockdevtest dispatch arm")

    print("[patch_38] OK — BlockDevice/BlockError HAL abstraction added, blockdevtest wired up.")
    print("[patch_38] Next: cargo build, then build-iso.sh + QEMU (with -device nvme attached) and run 'blockdevtest'.")


if __name__ == "__main__":
    main()
