//! The other machine's screen as gliff keeps it, and the encodings that
//! draw on it: raw, copy-rectangle and ZRLE.
//!
//! Pixels are asked for as 32-bit little-endian true colour with red, green
//! and blue at bits 16, 8 and 0, so they arrive as BGRX bytes; ZRLE packs
//! each into its three low bytes, B, G and R.

use flate2::{Decompress, FlushDecompress};

/// The largest screen taken, a side.
pub(super) const MAX_SIDE: u32 = 16384;

/// The screen, in BGRX bytes; with no pixels kept when only its size
/// matters (driving it from the edge).
pub(super) struct Screen {
    pub width: u32,
    pub height: u32,
    keep: bool,
    pub bgrx: Vec<u8>,
}

impl Screen {
    pub fn new(width: u32, height: u32, keep: bool) -> Self {
        let mut s = Self {
            width: 0,
            height: 0,
            keep,
            bgrx: Vec::new(),
        };
        s.resize(width, height);
        s
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        if self.keep {
            self.bgrx = vec![0; width as usize * height as usize * 4];
        }
    }

    /// Whether a `w`×`h` rectangle at `x`,`y` lies on the screen.
    pub fn holds(&self, x: u32, y: u32, w: u32, h: u32) -> bool {
        x + w <= self.width && y + h <= self.height
    }

    /// Draw `w`×`h` BGRX pixels at `x`,`y`; the rectangle must be on the
    /// screen.
    pub fn put(&mut self, x: u32, y: u32, w: u32, h: u32, src: &[u8]) {
        if !self.keep || w == 0 {
            return;
        }
        let row = w as usize * 4;
        for (r, line) in src.chunks_exact(row).take(h as usize).enumerate() {
            let at = ((y as usize + r) * self.width as usize + x as usize) * 4;
            self.bgrx[at..at + row].copy_from_slice(line);
        }
    }

    /// Copy the `w`×`h` rectangle at `sx`,`sy` to `x`,`y`; both must be on
    /// the screen.
    pub fn copy(&mut self, sx: u32, sy: u32, x: u32, y: u32, w: u32, h: u32) {
        if !self.keep || w == 0 {
            return;
        }
        let row = w as usize * 4;
        let stride = self.width as usize * 4;
        let mut moved = Vec::with_capacity(row * h as usize);
        for r in 0..h as usize {
            let at = (sy as usize + r) * stride + sx as usize * 4;
            moved.extend_from_slice(&self.bgrx[at..at + row]);
        }
        self.put(x, y, w, h, &moved);
    }
}

/// Inflate `input`, the next part of a ZRLE stream, which runs on across
/// rectangles.
pub(super) fn inflate(z: &mut Decompress, input: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(input.len().saturating_mul(4).clamp(4096, limit.max(4096)));
    let start = z.total_in();
    loop {
        let used = (z.total_in() - start) as usize;
        if out.len() == out.capacity() {
            out.reserve(out.capacity().max(4096));
        }
        let before = (z.total_in(), z.total_out());
        z.decompress_vec(&input[used..], &mut out, FlushDecompress::Sync)
            .map_err(|e| format!("ZRLE: {e}"))?;
        let used = (z.total_in() - start) as usize;
        if used == input.len() && out.len() < out.capacity() {
            return Ok(out);
        }
        if (z.total_in(), z.total_out()) == before {
            if used == input.len() {
                return Ok(out);
            }
            return Err("ZRLE: the stream stopped".into());
        }
        if out.len() > limit {
            return Err("ZRLE: a rectangle inflates too far".into());
        }
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn u8(&mut self) -> Result<u8, String> {
        let b = *self.data.get(self.pos).ok_or("ZRLE: a tile runs short")?;
        self.pos += 1;
        Ok(b)
    }

    /// A compressed pixel: B, G, R.
    fn cpixel(&mut self) -> Result<[u8; 4], String> {
        let p = self
            .data
            .get(self.pos..self.pos + 3)
            .ok_or("ZRLE: a tile runs short")?;
        self.pos += 3;
        Ok([p[0], p[1], p[2], 0])
    }

    /// A run's length: bytes summed while they are 255, plus one.
    fn run(&mut self) -> Result<usize, String> {
        let mut len = 1;
        loop {
            let b = self.u8()?;
            len += usize::from(b);
            if b != 255 {
                return Ok(len);
            }
        }
    }
}

/// Draw a ZRLE rectangle from its inflated `data`: 64×64 tiles, left to
/// right and top to bottom.
pub(super) fn zrle(
    screen: &mut Screen,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    data: &[u8],
) -> Result<(), String> {
    let mut r = Reader { data, pos: 0 };
    let mut tile = Vec::with_capacity(64 * 64 * 4);
    for ty in (0..h).step_by(64) {
        for tx in (0..w).step_by(64) {
            let tw = 64.min(w - tx);
            let th = 64.min(h - ty);
            let n = (tw * th) as usize;
            tile.clear();
            match r.u8()? {
                0 => {
                    for _ in 0..n {
                        tile.extend_from_slice(&r.cpixel()?);
                    }
                }
                1 => {
                    let p = r.cpixel()?;
                    for _ in 0..n {
                        tile.extend_from_slice(&p);
                    }
                }
                sub @ 2..=16 => {
                    let palette = (0..sub)
                        .map(|_| r.cpixel())
                        .collect::<Result<Vec<_>, _>>()?;
                    let bits = match sub {
                        2 => 1,
                        3 | 4 => 2,
                        _ => 4,
                    };
                    for _ in 0..th {
                        let (mut byte, mut left) = (0u8, 0);
                        for _ in 0..tw {
                            if left == 0 {
                                byte = r.u8()?;
                                left = 8;
                            }
                            left -= bits;
                            let i = usize::from((byte >> left) & ((1 << bits) - 1));
                            let p = palette.get(i).ok_or("ZRLE: an index past the palette")?;
                            tile.extend_from_slice(p);
                        }
                    }
                }
                128 => {
                    while tile.len() < n * 4 {
                        let p = r.cpixel()?;
                        let run = r.run()?;
                        if tile.len() / 4 + run > n {
                            return Err("ZRLE: a run past the tile".into());
                        }
                        for _ in 0..run {
                            tile.extend_from_slice(&p);
                        }
                    }
                }
                sub @ 130..=255 => {
                    let palette = (0..sub - 128)
                        .map(|_| r.cpixel())
                        .collect::<Result<Vec<_>, _>>()?;
                    while tile.len() < n * 4 {
                        let b = r.u8()?;
                        let p = *palette
                            .get(usize::from(b & 127))
                            .ok_or("ZRLE: an index past the palette")?;
                        let run = if b & 128 != 0 { r.run()? } else { 1 };
                        if tile.len() / 4 + run > n {
                            return Err("ZRLE: a run past the tile".into());
                        }
                        for _ in 0..run {
                            tile.extend_from_slice(&p);
                        }
                    }
                }
                other => return Err(format!("ZRLE: tile kind {other} is not one there is")),
            }
            screen.put(x + tx, y + ty, tw, th, &tile);
        }
    }
    Ok(())
}
