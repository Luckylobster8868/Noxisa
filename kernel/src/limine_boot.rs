//! Limine native protocol requests.
//!
//! Under Multiboot2, the framebuffer address we get can stop being backed
//! by real VRAM on real hardware after ExitBootServices — this only ever
//! showed up on the real Dell, not in QEMU, which is more forgiving.
//! Limine's native protocol guarantees the framebuffer response it gives
//! us stays valid, which is why we're switching to it.

use limine::BaseRevision;
use limine::request::{FramebufferRequest, MemmapRequest, HhdmRequest};
use limine::{RequestsStartMarker, RequestsEndMarker};

#[used]
#[unsafe(link_section = ".requests_start_marker")]
static _START_MARKER: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static BASE_REVISION: BaseRevision = BaseRevision::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static MEMMAP_REQUEST: MemmapRequest = MemmapRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

#[used]
#[unsafe(link_section = ".requests_end_marker")]
static _END_MARKER: RequestsEndMarker = RequestsEndMarker::new();

/// Call once at the very start of kernel_main to confirm Limine actually
/// honored our requested protocol revision before we trust any response.
pub fn assert_base_revision_supported() {
    if !BASE_REVISION.is_supported() {
        crate::kprintln!("[WARN] Limine base revision not confirmed supported — continuing anyway");
    }
}
