//! HEVC (H.265) Main profile: NAL unit framing, the header parser the
//! decoder needs, and the encoder and decoder themselves.
//!
//! The encoder leaves the parameter sets and slice headers to the driver,
//! so the decoder parses whatever the driver wrote rather than a fixed
//! layout.

pub mod decoder;
pub mod encoder;
pub mod nal;
pub mod parser;
