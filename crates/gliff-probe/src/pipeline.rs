use std::os::fd::AsFd;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use gliff_proto::color::{bgra_to_yuv444, downscale_bgra, psnr, yuv444_to_bgra};
use gliff_sw::VideoMode;
use gliff_vk::{Decoder, DmabufPlane, EncodedFrame, Encoder, EncoderSettings, Gpu};
use hypr_capture::{CaptureConfig, CaptureEvent, CaptureMode, Capturer};
use hypr_wl::Target;

use super::pipeline_quality;
use super::{pick_output, rgb_channels, status, sw_settings, ProbeDecoder, ProbeEncoder};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Fallback {
    Allowed,
    Disabled,
}

impl Fallback {
    pub(super) fn resolve(cli: Option<&str>, env: Option<&str>) -> Self {
        if cli.or(env) == Some("gpu") {
            Self::Disabled
        } else {
            Self::Allowed
        }
    }
}

pub(super) fn run(
    target: &Target,
    node: &std::path::Path,
    mode: CaptureMode,
    output: Option<String>,
    video: VideoMode,
    fallback: Fallback,
) -> Result<()> {
    match run_tier(target, node, mode, output.clone(), video) {
        Err(e) if video == VideoMode::Gpu && fallback == Fallback::Allowed => {
            status(
                false,
                &format!("GPU pipeline failed ({e:#}); the CPU pipeline will be used"),
            );
            run_tier(target, node, mode, output, VideoMode::Cpu)
        }
        result => result,
    }
}

