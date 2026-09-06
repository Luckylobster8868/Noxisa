//! Block-device HAL abstraction (Phase B.1, patch_38).
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
