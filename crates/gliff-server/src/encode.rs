//! The session's video encoder and the thread it runs on. An encode can
//! take tens of milliseconds, so it runs there rather than on the session
//! thread, where input, acks and pings would wait behind it. The session
//! asks for the next capture while it runs.

use std::os::fd::AsFd;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gliff_vk::{DmabufPlane, EncodedFrame, Encoder, EncoderSettings};
use hypr_capture::CapturedFrame;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::rate::cpu_rebuild_due;
use crate::session::VideoTier;

/// An encoder the thread owns. It is built on the thread from a `Spec` and
/// never leaves it, so it need not be `Send` (the Vulkan one is not).
pub(crate) trait FrameEncoder: Sized + 'static {
    type Spec: Send + 'static;
    type Frame: Send + 'static;

    fn build(spec: Self::Spec) -> Result<Self>;
    fn encode(
        &mut self,
        frame: &Self::Frame,
        stream: (u32, u32),
        key: bool,
    ) -> Result<EncodedFrame>;
    fn set_rate(&mut self, bitrate: u32, fps: u32, vbv_ms: u32);
}

/// A finished encode. The frame comes back so the session can keep it as
/// the still frame.
pub(crate) struct Encoded<F> {
    pub frame: F,
    pub result: Result<EncodedFrame>,
    /// Time spent encoding, without the wait in the queue.
    pub took: Duration,
}

enum Cmd<E: FrameEncoder> {
    Encode {
        frame: E::Frame,
        stream: (u32, u32),
        key: bool,
    },
    SetRate {
        bitrate: u32,
        fps: u32,
        vbv_ms: u32,
    },
    Rebuild(E::Spec, mpsc::Sender<Result<()>>),
    /// Answered once every earlier command is done.
    Flush(mpsc::Sender<()>),
}

/// The session's handle on the thread. Dropping it ends the thread.
pub(crate) struct EncodeThread<E: FrameEncoder> {
    cmds: Option<mpsc::Sender<Cmd<E>>>,
    done: UnboundedReceiver<Encoded<E::Frame>>,
    thread: Option<JoinHandle<()>>,
}

const ENDED: &str = "the encoder thread ended";

