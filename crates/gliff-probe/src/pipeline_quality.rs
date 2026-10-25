use anyhow::{anyhow, Context, Result};
use gliff_proto::color::psnr;

use super::rgb_channels;

pub(super) fn single_stream_psnr(
    pixels: &[u8],
    settings: gliff_sw::EncoderSettings,
    reference: &[u8],
) -> Result<f64> {
    let mut decoder = gliff_sw::Decoder::new(false, settings.width, settings.height)
        .context("OpenH264 reference decoder")?;
    let mut encoder =
        gliff_sw::Encoder::new(settings, false).context("OpenH264 reference encoder")?;
    let packet = encoder
        .encode_bgra(pixels, true)
        .context("OpenH264 reference encode")?;
    let decoded = match decoder
        .decode(&packet.main, &[])
        .context("OpenH264 reference decode")?
    {
        Some(frame) => frame,
        None => decoder
            .flush()?
            .ok_or_else(|| anyhow!("OpenH264 reference produced no frame"))?,
    };
    Ok(psnr(
        &rgb_channels(reference),
        &rgb_channels(&decoded.pixels),
    ))
}

// Flat 1080p frames measured 3.3 dB below the single-stream reference.
pub(super) const CPU_REFERENCE_MARGIN_DB: f64 = 4.0;

pub(super) fn cpu_passes(pipeline_psnr: f64, reference_psnr: f64) -> bool {
    pipeline_psnr >= 20.0 && pipeline_psnr >= reference_psnr - CPU_REFERENCE_MARGIN_DB
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_loss_passes_but_garbage_and_pipeline_loss_fail() {
        assert!(cpu_passes(29.9, 29.0));
        assert!(cpu_passes(59.6, 62.9));
        assert!(!cpu_passes(19.0, 19.0));
        assert!(!cpu_passes(29.0, 35.0));
    }
}
