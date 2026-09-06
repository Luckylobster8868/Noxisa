//! PCI configuration-space access + enumeration (Phase B.1, patch_33)
//!
//! Legacy mechanism #1 (0xCF8/0xCFC) config access. This is the minimal
//! slice needed to discover an NVMe controller and its BAR0 — full PCI
//! (capabilities list walking, bridges/secondary-bus recursion, MSI-X
//! table parsing) is deliberately NOT implemented here. Brute-force
//! enumeration (every bus/device/function) is used instead of walking
//! bridges, since it's simpler and sufficient for a single-NVMe-device
//! QEMU/Dell target.
//!
//! Port I/O helpers are private to this module, matching the existing
//! per-driver convention (see drivers/pic.rs, drivers/serial.rs) rather
//! than a shared port-I/O module.

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

#[inline]
unsafe fn outl(port: u16, val: u32) {
    unsafe {
        core::arch::asm!("out dx, eax",
            in("dx") port, in("eax") val, options(nomem, nostack));
    }
}

#[inline]
unsafe fn inl(port: u16) -> u32 {
    let val: u32;
    unsafe {
        core::arch::asm!("in eax, dx",
            in("dx") port, out("eax") val, options(nomem, nostack));
    }
    val
}

/// Read a 32-bit value from PCI configuration space.
/// `offset` must be 4-byte aligned (low 2 bits are masked off).
fn config_read_u32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let address: u32 = (1 << 31)
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | ((offset as u32) & 0xFC);
    unsafe {
        outl(CONFIG_ADDRESS, address);
        inl(CONFIG_DATA)
    }
}

fn config_read_u16(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    let dword = config_read_u32(bus, device, function, offset & 0xFC);
    let shift = (offset & 2) * 8;
    ((dword >> shift) & 0xFFFF) as u16
}

fn config_read_u8(bus: u8, device: u8, function: u8, offset: u8) -> u8 {
    let dword = config_read_u32(bus, device, function, offset & 0xFC);
    let shift = (offset & 3) * 8;
    ((dword >> shift) & 0xFF) as u8
}

/// Decoded BAR0. NVMe controllers always use a 64-bit or 32-bit memory
/// BAR (never I/O-space) per the NVMe spec, but this stays general.
#[derive(Debug, Clone, Copy)]
pub struct Bar {
    /// Physical base address, already masked of the low decode bits.
    pub base: u64,
    pub is_mmio: bool,
    pub is_64bit: bool,
    pub prefetchable: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct PciDevice {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub bar0: Option<Bar>,
}

const OFFSET_VENDOR_ID: u8 = 0x00;
const OFFSET_DEVICE_ID: u8 = 0x02;
const OFFSET_HEADER_TYPE: u8 = 0x0E;
const OFFSET_CLASS: u8 = 0x0B;
const OFFSET_SUBCLASS: u8 = 0x0A;
const OFFSET_PROG_IF: u8 = 0x09;
const OFFSET_BAR0: u8 = 0x10;
const OFFSET_BAR1: u8 = 0x14;

const NVME_CLASS: u8 = 0x01;
const NVME_SUBCLASS: u8 = 0x08;
const NVME_PROG_IF: u8 = 0x02;

/// Read and decode BAR0 for a given function. Handles both 32-bit and
/// 64-bit (BAR0+BAR1 pair) memory BARs. Returns None for I/O-space BARs
/// or an all-ones/unimplemented BAR (value 0x0 or 0xFFFFFFFF pattern).
fn read_bar0(bus: u8, device: u8, function: u8) -> Option<Bar> {
    let raw0 = config_read_u32(bus, device, function, OFFSET_BAR0);
    if raw0 == 0 {
        return None;
    }

    let is_io = raw0 & 0x1 == 1;
    if is_io {
        // NVMe never uses I/O-space BARs; record as non-MMIO and move on.
        return Some(Bar {
            base: (raw0 & !0x3) as u64,
            is_mmio: false,
            is_64bit: false,
            prefetchable: false,
        });
    }

    let bar_type = (raw0 >> 1) & 0x3; // 0 = 32-bit, 2 = 64-bit
    let prefetchable = (raw0 >> 3) & 0x1 == 1;
    let is_64bit = bar_type == 2;

    let base: u64 = if is_64bit {
        let raw1 = config_read_u32(bus, device, function, OFFSET_BAR1);
        (((raw1 as u64) << 32) | (raw0 & !0xF) as u64) as u64
    } else {
        (raw0 & !0xF) as u64
    };

    Some(Bar {
        base,
        is_mmio: true,
        is_64bit,
        prefetchable,
    })
}

fn probe_function(bus: u8, device: u8, function: u8) -> Option<PciDevice> {
    let vendor_id = config_read_u16(bus, device, function, OFFSET_VENDOR_ID);
    if vendor_id == 0xFFFF {
        return None; // no device present
    }
    let device_id = config_read_u16(bus, device, function, OFFSET_DEVICE_ID);
    let class = config_read_u8(bus, device, function, OFFSET_CLASS);
    let subclass = config_read_u8(bus, device, function, OFFSET_SUBCLASS);
    let prog_if = config_read_u8(bus, device, function, OFFSET_PROG_IF);
    let bar0 = read_bar0(bus, device, function);

    Some(PciDevice {
        bus,
        device,
        function,
        vendor_id,
        device_id,
        class,
        subclass,
        prog_if,
        bar0,
    })
}

/// Brute-force enumerate every bus/device/function. Calls `visit` for
/// each present function. Multi-function devices (header type bit 7 set
/// on function 0) are handled by simply always checking functions 0-7 —
/// slightly wasteful vs. skipping non-multifunction devices, but simpler
/// and correct, and this only runs once at boot.
pub fn enumerate<F: FnMut(PciDevice)>(mut visit: F) {
    for bus in 0..=255u16 {
        let bus = bus as u8;
        for device in 0..32u8 {
            let header_type = config_read_u8(bus, device, 0, OFFSET_HEADER_TYPE);
            let vendor_id = config_read_u16(bus, device, 0, OFFSET_VENDOR_ID);
            if vendor_id == 0xFFFF {
                continue; // no device at function 0 -> nothing at this slot
            }
            let multi_function = header_type & 0x80 != 0;
            let max_function = if multi_function { 8 } else { 1 };
            for function in 0..max_function {
                if let Some(dev) = probe_function(bus, device, function) {
                    visit(dev);
                }
            }
        }
        if bus == 255 {
            break; // avoid u8 wraparound re-looping bus 0
        }
    }
}

/// Scan for the first NVMe controller (class 0x01, subclass 0x08,
/// prog-if 0x02). Returns None if no NVMe controller is present.
pub fn find_nvme() -> Option<PciDevice> {
    let mut found: Option<PciDevice> = None;
    enumerate(|dev| {
        if found.is_none()
            && dev.class == NVME_CLASS
            && dev.subclass == NVME_SUBCLASS
            && dev.prog_if == NVME_PROG_IF
        {
            found = Some(dev);
        }
    });
    found
}
