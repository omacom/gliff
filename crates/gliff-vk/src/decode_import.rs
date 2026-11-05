use std::sync::Arc;

use gliff_va::{PrimeDescriptor, Surface, UsageHint};

use crate::image::Image;
use crate::{Error, Gpu, Result};

/// Import a decoder-owned NV12 surface and return its DRM modifier.
///
/// # Errors
/// Returns an error if VA-API cannot export the surface or Vulkan cannot
/// import it through either of the decoder's import paths.
pub fn decode_import_supported(gpu: &Arc<Gpu>) -> Result<u64> {
    let surface = Surface::new_nv12(&gpu.va, 640, 368, UsageHint::Decoder)?;
    let desc = surface.export()?;
    Image::import_decode_surface(gpu, &desc).map_err(|e| {
        Error::Unsupported(format!(
            "decoder NV12 surface with modifier {:#x}: {e}",
            desc.modifier
        ))
    })?;
    Ok(desc.modifier)
}

impl Image {
    pub(crate) fn import_decode_surface(gpu: &Arc<Gpu>, desc: &PrimeDescriptor) -> Result<Self> {
        match Self::import_nv12_planes(gpu, desc) {
            Ok(img) => Ok(img),
            Err(e) => {
                if desc.objects.len() == 2 && desc.planes.len() == 2 {
                    tracing::warn!(error = %e, "separate-plane import failed; trying the combined NV12 import");
                }
                Self::import_nv12(gpu, desc, false)
            }
        }
    }
}
