use anyhow::Result;
use gliff_vk::Gpu;

use super::status;

pub(super) fn info(node: &std::path::Path) -> Result<()> {
    let gpu = Gpu::open(Some(node))?;
    println!("  vulkan: {} ({})", gpu.name, gpu.driver);
    println!(
        "  va-api: {} (libva {}.{})",
        gpu.va.vendor, gpu.va.version.0, gpu.va.version.1
    );
    print_caps(&gpu.va_caps);
    print_encode(&gpu);
    match gpu.va_caps.can_decode() {
        Ok(()) => status(true, "VA-API H.264 decode"),
        Err(e) => {
            status(false, &format!("VA-API H.264 decode: {e}"));
            return Err(e.into());
        }
    }
    match gliff_vk::decode_import_supported(&gpu) {
        Ok(modifier) => status(
            true,
            &format!("Vulkan imports the decoder's NV12 surface (modifier {modifier:#x})"),
        ),
        Err(e) => {
            status(false, &format!(
                "decoder NV12 import: {e}; the GPU decode tier is unavailable; the CPU decoder will be used"
            ));
            return Err(e.into());
        }
    }
    Ok(())
}

fn print_caps(caps: &gliff_va::Caps) {
    if let Some(e) = caps.encode_entrypoint {
        println!(
            "  H.264 encode: entrypoint {}, rate control {}, packed headers {:#x}, maximum {}x{}",
            gliff_va::display::entrypoint_name(e),
            gliff_va::display::rate_control_names(caps.rate_control).join("|"),
            caps.packed_headers,
            caps.max_width,
            caps.max_height
        );
    }
}

/// Which API encodes H.264 here, if either. The VA-API line keeps its own
/// PASS (`e2e.sh` keys the VA-API-only checks on it), but a VA-API driver
/// without an encoder is not a failure when Vulkan Video encodes instead:
/// that is the normal NVIDIA case, and a bare FAIL there reads as if the
/// GPU tier were unavailable.
fn print_encode(gpu: &Gpu) {
    let vk = gpu.vk_encode_info();
    let va = gpu.va_caps.can_encode();
    match (&va, &vk) {
        (Ok(()), _) => status(true, "VA-API H.264 encode"),
        (Err(e), Some(_)) => println!("  va-api H.264 encode: {e}; Vulkan Video encodes instead"),
        (Err(e), None) => status(false, &format!("VA-API H.264 encode: {e}")),
    }
    match (&vk, va.is_ok()) {
        (Some(info), false) => status(
            true,
            &format!(
                "Vulkan Video H.264 encode: maximum {}x{}, {} encode queue(s), {} {}",
                info.max_extent.0,
                info.max_extent.1,
                info.queues,
                info.std_header,
                info.std_header_version
            ),
        ),
        // The VA-API encoder is the one the pipeline picks; say the fallback
        // is there without a second PASS line for an encoder nothing uses.
        (Some(info), true) => println!(
            "  vulkan video: H.264 encode also available (maximum {}x{}), unused while VA-API encodes",
            info.max_extent.0, info.max_extent.1
        ),
        (None, false) => status(
            false,
            "Vulkan Video H.264 encode: no encoder (no VK_KHR_video_encode_h264 H.264 profile, \
             or GLIFF_VK_ENCODE=0); the CPU tier will be used",
        ),
        (None, true) => {}
    }
}
