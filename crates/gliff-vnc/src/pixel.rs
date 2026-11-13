//! Pixel formats a VNC client can ask for, and the conversion from the
//! captured BGRA frames into them.

/// RFB `PIXEL_FORMAT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_colour: bool,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl PixelFormat {
    /// What the server offers: 32-bit true colour, 8 bits per channel,
    /// little-endian, which is BGRX in memory.
    pub const BGRX: Self = Self {
        bits_per_pixel: 32,
        depth: 24,
        big_endian: false,
        true_colour: true,
        red_max: 255,
        green_max: 255,
        blue_max: 255,
        red_shift: 16,
        green_shift: 8,
        blue_shift: 0,
    };

    /// The colour map the server sets for a client that asks for one: 3
    /// bits of red, 3 of green and 2 of blue in each 8-bit index.
    pub const PALETTE_332: Self = Self {
        bits_per_pixel: 8,
        depth: 8,
        big_endian: false,
        true_colour: true,
        red_max: 7,
        green_max: 7,
        blue_max: 3,
        red_shift: 5,
        green_shift: 2,
        blue_shift: 0,
    };

    pub fn bytes_per_pixel(&self) -> usize {
        usize::from(self.bits_per_pixel / 8)
    }

    pub fn to_bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0] = self.bits_per_pixel;
        b[1] = self.depth;
        b[2] = u8::from(self.big_endian);
        b[3] = u8::from(self.true_colour);
        b[4..6].copy_from_slice(&self.red_max.to_be_bytes());
        b[6..8].copy_from_slice(&self.green_max.to_be_bytes());
        b[8..10].copy_from_slice(&self.blue_max.to_be_bytes());
        b[10] = self.red_shift;
        b[11] = self.green_shift;
        b[12] = self.blue_shift;
        b
    }

    pub fn from_bytes(b: &[u8; 16]) -> Self {
        Self {
            bits_per_pixel: b[0],
            depth: b[1],
            big_endian: b[2] != 0,
            true_colour: b[3] != 0,
            red_max: u16::from_be_bytes([b[4], b[5]]),
            green_max: u16::from_be_bytes([b[6], b[7]]),
            blue_max: u16::from_be_bytes([b[8], b[9]]),
            red_shift: b[10],
            green_shift: b[11],
            blue_shift: b[12],
        }
    }

    /// Why a client's format cannot be served, if it cannot.
    pub fn check(&self) -> Result<(), String> {
        if !matches!(self.bits_per_pixel, 8 | 16 | 32) {
            return Err(format!("{} bits per pixel", self.bits_per_pixel));
        }
        if !self.true_colour {
            // A colour map client gets PALETTE_332's indices.
            return if self.bits_per_pixel == 8 {
                Ok(())
            } else {
                Err(format!(
                    "a colour map with {} bits per pixel",
                    self.bits_per_pixel
                ))
            };
        }
        let bits = u32::from(self.bits_per_pixel);
        for (max, shift) in [
            (self.red_max, self.red_shift),
            (self.green_max, self.green_shift),
            (self.blue_max, self.blue_shift),
        ] {
            let width = u16::BITS - max.leading_zeros();
            if max == 0 || u32::from(shift) + width > bits {
                return Err(format!("a channel of max {max} at shift {shift}"));
            }
        }
        Ok(())
    }
}

/// Converts BGRA pixels into a client's format through per-channel tables.
#[derive(Debug, Clone)]
pub struct Converter {
    format: PixelFormat,
    red: [u32; 256],
    green: [u32; 256],
    blue: [u32; 256],
}

impl Converter {
    /// `format` must have passed [`PixelFormat::check`]. A colour map
    /// client is converted as [`PixelFormat::PALETTE_332`].
    pub fn new(format: PixelFormat) -> Self {
        let channels = if format.true_colour {
            format
        } else {
            PixelFormat::PALETTE_332
        };
        let table = |max: u16, shift: u8| {
            let mut t = [0u32; 256];
            for (v, slot) in t.iter_mut().enumerate() {
                let scaled = (v as u32 * u32::from(max) + 127) / 255;
                *slot = scaled << shift;
            }
            t
        };
        Self {
            format,
            red: table(channels.red_max, channels.red_shift),
            green: table(channels.green_max, channels.green_shift),
            blue: table(channels.blue_max, channels.blue_shift),
        }
    }

