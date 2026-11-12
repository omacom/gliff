//! Each encoding is decoded back, as a client following RFC 6143 would, and
//! must give the client's value for every pixel of the rectangle.

use super::*;
use crate::pixel::{Converter, PixelFormat};
use flate2::{Decompress, FlushDecompress};
use proptest::prelude::*;

fn rgb565() -> PixelFormat {
    PixelFormat {
        bits_per_pixel: 16,
        depth: 16,
        big_endian: false,
        true_colour: true,
        red_max: 31,
        green_max: 63,
        blue_max: 31,
        red_shift: 11,
        green_shift: 5,
        blue_shift: 0,
    }
}

fn big_endian_rgb888() -> PixelFormat {
    PixelFormat {
        big_endian: true,
        red_shift: 0,
        green_shift: 8,
        blue_shift: 16,
        ..PixelFormat::BGRX
    }
}

fn colour_map() -> PixelFormat {
    PixelFormat {
        true_colour: false,
        ..PixelFormat::PALETTE_332
    }
}

fn formats() -> Vec<PixelFormat> {
    vec![
        PixelFormat::BGRX,
        big_endian_rgb888(),
        rgb565(),
        colour_map(),
    ]
}

/// What the client should end up with for `rect`.
fn expected(c: &Converter, image: &Image, r: Rect) -> Vec<u32> {
    block(
        c,
        image,
        r.x as usize,
        r.y as usize,
        r.width as usize,
        r.height as usize,
    )
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> u8 {
        let v = self.data[self.pos];
        self.pos += 1;
        v
    }
    fn take(&mut self, n: usize) -> &'a [u8] {
        let v = &self.data[self.pos..self.pos + n];
        self.pos += n;
        v
    }
    fn pixel(&mut self, f: &PixelFormat) -> u32 {
        let b = self.take(f.bytes_per_pixel());
        match (b.len(), f.big_endian) {
            (1, _) => u32::from(b[0]),
            (2, false) => u32::from(u16::from_le_bytes([b[0], b[1]])),
            (2, true) => u32::from(u16::from_be_bytes([b[0], b[1]])),
            (_, false) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            (_, true) => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        }
    }
    fn done(&self) -> bool {
        self.pos == self.data.len()
    }
}

fn header(r: &mut Reader, rect: Rect, encoding: i32) {
    let h = r.take(12);
    assert_eq!(u16::from_be_bytes([h[0], h[1]]), rect.x);
    assert_eq!(u16::from_be_bytes([h[2], h[3]]), rect.y);
    assert_eq!(u16::from_be_bytes([h[4], h[5]]), rect.width);
    assert_eq!(u16::from_be_bytes([h[6], h[7]]), rect.height);
    assert_eq!(i32::from_be_bytes([h[8], h[9], h[10], h[11]]), encoding);
}

fn decode_raw(f: &PixelFormat, data: &[u8], rect: Rect) -> Vec<u32> {
    let mut r = Reader { data, pos: 0 };
    header(&mut r, rect, number::RAW);
    let out = (0..rect.width as usize * rect.height as usize)
        .map(|_| r.pixel(f))
        .collect();
    assert!(r.done());
    out
}

fn decode_hextile(f: &PixelFormat, data: &[u8], rect: Rect) -> Vec<u32> {
    let mut r = Reader { data, pos: 0 };
    header(&mut r, rect, number::HEXTILE);
    let (w, h) = (rect.width as usize, rect.height as usize);
    let mut out = vec![u32::MAX; w * h];
    let (mut bg, mut fg) = (None, None);
    for ty in (0..h).step_by(16) {
        for tx in (0..w).step_by(16) {
            let (tw, th) = ((w - tx).min(16), (h - ty).min(16));
            let flags = r.u8();
            if flags & hex::RAW != 0 {
                for y in 0..th {
                    for x in 0..tw {
                        out[(ty + y) * w + tx + x] = r.pixel(f);
                    }
                }
                bg = None;
                fg = None;
                continue;
            }
            if flags & hex::BACKGROUND != 0 {
                bg = Some(r.pixel(f));
            }
            let b = bg.expect("background carried from a non-raw tile");
            for y in 0..th {
                for x in 0..tw {
                    out[(ty + y) * w + tx + x] = b;
                }
            }
            if flags & hex::FOREGROUND != 0 {
                fg = Some(r.pixel(f));
            }
            if flags & hex::ANY_SUBRECTS != 0 {
                let n = r.u8();
                for _ in 0..n {
                    let colour = if flags & hex::SUBRECTS_COLOURED != 0 {
                        r.pixel(f)
                    } else {
                        fg.expect("foreground carried")
                    };
                    let xy = r.u8();
                    let wh = r.u8();
                    let (sx, sy) = (usize::from(xy >> 4), usize::from(xy & 15));
                    let (sw, sh) = (usize::from(wh >> 4) + 1, usize::from(wh & 15) + 1);
                    assert!(sx + sw <= tw && sy + sh <= th);
                    for y in sy..sy + sh {
                        for x in sx..sx + sw {
                            out[(ty + y) * w + tx + x] = colour;
                        }
                    }
                }
                if flags & hex::SUBRECTS_COLOURED != 0 {
                    fg = None;
                }
            }
        }
    }
    assert!(r.done());
    out
}