/// Capture one frame and push it through the exact server and client
/// pipelines: dmabuf import, GPU split, two encodes, two decodes, GPU
/// recombine. Compares the result with the CPU 4:4:4 reference.
fn run_tier(
    target: &Target,
    node: &std::path::Path,
    mode: CaptureMode,
    output: Option<String>,
    video: VideoMode,
) -> Result<()> {
    if mode == CaptureMode::Shm && video == VideoMode::Gpu {
        bail!("--capture shm frames can only go through the CPU pipeline; add --video cpu");
    }
    let output = pick_output(target, output)?;
    let mut cfg = CaptureConfig::new(output.clone());
    cfg.target = target.clone();
    cfg.render_node = node.to_path_buf();
    cfg.mode = mode;
    // Capture the way the tier under test captures, as the server does: the
    // GPU tier imports the dmabuf as it is and must not get a wl_shm buffer,
    // while the CPU tier maps the frame and keeps the fallback.
    cfg.shm_fallback = video != VideoMode::Gpu;
    cfg.cursor = false;
    let (tx, rx) = mpsc::channel();
    let capturer = Capturer::start(
        cfg,
        Box::new(move |ev| {
            let _ = tx.send(ev);
        }),
    )?;
    capturer.request_frame()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut captured = None;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(CaptureEvent::Frame(frame)) => {
                captured = Some(frame);
                break;
            }
            Ok(CaptureEvent::Error(e)) => bail!("capture error: {e}"),
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
    let frame = captured.ok_or_else(|| {
        anyhow!(
            "no frame captured (a headless output renders reliably; a physical KVM output may not)"
        )
    })?;
    if video == VideoMode::Gpu && frame.buffer.is_shm() {
        bail!(
            "the GPU tier needs dmabuf capture, but capture fell back to wl_shm; \
             this is what the server refuses too (run with --video cpu)"
        );
    }
    let info = &frame.buffer.info;
    // Even dimensions, as `read_bgra` crops to for the 4:2:0 path.
    let (w, h) = frame.extent();
    let fourcc = drm_fourcc::DrmFourcc::try_from(info.fourcc)
        .map_err(|_| anyhow!("captured fourcc {:#x} is not a DRM format", info.fourcc))?;
    println!(
        "  captured {w}x{h} {fourcc:?} modifier {:#x} {} from {output}",
        info.modifier,
        if frame.buffer.is_shm() {
            "(wl_shm)"
        } else {
            "(dmabuf)"
        }
    );
    // The CPU reference for the PSNR, when the frame maps at all: a tiled
    // capture buffer (NVIDIA, GPU tier) is not CPU-readable, which is why the
    // CPU tier captures into wl_shm instead.
    let reference = match frame.read_bgra() {
        Ok(r) => Some(r),
        Err(e) if video == VideoMode::Gpu => {
            println!("  no CPU reference for this frame: {e}; the PSNR is skipped");
            None
        }
        Err(e) => return Err(e).context("read the captured frame"),
    };
    let plane = DmabufPlane {
        fd: info.fd.as_fd(),
        width: info.width,
        height: info.height,
        offset: info.planes[0].offset,
        stride: info.planes[0].stride,
        fourcc,
        modifier: info.modifier,
    };

    let gpu = match video {
        VideoMode::Gpu => Some(Gpu::open(Some(node))?),
        VideoMode::Cpu => None,
    };
    let encoder_max = match &gpu {
        Some(gpu) => Encoder::max_size(gpu).context("encoder limits")?,
        None => gliff_sw::Encoder::MAX_SIZE,
    };
    let (sw, sh) = EncoderSettings::fit_extent(w, h, encoder_max);
    let scaled = (sw, sh) != (w, h);
    if scaled {
        println!(
            "  scaling {w}x{h} to {sw}x{sh} to fit the encoder maximum {}x{}",
            encoder_max.0, encoder_max.1
        );
    }
    let settings = EncoderSettings {
        width: sw,
        height: sh,
        bitrate: EncoderSettings::default_bitrate(sw, sh, 60),
        framerate: 60,
        vbv_ms: EncoderSettings::DEFAULT_VBV_MS,
    };
    let bitrate = settings.bitrate;
    let (mut encoder, mut decoder) = match &gpu {
        Some(gpu) => (
            ProbeEncoder::Gpu(Box::new(
                Encoder::new(gpu, settings, true).context("encoder")?,
            )),
            ProbeDecoder::Gpu(Box::new(
                Decoder::new(gpu, true, sw, sh).context("decoder")?,
            )),
        ),
        None => (
            ProbeEncoder::Cpu(Box::new(
                gliff_sw::Encoder::new(sw_settings(sw, sh, settings.bitrate), true)
                    .context("encoder")?,
            )),
            ProbeDecoder::Cpu(Box::new(
                gliff_sw::Decoder::new(true, sw, sh).context("decoder")?,
            )),
        ),
    };
    let t0 = std::time::Instant::now();
    let packet = match &mut encoder {
        ProbeEncoder::Gpu(enc) => enc
            .encode_dmabuf_transformed(1, &plane, frame.transform, true)
            .context("encode")?,
        ProbeEncoder::Cpu(enc) => {
            let reference = reference
                .as_ref()
                .ok_or_else(|| anyhow!("the CPU tier needs a mappable capture buffer"))?;
            let pixels = if scaled {
                downscale_bgra(
                    &reference.pixels,
                    reference.width,
                    reference.height,
                    sw as usize,
                    sh as usize,
                )
            } else {
                reference.pixels.clone()
            };
            let f = enc.encode_bgra(&pixels, true).context("encode")?;
            EncodedFrame {
                main: f.main,
                aux: f.aux,
                keyframe: f.keyframe,
            }
        }
    };
    let enc_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let aux = packet.aux.as_deref().unwrap_or(&[]);
    println!(
        "  encoded main {} bytes, aux {} bytes, key={}, {enc_ms:.1} ms",
        packet.main.len(),
        aux.len(),
        packet.keyframe
    );
    let t1 = std::time::Instant::now();
    let out = match decoder
        .decode_to_bgra(&packet.main, aux)
        .context("decode")?
    {
        Some(out) => out,
        // The CPU decoder holds its only picture until the next unit or a
        // drain; this is the last unit, so drain.
        None => decoder
            .flush()
            .context("flush")?
            .ok_or_else(|| anyhow!("decode produced no frame"))?,
    };
    let dec_ms = t1.elapsed().as_secs_f64() * 1000.0;
    drop(frame);
    drop(capturer);

    let Some(reference) = reference else {
        println!("  decoded {dec_ms:.1} ms");
        status(
            true,
            "Dual420 4:4:4 GPU pipeline on a captured frame (ran; no PSNR without a \
             CPU-mappable capture buffer)",
        );
        return Ok(());
    };

    // What the CPU reference path makes of the same pixels, so only coding
    // loss and shader rounding count. For scaled GPU streams the shader's
    // downscale filter differs, so their PSNR remains informational.
    let pixels = if scaled {
        downscale_bgra(
            &reference.pixels,
            reference.width,
            reference.height,
            sw as usize,
            sh as usize,
        )
    } else {
        reference.pixels.clone()
    };
    let cpu = yuv444_to_bgra(&bgra_to_yuv444(
        &pixels,
        sw as usize * 4,
        sw as usize,
        sh as usize,
    ));
    let rgb_psnr = psnr(&rgb_channels(&cpu), &rgb_channels(&out));
    println!("  decoded {dec_ms:.1} ms; end-to-end RGB PSNR vs CPU reference {rgb_psnr:.1} dB");
    let (tier, ok) = if gpu.is_some() {
        ("GPU", scaled || rgb_psnr > 35.0)
    } else {
        let reference_psnr =
            pipeline_quality::single_stream_psnr(&pixels, sw_settings(sw, sh, bitrate), &cpu)?;
        println!(
            "  pipeline {rgb_psnr:.1} dB, OpenH264 single-stream reference {reference_psnr:.1} dB (margin {:.1} dB)",
            pipeline_quality::CPU_REFERENCE_MARGIN_DB
        );
        (
            "CPU",
            pipeline_quality::cpu_passes(rgb_psnr, reference_psnr),
        )
    };
    status(
        ok,
        &format!("Dual420 4:4:4 {tier} pipeline on a captured frame"),
    );
    if !ok {
        bail!("pipeline PSNR too low");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_gpu_disables_fallback_with_cli_precedence() {
        for (cli, env, expected) in [
            (None, None, Fallback::Allowed),
            (Some("gpu"), None, Fallback::Disabled),
            (None, Some("gpu"), Fallback::Disabled),
            (Some("cpu"), Some("gpu"), Fallback::Allowed),
            (Some("gpu"), Some("cpu"), Fallback::Disabled),
            (None, Some("invalid"), Fallback::Allowed),
        ] {
            assert_eq!(Fallback::resolve(cli, env), expected, "{cli:?}, {env:?}");
        }
    }
}
