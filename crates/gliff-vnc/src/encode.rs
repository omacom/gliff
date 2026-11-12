//! Rectangle encodings: Raw, Hextile and ZRLE, from packed BGRA frames.

use std::collections::HashMap;

use flate2::{Compress, Compression, FlushCompress};

use crate::pixel::Converter;

/// RFB encoding numbers this server knows.
pub mod number {
    pub const RAW: i32 = 0;
    pub const COPY_RECT: i32 = 1;
    pub const HEXTILE: i32 = 5;
    pub const ZRLE: i32 = 16;
    /// Pseudo-encoding: the client takes a framebuffer size change.
    pub const DESKTOP_SIZE: i32 = -223;
}

/// A packed BGRA image, `width * 4` bytes per row.
#[derive(Debug, Clone, Copy)]
pub struct Image<'a> {
    pub width: usize,
    pub height: usize,
    pub pixels: &'a [u8],
}

impl Image<'_> {
    #[inline]
    fn at(&self, x: usize, y: usize) -> &[u8] {
        let i = (y * self.width + x) * 4;
        &self.pixels[i..i + 4]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// Which encoding a client gets, from its SetEncodings list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Raw,
    Hextile,
    Zrle,
}

impl Method {
    /// The first encoding in the client's preference order that the server
    /// knows; Raw, which every client must take, otherwise.
    pub fn choose(preferred: &[i32]) -> Self {
        preferred
            .iter()
            .find_map(|&e| match e {
                number::ZRLE => Some(Self::Zrle),
                number::HEXTILE => Some(Self::Hextile),
                number::RAW => Some(Self::Raw),
                _ => None,
            })
            .unwrap_or(Self::Raw)
    }

    pub fn number(self) -> i32 {
        match self {
            Self::Raw => number::RAW,
            Self::Hextile => number::HEXTILE,
            Self::Zrle => number::ZRLE,
        }
    }
}

/// One connection's encoder: the client's pixel format and, for ZRLE, the
/// zlib stream that lasts as long as the connection.
pub struct Encoder {
    pub converter: Converter,
    pub method: Method,
    zlib: Compress,
}

impl Encoder {
    pub fn new(converter: Converter, method: Method) -> Self {
        Self {
            converter,
            method,
            zlib: Compress::new(Compression::new(6), true),
        }
    }

    /// Append `rect` of `image`, header included.
    pub fn rect(&mut self, image: &Image, rect: Rect, out: &mut Vec<u8>) {
        out.extend_from_slice(&rect.x.to_be_bytes());
        out.extend_from_slice(&rect.y.to_be_bytes());
        out.extend_from_slice(&rect.width.to_be_bytes());
        out.extend_from_slice(&rect.height.to_be_bytes());
        out.extend_from_slice(&self.method.number().to_be_bytes());
        match self.method {
            Method::Raw => raw(&self.converter, image, rect, out),
            Method::Hextile => hextile(&self.converter, image, rect, out),
            Method::Zrle => {
                let mut tiles = Vec::new();
                zrle_tiles(&self.converter, image, rect, &mut tiles);
                let start = out.len();
                out.extend_from_slice(&[0; 4]);
                deflate(&mut self.zlib, &tiles, out);
                let len = (out.len() - start - 4) as u32;
                out[start..start + 4].copy_from_slice(&len.to_be_bytes());
            }
        }
    }
}

fn raw(c: &Converter, image: &Image, r: Rect, out: &mut Vec<u8>) {
    for y in r.y as usize..(r.y + r.height) as usize {
        for x in r.x as usize..(r.x + r.width) as usize {
            c.put(c.pixel(image.at(x, y)), out);
        }
    }
}

/// The client values of a `w`x`h` block at `x`,`y`, row by row.
fn block(c: &Converter, image: &Image, x: usize, y: usize, w: usize, h: usize) -> Vec<u32> {
    let mut v = Vec::with_capacity(w * h);
    for row in y..y + h {
        for col in x..x + w {
            v.push(c.pixel(image.at(col, row)));
        }
    }
    v
}

mod hex {
    pub const RAW: u8 = 1;
    pub const BACKGROUND: u8 = 2;
    pub const FOREGROUND: u8 = 4;
    pub const ANY_SUBRECTS: u8 = 8;
    pub const SUBRECTS_COLOURED: u8 = 16;
}

