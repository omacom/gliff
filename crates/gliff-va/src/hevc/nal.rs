//! HEVC NAL units in an Annex B stream. The start codes are the same as
//! H.264's; the header is two bytes, with the type in bits 1-6 of the first.

use crate::h264::annexb;

pub const TRAIL_N: u8 = 0;
pub const TRAIL_R: u8 = 1;
pub const IDR_W_RADL: u8 = 19;
pub const IDR_N_LP: u8 = 20;
pub const CRA_NUT: u8 = 21;
pub const VPS: u8 = 32;
pub const SPS: u8 = 33;
pub const PPS: u8 = 34;

/// One NAL unit: its type, temporal id, and its bytes starting at the
/// two-byte header (no start code).
#[derive(Debug, Clone, Copy)]
pub struct Nal<'a> {
    pub nal_type: u8,
    pub temporal_id: u8,
    pub data: &'a [u8],
}

impl Nal<'_> {
    /// A coded slice segment of a picture (types 0-31).
    pub fn is_slice(&self) -> bool {
        self.nal_type < 32
    }

    pub fn is_idr(&self) -> bool {
        matches!(self.nal_type, IDR_W_RADL | IDR_N_LP)
    }

    /// An intra random access point: BLA, IDR or CRA (types 16-23).
    pub fn is_irap(&self) -> bool {
        (16..=23).contains(&self.nal_type)
    }
}

/// Split an Annex B stream into HEVC NAL units.
pub fn nal_units(data: &[u8]) -> Vec<Nal<'_>> {
    annexb::nal_units(data)
        .into_iter()
        .filter(|n| n.data.len() >= 2)
        .map(|n| Nal {
            nal_type: (n.data[0] >> 1) & 0x3f,
            temporal_id: (n.data[1] & 7).saturating_sub(1),
            data: n.data,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_and_types_units() {
        let s = [
            0, 0, 0, 1, 0x40, 0x01, 9, // VPS
            0, 0, 1, 0x42, 0x01, 9, // SPS
            0, 0, 1, 0x44, 0x01, 9, // PPS
            0, 0, 0, 1, 0x26, 0x01, 9, 9, // IDR_W_RADL
            0, 0, 1, 0x02, 0x01, 9, // TRAIL_R
        ];
        let units = nal_units(&s);
        let types: Vec<u8> = units.iter().map(|n| n.nal_type).collect();
        assert_eq!(types, vec![VPS, SPS, PPS, IDR_W_RADL, TRAIL_R]);
        assert!(units[3].is_idr() && units[3].is_irap() && units[3].is_slice());
        assert!(!units[4].is_idr() && units[4].is_slice());
        assert!(units.iter().all(|n| n.temporal_id == 0));
    }
}
