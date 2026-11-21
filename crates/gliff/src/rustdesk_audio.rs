//! Play Opus packets from the current RustDesk view through PipeWire.

use std::ffi::c_void;
use std::io;
use std::process::Stdio;
use std::ptr::NonNull;

use tokio::io::AsyncWriteExt as _;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

#[link(name = "opus")]
unsafe extern "C" {
    fn opus_decoder_create(rate: i32, channels: i32, error: *mut i32) -> *mut c_void;
    fn opus_decode(
        decoder: *mut c_void,
        data: *const u8,
        len: i32,
        pcm: *mut i16,
        frame_size: i32,
        decode_fec: i32,
    ) -> i32;
    fn opus_decoder_destroy(decoder: *mut c_void);
}

struct Decoder {
    state: NonNull<c_void>,
    channels: usize,
    samples: Vec<i16>,
}

impl Decoder {
    fn new(rate: u32, channels: u32) -> io::Result<Self> {
        if !matches!(rate, 8000 | 12000 | 16000 | 24000 | 48000) || !matches!(channels, 1 | 2) {
            return Err(io::Error::other("RustDesk sent an unsupported Opus format"));
        }
        let mut error = 0;
        // SAFETY: Rate and channels are valid for libopus, and error is writable.
        let state = unsafe { opus_decoder_create(rate as i32, channels as i32, &mut error) };
        let state = NonNull::new(state)
            .ok_or_else(|| io::Error::other(format!("libopus could not start ({error})")))?;
        Ok(Self {
            state,
            channels: channels as usize,
            // Opus can decode up to 120 ms from a single packet.
            samples: vec![0; rate as usize * 120 / 1000 * channels as usize],
        })
    }

    fn decode(&mut self, packet: &[u8]) -> Option<&[i16]> {
        let len = i32::try_from(packet.len()).ok()?;
        if len == 0 {
            return None;
        }
        // SAFETY: The decoder is live and uniquely owned. Both buffers are valid
        // for the advertised length and output capacity.
        let count = unsafe {
            opus_decode(
                self.state.as_ptr(),
                packet.as_ptr(),
                len,
                self.samples.as_mut_ptr(),
                (self.samples.len() / self.channels) as i32,
                0,
            )
        };
        self.samples
            .get(..usize::try_from(count).ok()? * self.channels)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: This instance is the only owner of the decoder.
        unsafe { opus_decoder_destroy(self.state.as_ptr()) }
    }
}

pub struct Playback {
    pub format: (u32, u32),
    packets: mpsc::Sender<Vec<u8>>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Playback {
    pub fn start(
        host: &str,
        sample_rate: u32,
        channels: u32,
        failed: impl FnOnce(String) + 'static,
    ) -> io::Result<Self> {
        let mut decoder = Decoder::new(sample_rate, channels)?;
        let mut player = tokio::process::Command::new("pw-play")
            .args(crate::sound::pcm_player_args(host, sample_rate, channels))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut pcm = player.stdin.take().expect("piped stdin");
        // A slow player must never stall the RustDesk video or input.
        let (packets, mut incoming) = mpsc::channel::<Vec<u8>>(8);
        let (stop, stopped) = oneshot::channel();
        let task = tokio::task::spawn_local(async move {
            let write = async {
                let mut bytes = Vec::new();
                while let Some(packet) = incoming.recv().await {
                    if let Some(samples) = decoder.decode(&packet) {
                        bytes.clear();
                        bytes.extend(samples.iter().flat_map(|sample| sample.to_ne_bytes()));
                        pcm.write_all(&bytes).await?;
                    }
                }
                Ok(())
            };
            let result = tokio::select! {
                biased;
                _ = stopped => Ok(()),
                status = player.wait() => status.and_then(|status| {
                    Err(io::Error::other(format!("pw-play stopped ({status})")))
                }),
                result = write => result,
            };
            let _ = player.kill().await;
            if let Err(error) = result {
                failed(error.to_string());
            }
        });
        Ok(Self {
            format: (sample_rate, channels),
            packets,
            stop: Some(stop),
            task: Some(task),
        })
    }

    pub fn packet(&self, packet: Vec<u8>) {
        let _ = self.packets.try_send(packet);
    }

    pub async fn finish(mut self) {
        self.stop.take();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_one_opus_packet_to_interleaved_pcm() {
        let mut decoder = Decoder::new(48000, 2).unwrap();
        // A 20 ms Opus DTX packet at 48 kHz.
        let pcm = decoder.decode(&[0xf8, 0xff, 0xfe]).unwrap();
        assert_eq!(pcm.len(), 960 * 2);
        assert!(decoder.decode(&[]).is_none());
    }

    #[test]
    fn refuses_formats_the_peer_could_not_encode() {
        assert!(Decoder::new(44000, 2).is_err());
        assert!(Decoder::new(48000, 8).is_err());
    }
}
