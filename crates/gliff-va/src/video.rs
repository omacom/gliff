//! One encoder and one decoder type over both codecs, so the pipeline
//! drives either the same way.

use std::sync::Arc;

use crate::display::{Caps, Display, VaCodec};
use crate::encoder::{EncodedPacket, H264Encoder, PendingEncode};
use crate::hevc::decoder::HevcDecoder;
use crate::hevc::encoder::HevcEncoder;
use crate::settings::EncoderSettings;
use crate::surface::Surface;
use crate::{H264Decoder, Result};

/// Both codecs are boxed: their sizes differ a lot, and one is made per
/// stream, so the allocation costs nothing.
pub enum VideoEncoder {
    H264(Box<H264Encoder>),
    Hevc(Box<HevcEncoder>),
}

impl VideoEncoder {
    /// An encoder for `caps.codec`.
    pub fn new(display: &Arc<Display>, caps: &Caps, settings: EncoderSettings) -> Result<Self> {
        Ok(match caps.codec {
            VaCodec::H264 => Self::H264(Box::new(H264Encoder::new(display, caps, settings)?)),
            VaCodec::Hevc => Self::Hevc(Box::new(HevcEncoder::new(display, caps, settings)?)),
        })
    }

    /// The largest coded size `caps.codec` encodes.
    pub fn max_coded_extent(caps: &Caps) -> (u32, u32) {
        match caps.codec {
            VaCodec::H264 => H264Encoder::max_coded_extent(caps),
            VaCodec::Hevc => HevcEncoder::max_coded_extent(caps),
        }
    }

    pub fn codec(&self) -> VaCodec {
        match self {
            Self::H264(_) => VaCodec::H264,
            Self::Hevc(_) => VaCodec::Hevc,
        }
    }

    pub fn settings(&self) -> &EncoderSettings {
        match self {
            Self::H264(e) => e.settings(),
            Self::Hevc(e) => e.settings(),
        }
    }

    pub fn input(&self) -> &Surface {
        match self {
            Self::H264(e) => e.input(),
            Self::Hevc(e) => e.input(),
        }
    }

    pub fn set_rate(&mut self, bitrate: u32, framerate: u32, vbv_ms: u32) {
        match self {
            Self::H264(e) => e.set_rate(bitrate, framerate, vbv_ms),
            Self::Hevc(e) => e.set_rate(bitrate, framerate, vbv_ms),
        }
    }

    pub fn submit(&mut self, force_keyframe: bool) -> Result<PendingEncode> {
        match self {
            Self::H264(e) => e.submit(force_keyframe),
            Self::Hevc(e) => e.submit(force_keyframe),
        }
    }

    pub fn finish(&mut self, pending: PendingEncode) -> Result<EncodedPacket> {
        match self {
            Self::H264(e) => e.finish(pending),
            Self::Hevc(e) => e.finish(pending),
        }
    }
}

pub enum VideoDecoder {
    H264(Box<H264Decoder>),
    Hevc(Box<HevcDecoder>),
}

impl VideoDecoder {
    /// A decoder for `caps.codec`.
    pub fn new(display: &Arc<Display>, caps: &Caps) -> Result<Self> {
        Ok(match caps.codec {
            VaCodec::H264 => Self::H264(Box::new(H264Decoder::new(display, caps)?)),
            VaCodec::Hevc => Self::Hevc(Box::new(HevcDecoder::new(display, caps)?)),
        })
    }

    pub fn display_size(&self) -> Option<(u32, u32)> {
        match self {
            Self::H264(d) => d.display_size(),
            Self::Hevc(d) => d.display_size(),
        }
    }

    pub fn surfaces(&self) -> &[Surface] {
        match self {
            Self::H264(d) => d.surfaces(),
            Self::Hevc(d) => d.surfaces(),
        }
    }

    pub fn generation(&self) -> u64 {
        match self {
            Self::H264(d) => d.generation(),
            Self::Hevc(d) => d.generation(),
        }
    }

    pub fn last_output(&self) -> Option<usize> {
        match self {
            Self::H264(d) => d.last_output(),
            Self::Hevc(d) => d.last_output(),
        }
    }

    pub fn decode(&mut self, access_unit: &[u8]) -> Result<Option<usize>> {
        match self {
            Self::H264(d) => d.decode(access_unit),
            Self::Hevc(d) => d.decode(access_unit),
        }
    }
}