/// Hextile: 16x16 tiles, each a background with solid subrectangles, or
/// raw when that is no larger.
fn hextile(c: &Converter, image: &Image, r: Rect, out: &mut Vec<u8>) {
    let bpp = c.format().bytes_per_pixel();
    // Colours carried from one tile to the next within this rectangle.
    let mut last_bg: Option<u32> = None;
    let mut last_fg: Option<u32> = None;
    let (x0, y0) = (r.x as usize, r.y as usize);
    let (x1, y1) = (x0 + r.width as usize, y0 + r.height as usize);
    let mut ty = y0;
    while ty < y1 {
        let th = (y1 - ty).min(16);
        let mut tx = x0;
        while tx < x1 {
            let tw = (x1 - tx).min(16);
            let px = block(c, image, tx, ty, tw, th);
            let bg = most_common(&px);
            let subrects = subrects(&px, tw, th, bg);
            let colours = {
                let mut seen: Vec<u32> = Vec::with_capacity(3);
                for &(colour, ..) in &subrects {
                    if !seen.contains(&colour) {
                        seen.push(colour);
                        if seen.len() > 1 {
                            break;
                        }
                    }
                }
                seen
            };
            let mono = colours.len() <= 1;
            let per_subrect = if mono { 2 } else { 2 + bpp };
            let encoded = 1 + bpp * 2 + subrects.len() * per_subrect + 1;
            if subrects.len() > 255 || encoded >= tw * th * bpp {
                out.push(hex::RAW);
                for &v in &px {
                    c.put(v, out);
                }
                last_bg = None;
                last_fg = None;
            } else {
                let mut flags = 0u8;
                let mut body = Vec::new();
                if last_bg != Some(bg) {
                    flags |= hex::BACKGROUND;
                    c.put(bg, &mut body);
                    last_bg = Some(bg);
                }
                if !subrects.is_empty() {
                    flags |= hex::ANY_SUBRECTS;
                    if mono {
                        let fg = colours[0];
                        if last_fg != Some(fg) {
                            flags |= hex::FOREGROUND;
                            c.put(fg, &mut body);
                            last_fg = Some(fg);
                        }
                    } else {
                        flags |= hex::SUBRECTS_COLOURED;
                        last_fg = None;
                    }
                    body.push(subrects.len() as u8);
                    for &(colour, sx, sy, sw, sh) in &subrects {
                        if !mono {
                            c.put(colour, &mut body);
                        }
                        body.push(((sx as u8) << 4) | sy as u8);
                        body.push((((sw - 1) as u8) << 4) | (sh - 1) as u8);
                    }
                }
                out.push(flags);
                out.extend_from_slice(&body);
            }
            tx += tw;
        }
        ty += th;
    }
}

fn most_common(px: &[u32]) -> u32 {
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for &p in px {
        *counts.entry(p).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|&(colour, n)| (n, std::cmp::Reverse(colour)))
        .map(|(colour, _)| colour)
        .unwrap_or(0)
}

/// Cover every pixel that is not `bg` with solid rectangles, greedily: the
/// widest run from the first uncovered pixel, grown down while it holds.
/// Each is (colour, x, y, width, height).
fn subrects(px: &[u32], w: usize, h: usize, bg: u32) -> Vec<(u32, usize, usize, usize, usize)> {
    let mut covered = vec![false; w * h];
    let mut out = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if covered[i] || px[i] == bg {
                continue;
            }
            let colour = px[i];
            let mut rw = 1;
            while x + rw < w && !covered[i + rw] && px[i + rw] == colour {
                rw += 1;
            }
            let mut rh = 1;
            'grow: while y + rh < h {
                let row = (y + rh) * w + x;
                for k in 0..rw {
                    if covered[row + k] || px[row + k] != colour {
                        break 'grow;
                    }
                }
                rh += 1;
            }
            for dy in 0..rh {
                for dx in 0..rw {
                    covered[(y + dy) * w + x + dx] = true;
                }
            }
            out.push((colour, x, y, rw, rh));
        }
    }
    out
}

/// How ZRLE writes a pixel: three bytes when a 32-bit true colour value
/// fits in three, the client's full size otherwise.
struct CPixel {
    size: usize,
    /// Which bytes of the client's encoding to keep, when `size` is 3.
    skip_first: bool,
}

impl CPixel {
    fn of(c: &Converter) -> Self {
        let f = c.format();
        let bits = |max: u16, shift: u8| -> u32 { u32::from(max) << shift };
        let used = bits(f.red_max, f.red_shift)
            | bits(f.green_max, f.green_shift)
            | bits(f.blue_max, f.blue_shift);
        if f.true_colour && f.bits_per_pixel == 32 && f.depth <= 24 {
            let low = used & 0xff00_0000 == 0;
            let high = used & 0x0000_00ff == 0;
            if low || high {
                // Little-endian puts the low byte first; big-endian last.
                let skip_first = if f.big_endian { low } else { !low };
                return Self {
                    size: 3,
                    skip_first,
                };
            }
        }
        Self {
            size: f.bytes_per_pixel(),
            skip_first: false,
        }
    }

    fn put(&self, c: &Converter, value: u32, out: &mut Vec<u8>) {
        if self.size != 3 {
            return c.put(value, out);
        }
        let mut full = Vec::with_capacity(4);
        c.put(value, &mut full);
        if self.skip_first {
            out.extend_from_slice(&full[1..4]);
        } else {
            out.extend_from_slice(&full[0..3]);
        }
    }
}

