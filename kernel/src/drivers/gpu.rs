//! GPU / Framebuffer driver
//!
//! Uses the linear framebuffer provided by the bootloader (GOP/UEFI).
//! The compositor later takes over and uses Vulkan/DRM for accelerated rendering.

use spin::Mutex;

struct Fb {
    addr:   u64,
    width:  u32,
    height: u32,
    pitch:  u32,   // bytes per row
}

static FB: Mutex<Option<Fb>> = Mutex::new(None);

/// Initialise with the framebuffer information from the bootloader.
pub fn init(phys_addr: u64, width: u32, height: u32) {
    // Map the framebuffer into the kernel's virtual address space.
    let virt_addr = crate::memory::paging::phys_to_virt(phys_addr);
    let pitch     = width * 4; // assume 32 bpp (ARGB8888)

    *FB.lock() = Some(Fb { addr: virt_addr, width, height, pitch });

    // Clear screen to dark background
    fill_rect(0, 0, width, height, 0x0F_11_17); // #0f1117
    crate::kprintln!("[gpu] Framebuffer {}×{} at {:#x}", width, height, virt_addr);
}

/// Fill a rectangle with a 32-bit ARGB colour.
/// Uses raw volatile writes — no SSE/memset.
pub fn fill_rect(x: u32, y: u32, w: u32, h: u32, colour: u32) {
    let fb_guard = FB.lock();
    let Some(fb) = fb_guard.as_ref() else { return };

    let pitch_px = fb.pitch / 4;
    let x_end = (x + w).min(fb.width);
    let y_end = (y + h).min(fb.height);

    for row in y..y_end {
        let row_base = fb.addr + (row as u64 * fb.pitch as u64);
        let mut ptr = (row_base + x as u64 * 4) as *mut u32;
        for _ in x..x_end {
            unsafe {
                ptr.write_volatile(colour);
                ptr = ptr.add(1);
            }
        }
    }
}

/// Draw a single pixel.
#[inline]
pub fn set_pixel(x: u32, y: u32, colour: u32) {
    fill_rect(x, y, 1, 1, colour);
}

/// Return (width, height) of the current framebuffer, or (0,0) if not initialised.
pub fn dimensions() -> (u32, u32) {
    let g = FB.lock();
    g.as_ref().map_or((0, 0), |fb| (fb.width, fb.height))
}