/// A ZRLE decoder with its own zlib stream, like a client's.
struct ZrleClient {
    zlib: Decompress,
}

impl ZrleClient {
    fn new() -> Self {
        Self {
            zlib: Decompress::new(true),
        }
    }

    fn decode(&mut self, f: &PixelFormat, cpixel: usize, data: &[u8], rect: Rect) -> Vec<u32> {
        let mut r = Reader { data, pos: 0 };
        header(&mut r, rect, number::ZRLE);
        let len = u32::from_be_bytes(r.take(4).try_into().unwrap()) as usize;
        let compressed = r.take(len);
        assert!(r.done());
        let mut plain = Vec::with_capacity(1 << 20);
        let before = self.zlib.total_out();
        self.zlib
            .decompress_vec(compressed, &mut plain, FlushDecompress::Sync)
            .unwrap();
        assert_eq!(self.zlib.total_out() - before, plain.len() as u64);
        let mut t = Reader {
            data: &plain,
            pos: 0,
        };
        let cpx = |t: &mut Reader| -> u32 {
            if cpixel == 3 {
                let b = t.take(3);
                // The kept bytes of the client's value; see CPixel.
                let mut full = [0u8; 4];
                let skip_first = {
                    let used = (u32::from(f.red_max) << f.red_shift)
                        | (u32::from(f.green_max) << f.green_shift)
                        | (u32::from(f.blue_max) << f.blue_shift);
                    let low = used & 0xff00_0000 == 0;
                    if f.big_endian {
                        low
                    } else {
                        !low
                    }
                };
                if skip_first {
                    full[1..].copy_from_slice(b);
                } else {
                    full[..3].copy_from_slice(b);
                }
                if f.big_endian {
                    u32::from_be_bytes(full)
                } else {
                    u32::from_le_bytes(full)
                }
            } else {
                t.pixel(f)
            }
        };
        let run = |t: &mut Reader| -> usize {
            let mut n = 1;
            loop {
                let b = t.u8();
                n += usize::from(b);
                if b != 255 {
                    return n;
                }
            }
        };
        let (w, h) = (rect.width as usize, rect.height as usize);
        let mut out = vec![u32::MAX; w * h];
        for ty in (0..h).step_by(64) {
            for tx in (0..w).step_by(64) {
                let (tw, th) = ((w - tx).min(64), (h - ty).min(64));
                let mut tile = Vec::with_capacity(tw * th);
                let sub = t.u8();
                match sub {
                    0 => (0..tw * th).for_each(|_| tile.push(cpx(&mut t))),
                    1 => {
                        let v = cpx(&mut t);
                        tile.resize(tw * th, v);
                    }
                    2..=16 => {
                        let pal: Vec<u32> = (0..sub).map(|_| cpx(&mut t)).collect();
                        let bits = match sub {
                            2 => 1,
                            3..=4 => 2,
                            _ => 4,
                        };
                        for _ in 0..th {
                            let row = t.take((tw * bits).div_ceil(8));
                            for x in 0..tw {
                                let bit = x * bits;
                                let i = (row[bit / 8] >> (8 - bits - bit % 8)) & ((1 << bits) - 1);
                                tile.push(pal[usize::from(i)]);
                            }
                        }
                    }
                    128 => {
                        while tile.len() < tw * th {
                            let v = cpx(&mut t);
                            let n = run(&mut t);
                            tile.extend(std::iter::repeat_n(v, n));
                        }
                    }
                    130..=255 => {
                        let pal: Vec<u32> = (0..sub - 128).map(|_| cpx(&mut t)).collect();
                        while tile.len() < tw * th {
                            let i = t.u8();
                            let n = if i & 0x80 != 0 { run(&mut t) } else { 1 };
                            tile.extend(std::iter::repeat_n(pal[usize::from(i & 0x7f)], n));
                        }
                    }
                    other => panic!("subencoding {other}"),
                }
                assert_eq!(tile.len(), tw * th);
                for y in 0..th {
                    for x in 0..tw {
                        out[(ty + y) * w + tx + x] = tile[y * tw + x];
                    }
                }
            }
        }
        assert!(t.done());
        out
    }
}