/// The uncompressed ZRLE tile stream for `r`: 64x64 tiles, each in the
/// smallest of raw, solid, packed palette, plain RLE or palette RLE.
fn zrle_tiles(c: &Converter, image: &Image, r: Rect, out: &mut Vec<u8>) {
    let cp = CPixel::of(c);
    let (x0, y0) = (r.x as usize, r.y as usize);
    let (x1, y1) = (x0 + r.width as usize, y0 + r.height as usize);
    let mut ty = y0;
    while ty < y1 {
        let th = (y1 - ty).min(64);
        let mut tx = x0;
        while tx < x1 {
            let tw = (x1 - tx).min(64);
            zrle_tile(c, &cp, &block(c, image, tx, ty, tw, th), tw, th, out);
            tx += tw;
        }
        ty += th;
    }
}

fn zrle_tile(c: &Converter, cp: &CPixel, px: &[u32], w: usize, h: usize, out: &mut Vec<u8>) {
    // The palette, in first-seen order, while it stays within 127 colours.
    let mut palette: Vec<u32> = Vec::new();
    let mut index: HashMap<u32, u8> = HashMap::new();
    for &p in px {
        if let std::collections::hash_map::Entry::Vacant(slot) = index.entry(p) {
            if palette.len() == 127 {
                palette.clear();
                break;
            }
            slot.insert(palette.len() as u8);
            palette.push(p);
        }
    }
    let small = !palette.is_empty();
    if small && palette.len() == 1 {
        out.push(1);
        cp.put(c, palette[0], out);
        return;
    }
    let runs = runs(px);
    let run_len = |n: usize| (n - 1) / 255 + 1;
    let raw = px.len() * cp.size;
    let plain = runs
        .iter()
        .map(|&(_, n)| cp.size + run_len(n))
        .sum::<usize>();
    let packed_bits = match palette.len() {
        2 => 1,
        3..=4 => 2,
        5..=16 => 4,
        _ => 0,
    };
    let packed = if small && packed_bits > 0 {
        palette.len() * cp.size + h * (w * packed_bits).div_ceil(8)
    } else {
        usize::MAX
    };
    let palette_rle = if small {
        palette.len() * cp.size
            + runs
                .iter()
                .map(|&(_, n)| if n == 1 { 1 } else { 1 + run_len(n) })
                .sum::<usize>()
    } else {
        usize::MAX
    };
    let best = raw.min(plain).min(packed).min(palette_rle);
    if best == packed {
        out.push(palette.len() as u8);
        for &p in &palette {
            cp.put(c, p, out);
        }
        for row in px.chunks_exact(w) {
            let mut byte = 0u8;
            let mut used = 0;
            for &p in row {
                byte = (byte << packed_bits) | index[&p];
                used += packed_bits;
                if used == 8 {
                    out.push(byte);
                    byte = 0;
                    used = 0;
                }
            }
            if used > 0 {
                out.push(byte << (8 - used));
            }
        }
    } else if best == palette_rle {
        out.push(128 + palette.len() as u8);
        for &p in &palette {
            cp.put(c, p, out);
        }
        for &(p, n) in &runs {
            if n == 1 {
                out.push(index[&p]);
            } else {
                out.push(index[&p] | 0x80);
                put_run(n, out);
            }
        }
    } else if best == plain {
        out.push(128);
        for &(p, n) in &runs {
            cp.put(c, p, out);
            put_run(n, out);
        }
    } else {
        out.push(0);
        for &p in px {
            cp.put(c, p, out);
        }
    }
}

/// Runs of equal values across the tile, row after row.
fn runs(px: &[u32]) -> Vec<(u32, usize)> {
    let mut out: Vec<(u32, usize)> = Vec::new();
    for &p in px {
        match out.last_mut() {
            Some((v, n)) if *v == p => *n += 1,
            _ => out.push((p, 1)),
        }
    }
    out
}

/// A run length as ZRLE writes it: `n - 1` as 255s and a final byte.
fn put_run(n: usize, out: &mut Vec<u8>) {
    let mut rest = n - 1;
    while rest >= 255 {
        out.push(255);
        rest -= 255;
    }
    out.push(rest as u8);
}

/// Compress `data` onto `out` and flush, so the client can decode this
/// rectangle before the next one arrives.
fn deflate(zlib: &mut Compress, data: &[u8], out: &mut Vec<u8>) {
    let mut input = data;
    loop {
        if out.capacity() - out.len() < 64 {
            out.reserve((input.len() / 2).max(4096));
        }
        let before = zlib.total_in();
        zlib.compress_vec(input, out, FlushCompress::Sync)
            .expect("zlib compression of an in-memory buffer");
        input = &input[(zlib.total_in() - before) as usize..];
        // zlib has flushed everything once it stops short of filling the
        // space it was given.
        if input.is_empty() && out.len() < out.capacity() {
            break;
        }
    }
}

#[cfg(test)]
mod tests;