    pub fn format(&self) -> &PixelFormat {
        &self.format
    }

    /// The client's value for one BGRA pixel.
    #[inline]
    pub fn pixel(&self, bgra: &[u8]) -> u32 {
        self.blue[usize::from(bgra[0])]
            | self.green[usize::from(bgra[1])]
            | self.red[usize::from(bgra[2])]
    }

    /// Append `value` in the client's byte order and size.
    #[inline]
    pub fn put(&self, value: u32, out: &mut Vec<u8>) {
        match (self.format.bits_per_pixel, self.format.big_endian) {
            (8, _) => out.push(value as u8),
            (16, false) => out.extend_from_slice(&(value as u16).to_le_bytes()),
            (16, true) => out.extend_from_slice(&(value as u16).to_be_bytes()),
            (_, false) => out.extend_from_slice(&value.to_le_bytes()),
            (_, true) => out.extend_from_slice(&value.to_be_bytes()),
        }
    }

    /// The colour map a colour map client needs, as RFB
    /// `SetColourMapEntries` colours (16 bits per channel), index order.
    pub fn colour_map(&self) -> Option<Vec<[u16; 3]>> {
        if self.format.true_colour {
            return None;
        }
        let p = PixelFormat::PALETTE_332;
        let expand = |v: u32, max: u16| (v * 65535 / u32::from(max)) as u16;
        Some(
            (0u32..256)
                .map(|i| {
                    [
                        expand((i >> p.red_shift) & u32::from(p.red_max), p.red_max),
                        expand((i >> p.green_shift) & u32::from(p.green_max), p.green_max),
                        expand((i >> p.blue_shift) & u32::from(p.blue_max), p.blue_max),
                    ]
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_format_round_trips() {
        let f = PixelFormat::BGRX;
        assert_eq!(PixelFormat::from_bytes(&f.to_bytes()), f);
        assert!(f.check().is_ok());
    }

    #[test]
    fn bgrx_little_endian_is_the_captured_bytes() {
        let c = Converter::new(PixelFormat::BGRX);
        let mut out = Vec::new();
        c.put(c.pixel(&[0x11, 0x22, 0x33, 0xff]), &mut out);
        assert_eq!(out, [0x11, 0x22, 0x33, 0x00]);
    }

    #[test]
    fn rgb565_big_endian() {
        let f = PixelFormat {
            bits_per_pixel: 16,
            depth: 16,
            big_endian: true,
            true_colour: true,
            red_max: 31,
            green_max: 63,
            blue_max: 31,
            red_shift: 11,
            green_shift: 5,
            blue_shift: 0,
        };
        assert!(f.check().is_ok());
        let c = Converter::new(f);
        let mut out = Vec::new();
        // Pure red, then white.
        c.put(c.pixel(&[0, 0, 255, 255]), &mut out);
        c.put(c.pixel(&[255, 255, 255, 255]), &mut out);
        assert_eq!(out, [0xf8, 0x00, 0xff, 0xff]);
    }

    #[test]
    fn colour_map_clients_get_332_indices_and_their_map() {
        let f = PixelFormat {
            true_colour: false,
            ..PixelFormat::PALETTE_332
        };
        assert!(f.check().is_ok());
        let c = Converter::new(f);
        let white = c.pixel(&[255, 255, 255, 255]);
        assert_eq!(white, 0xff);
        let map = c.colour_map().unwrap();
        assert_eq!(map.len(), 256);
        assert_eq!(map[0xff], [65535, 65535, 65535]);
        assert_eq!(map[0], [0, 0, 0]);
    }

    #[test]
    fn impossible_formats_are_refused() {
        let mut f = PixelFormat::BGRX;
        f.bits_per_pixel = 24;
        assert!(f.check().is_err());
        let mut f = PixelFormat::BGRX;
        f.red_shift = 30;
        assert!(f.check().is_err());
        let mut f = PixelFormat::BGRX;
        f.true_colour = false;
        assert!(f.check().is_err());
    }
}