fn bgra(width: usize, height: usize, f: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
    let mut v = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            let [r, g, b] = f(x, y);
            v.extend_from_slice(&[b, g, r, 255]);
        }
    }
    v
}

/// A desktop-like picture: flat panels, a frame, text-like specks and a
/// gradient, which exercises every tile kind.
fn desktop(width: usize, height: usize) -> Vec<u8> {
    bgra(width, height, |x, y| {
        if y < 12 {
            [30, 30, 40]
        } else if x % 97 == 0 || y % 89 == 0 {
            [200, 200, 200]
        } else if (x / 3 + y / 5) % 11 == 0 && x > 40 && x < 300 {
            [250, 250, 250]
        } else if x > width / 2 {
            [(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]
        } else {
            [20, 90, 60]
        }
    })
}

fn all_methods(image: &Image, rect: Rect) {
    for f in formats() {
        let c = Converter::new(f);
        let want = expected(&c, image, rect);
        let mut out = Vec::new();
        Encoder::new(c.clone(), Method::Raw).rect(image, rect, &mut out);
        assert_eq!(decode_raw(&f, &out, rect), want, "raw {f:?}");
        let mut out = Vec::new();
        Encoder::new(c.clone(), Method::Hextile).rect(image, rect, &mut out);
        assert_eq!(decode_hextile(&f, &out, rect), want, "hextile {f:?}");
        let mut enc = Encoder::new(c.clone(), Method::Zrle);
        let cp = CPixel::of(&c).size;
        let mut client = ZrleClient::new();
        // Twice through the same stream, as a connection does.
        for _ in 0..2 {
            let mut out = Vec::new();
            enc.rect(image, rect, &mut out);
            assert_eq!(client.decode(&f, cp, &out, rect), want, "zrle {f:?}");
        }
    }
}

#[test]
fn a_desktop_decodes_back_in_every_encoding_and_format() {
    let (w, h) = (333, 141);
    let pixels = desktop(w, h);
    let image = Image {
        width: w,
        height: h,
        pixels: &pixels,
    };
    all_methods(
        &image,
        Rect {
            x: 0,
            y: 0,
            width: w as u16,
            height: h as u16,
        },
    );
    // A rectangle that starts and ends mid-tile.
    all_methods(
        &image,
        Rect {
            x: 7,
            y: 13,
            width: 150,
            height: 77,
        },
    );
}

#[test]
fn flat_areas_encode_small() {
    let (w, h) = (256, 256);
    let pixels = bgra(w, h, |_, _| [10, 20, 30]);
    let image = Image {
        width: w,
        height: h,
        pixels: &pixels,
    };
    let rect = Rect {
        x: 0,
        y: 0,
        width: 256,
        height: 256,
    };
    let c = Converter::new(PixelFormat::BGRX);
    let mut hextile = Vec::new();
    Encoder::new(c.clone(), Method::Hextile).rect(&image, rect, &mut hextile);
    // One background, then a bare flag byte for each of the other 255 tiles.
    assert_eq!(hextile.len(), 12 + 1 + 4 + 255);
    let mut zrle = Vec::new();
    Encoder::new(c, Method::Zrle).rect(&image, rect, &mut zrle);
    assert!(zrle.len() < 64, "{} bytes", zrle.len());
}

#[test]
fn the_clients_order_picks_the_method() {
    assert_eq!(
        Method::choose(&[number::HEXTILE, number::ZRLE]),
        Method::Hextile
    );
    assert_eq!(
        Method::choose(&[7, number::ZRLE, number::RAW]),
        Method::Zrle
    );
    assert_eq!(Method::choose(&[number::DESKTOP_SIZE]), Method::Raw);
    assert_eq!(Method::choose(&[]), Method::Raw);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn random_pictures_decode_back(
        w in 1usize..80,
        h in 1usize..80,
        colours in 1usize..300,
        seed in any::<u64>(),
    ) {
        // A few colours for paletted tiles, many for raw ones.
        let pixels = bgra(w, h, |x, y| {
            let v = (seed ^ (x as u64 * 7919 + y as u64 * 104_729)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            let k = (v >> 40) as usize % colours;
            [(k * 37) as u8, (k * 91) as u8, (k * 13) as u8]
        });
        let image = Image { width: w, height: h, pixels: &pixels };
        all_methods(&image, Rect { x: 0, y: 0, width: w as u16, height: h as u16 });
    }
}