impl<E: FrameEncoder> EncodeThread<E> {
    /// Start the thread and build its first encoder; fails if the build does.
    pub fn spawn(spec: E::Spec) -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (done_tx, done_rx) = unbounded_channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("gliff-encode".into())
            .spawn(move || match E::build(spec) {
                Ok(encoder) => {
                    let _ = ready_tx.send(Ok(()));
                    serve(encoder, cmd_rx, done_tx);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .context("spawn the encoder thread")?;
        let encoder = Self {
            cmds: Some(cmd_tx),
            done: done_rx,
            thread: Some(thread),
        };
        ready_rx.recv().context(ENDED)??;
        Ok(encoder)
    }

    /// Encode `frame` at `stream` size; the result arrives on `finished`.
    pub fn encode(&self, frame: E::Frame, stream: (u32, u32), key: bool) {
        self.send(Cmd::Encode { frame, stream, key });
    }

    /// Change the rate from the next encode on.
    pub fn set_rate(&self, bitrate: u32, fps: u32, vbv_ms: u32) {
        self.send(Cmd::SetRate {
            bitrate,
            fps,
            vbv_ms,
        });
    }

    /// Replace the encoder, blocking until the new one is built. On failure
    /// the thread keeps the old one.
    pub fn rebuild(&self, spec: E::Spec) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.send(Cmd::Rebuild(spec, tx));
        rx.recv().context(ENDED)?
    }

    pub async fn finished(&mut self) -> Result<Encoded<E::Frame>> {
        self.done.recv().await.context(ENDED)
    }

    /// Block until every encode asked for so far is done, and return the
    /// oldest result not yet taken.
    pub fn wait(&mut self) -> Result<Option<Encoded<E::Frame>>> {
        let (tx, rx) = mpsc::channel();
        self.send(Cmd::Flush(tx));
        rx.recv().context(ENDED)?;
        // The thread sent the result before it answered the flush, and
        // try_recv never reports a spurious Empty.
        Ok(self.done.try_recv().ok())
    }

    /// End the thread after the encode in progress, and drop every result
    /// not yet taken, with its frame.
    pub fn stop(&mut self) {
        drop(self.cmds.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        while self.done.try_recv().is_ok() {}
    }

    /// A send fails only when the thread has ended, which `finished` and
    /// the round trips report.
    fn send(&self, cmd: Cmd<E>) {
        if let Some(cmds) = &self.cmds {
            let _ = cmds.send(cmd);
        }
    }
}

impl<E: FrameEncoder> Drop for EncodeThread<E> {
    fn drop(&mut self) {
        self.stop();
    }
}

fn serve<E: FrameEncoder>(
    mut encoder: E,
    cmds: mpsc::Receiver<Cmd<E>>,
    done: UnboundedSender<Encoded<E::Frame>>,
) {
    while let Ok(cmd) = cmds.recv() {
        match cmd {
            Cmd::Encode { frame, stream, key } => {
                let t0 = Instant::now();
                let result = encoder.encode(&frame, stream, key);
                let took = t0.elapsed();
                if done
                    .send(Encoded {
                        frame,
                        result,
                        took,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Cmd::SetRate {
                bitrate,
                fps,
                vbv_ms,
            } => encoder.set_rate(bitrate, fps, vbv_ms),
            Cmd::Rebuild(spec, reply) => {
                let _ = reply.send(E::build(spec).map(|new| encoder = new));
            }
            Cmd::Flush(reply) => {
                let _ = reply.send(());
            }
        }
    }
}

pub(crate) enum VideoEncoder {
    Gpu(Box<Encoder>),
    Cpu(Box<gliff_sw::Encoder>),
}

/// What the encoder thread builds a `VideoEncoder` from.
pub(crate) struct EncoderSpec {
    pub video: VideoTier,
    pub settings: EncoderSettings,
    pub dual: bool,
}

impl FrameEncoder for VideoEncoder {
    type Spec = EncoderSpec;
    type Frame = CapturedFrame;

    fn build(spec: EncoderSpec) -> Result<Self> {
        let EncoderSpec {
            video,
            settings,
            dual,
        } = spec;
        match video {
            VideoTier::Gpu(gpu) => Ok(Self::Gpu(Box::new(Encoder::new(&gpu, settings, dual)?))),
            VideoTier::Cpu => Ok(Self::Cpu(Box::new(gliff_sw::Encoder::new(
                gliff_sw::EncoderSettings {
                    width: settings.width,
                    height: settings.height,
                    bitrate: settings.bitrate,
                    framerate: settings.framerate,
                },
                dual,
            )?))),
        }
    }

    /// Change the CBR target, the frame rate it is spread over, and (GPU
    /// only) the rate-control buffer window, from the next frame on. The
    /// CPU encoder has no VBV knob; OpenH264 manages its own buffer. It
    /// also cannot change its rate live — every change re-creates the
    /// encoders and costs a keyframe — so `cpu_rebuild_due` paces the
    /// rebuilds: a congestion cut at once, other moves only when large and
    /// at most every two seconds.
    fn set_rate(&mut self, bitrate: u32, framerate: u32, vbv_ms: u32) {
        match self {
            Self::Gpu(enc) => enc.set_rate(bitrate, framerate, vbv_ms),
            Self::Cpu(enc) => {
                let s = enc.settings();
                if cpu_rebuild_due(
                    (s.bitrate, s.framerate),
                    (bitrate, framerate),
                    enc.rebuilt_at().elapsed(),
                ) {
                    enc.set_rate(bitrate, framerate);
                }
            }
        }
    }

    /// Encode a captured frame at `stream` size: the GPU imports the dmabuf
    /// and scales in its split shader; the CPU maps the buffer and scales
    /// the pixels before converting.
    fn encode(
        &mut self,
        frame: &CapturedFrame,
        stream: (u32, u32),
        force_keyframe: bool,
    ) -> Result<EncodedFrame> {
        match self {
            Self::Gpu(enc) => {
                anyhow::ensure!(
                    !frame.buffer.is_shm(),
                    "the GPU tier needs dmabuf capture, but capture fell back to wl_shm (no linear \
                     dmabuf, no dmabuf global or no render node); run with --video cpu"
                );
                let info = &frame.buffer.info;
                let fourcc = drm_fourcc::DrmFourcc::try_from(info.fourcc).map_err(|_| {
                    anyhow::anyhow!("capture fourcc {:#x} is not a DRM format", info.fourcc)
                })?;
                let plane = DmabufPlane {
                    fd: info.fd.as_fd(),
                    width: info.width,
                    height: info.height,
                    offset: info.planes[0].offset,
                    stride: info.planes[0].stride,
                    fourcc,
                    modifier: info.modifier,
                };
                // The import is cached per ring buffer; the generation changes
                // when the ring is reallocated (resize), so old imports are
                // never reused.
                let buffer_key = (frame.buffer.generation << 32) | frame.buffer.index as u64;
                Ok(enc.encode_dmabuf_transformed(
                    buffer_key,
                    &plane,
                    frame.transform,
                    force_keyframe,
                )?)
            }
            Self::Cpu(enc) => {
                let t0 = Instant::now();
                let image = frame.read_bgra()?;
                tracing::debug!(
                    read_us = t0.elapsed().as_micros(),
                    "read the captured frame"
                );
                let (w, h) = (stream.0 as usize, stream.1 as usize);
                let pixels = if (image.width, image.height) == (w, h) {
                    image.pixels
                } else {
                    gliff_proto::color::downscale_bgra(
                        &image.pixels,
                        image.width,
                        image.height,
                        w,
                        h,
                    )
                };
                let f = enc.encode_bgra(&pixels, force_keyframe)?;
                Ok(EncodedFrame {
                    main: f.main,
                    aux: f.aux,
                    keyframe: f.keyframe,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Codes a frame as `[frame, bitrate]`, so a test sees the rate it got.
    struct Fake {
        bitrate: u32,
    }

    impl FrameEncoder for Fake {
        /// The starting bitrate, or `None` to fail the build.
        type Spec = Option<u32>;
        type Frame = u8;

        fn build(spec: Option<u32>) -> Result<Self> {
            Ok(Self {
                bitrate: spec.context("rejected")?,
            })
        }

        fn encode(&mut self, frame: &u8, _: (u32, u32), key: bool) -> Result<EncodedFrame> {
            // Slow enough that the commands sent after it queue up.
            std::thread::sleep(Duration::from_millis(10));
            Ok(EncodedFrame {
                main: vec![*frame, self.bitrate as u8],
                aux: None,
                keyframe: key,
            })
        }

        fn set_rate(&mut self, bitrate: u32, _: u32, _: u32) {
            self.bitrate = bitrate;
        }
    }

    fn next_coded(thread: &mut EncodeThread<Fake>) -> Vec<u8> {
        thread.wait().unwrap().unwrap().result.unwrap().main
    }

    #[test]
    fn commands_apply_in_order() {
        let mut thread = EncodeThread::<Fake>::spawn(Some(1)).unwrap();
        thread.encode(10, (2, 2), true);
        thread.set_rate(2, 30, 0);
        thread.encode(11, (2, 2), false);
        assert_eq!(next_coded(&mut thread), [10, 1]);
        assert_eq!(next_coded(&mut thread), [11, 2]);
        assert!(thread.wait().unwrap().is_none());
    }

    #[test]
    fn a_failed_build_keeps_the_old_encoder() {
        assert!(EncodeThread::<Fake>::spawn(None).is_err());
        let mut thread = EncodeThread::<Fake>::spawn(Some(1)).unwrap();
        assert!(thread.rebuild(None).is_err());
        thread.encode(12, (2, 2), false);
        assert_eq!(next_coded(&mut thread), [12, 1]);
        thread.rebuild(Some(3)).unwrap();
        thread.encode(13, (2, 2), false);
        assert_eq!(next_coded(&mut thread), [13, 3]);
    }
}
