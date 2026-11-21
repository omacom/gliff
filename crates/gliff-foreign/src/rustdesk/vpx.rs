//! VP8 and VP9 decoding with the system's libvpx, the codecs every
//! RustDesk can send, and the conversion of its pictures to BGRX.

use std::ffi::{c_char, c_int, c_long, c_uint, c_void, CStr};
use std::ptr;

#[repr(C)]
struct CodecCtx {
    name: *const c_char,
    iface: *const c_void,
    err: c_int,
    err_detail: *const c_char,
    init_flags: c_long,
    config: *const c_void,
    priv_: *mut c_void,
}

#[repr(C)]
struct DecCfg {
    threads: c_uint,
    w: c_uint,
    h: c_uint,
}

#[repr(C)]
struct Image {
    fmt: c_int,
    cs: c_int,
    range: c_int,
    w: c_uint,
    h: c_uint,
    bit_depth: c_uint,
    d_w: c_uint,
    d_h: c_uint,
    r_w: c_uint,
    r_h: c_uint,
    x_chroma_shift: c_uint,
    y_chroma_shift: c_uint,
    planes: [*mut u8; 4],
    stride: [c_int; 4],
    bps: c_int,
    user_priv: *mut c_void,
    img_data: *mut u8,
    img_data_owner: c_int,
    self_allocd: c_int,
    fb_priv: *mut c_void,
}

/// `VPX_DECODER_ABI_VERSION`: 3 + `VPX_CODEC_ABI_VERSION` (4 +
/// `VPX_IMAGE_ABI_VERSION`, 5).
const DECODER_ABI_VERSION: c_int = 12;
const IMG_FMT_PLANAR: c_int = 0x100;
const IMG_FMT_HIGHBITDEPTH: c_int = 0x800;
const IMG_FMT_I420: c_int = IMG_FMT_PLANAR | 2;
const IMG_FMT_I444: c_int = IMG_FMT_PLANAR | 6;
const CS_BT_709: c_int = 2;
const CR_FULL_RANGE: c_int = 1;

#[link(name = "vpx")]
unsafe extern "C" {
    fn vpx_codec_vp8_dx() -> *const c_void;
    fn vpx_codec_vp9_dx() -> *const c_void;
    fn vpx_codec_dec_init_ver(
        ctx: *mut CodecCtx,
        iface: *const c_void,
        cfg: *const DecCfg,
        flags: c_long,
        ver: c_int,
    ) -> c_int;
    fn vpx_codec_decode(
        ctx: *mut CodecCtx,
        data: *const u8,
        data_sz: c_uint,
        user_priv: *mut c_void,
        deadline: c_long,
    ) -> c_int;
    fn vpx_codec_get_frame(ctx: *mut CodecCtx, iter: *mut *const c_void) -> *const Image;
    fn vpx_codec_destroy(ctx: *mut CodecCtx) -> c_int;
    fn vpx_codec_err_to_string(err: c_int) -> *const c_char;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Vp8,
    Vp9,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::Vp8 => "VP8",
            Codec::Vp9 => "VP9",
        }
    }
}

/// A decoded picture, BGRX, `width * 4` bytes a row.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub bgrx: Vec<u8>,
}

pub struct Decoder {
    // Boxed: libvpx keeps pointers into the context.
    ctx: Box<CodecCtx>,
    pub codec: Codec,
}

// SAFETY: the context is only touched through `&mut self`; libvpx keeps no
// thread-local state for it.
unsafe impl Send for Decoder {}

fn error_text(err: c_int) -> String {
    // SAFETY: libvpx returns a static string for any code.
    let text = unsafe { vpx_codec_err_to_string(err) };
    if text.is_null() {
        return format!("error {err}");
    }
    // SAFETY: a NUL-terminated static string, checked non-null above.
    unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned()
}

impl Decoder {
    pub fn new(codec: Codec) -> anyhow::Result<Self> {
        let mut ctx = Box::new(CodecCtx {
            name: ptr::null(),
            iface: ptr::null(),
            err: 0,
            err_detail: ptr::null(),
            init_flags: 0,
            config: ptr::null(),
            priv_: ptr::null_mut(),
        });
        let cfg = DecCfg {
            threads: std::thread::available_parallelism().map_or(1, |n| n.get().min(8)) as c_uint,
            w: 0,
            h: 0,
        };
        // SAFETY: plain constructors of libvpx's static interfaces.
        let iface = unsafe {
            match codec {
                Codec::Vp8 => vpx_codec_vp8_dx(),
                Codec::Vp9 => vpx_codec_vp9_dx(),
            }
        };
        // SAFETY: `ctx` and `cfg` are valid for the call and laid out as
        // the libvpx headers (ABI 12) declare.
        let err = unsafe { vpx_codec_dec_init_ver(&mut *ctx, iface, &cfg, 0, DECODER_ABI_VERSION) };
        anyhow::ensure!(
            err == 0,
            "libvpx cannot decode {}: {}",
            codec.name(),
            error_text(err)
        );
        Ok(Self { ctx, codec })
    }

    /// Decode one compressed frame; the newest picture it yields, if any.
    pub fn decode(&mut self, data: &[u8]) -> anyhow::Result<Option<Picture>> {
        let len = c_uint::try_from(data.len())?;
        // SAFETY: an initialized context and a buffer of `len` bytes.
        let err =
            unsafe { vpx_codec_decode(&mut *self.ctx, data.as_ptr(), len, ptr::null_mut(), 0) };
        anyhow::ensure!(
            err == 0,
            "{} decoding failed: {}",
            self.codec.name(),
            error_text(err)
        );
        let mut iter: *const c_void = ptr::null();
        let mut newest = None;
        loop {
            // SAFETY: the iterator starts null and is libvpx's own after.
            let img = unsafe { vpx_codec_get_frame(&mut *self.ctx, &mut iter) };
            if img.is_null() {
                break;
            }
            // SAFETY: libvpx returned a valid image, alive until the next
            // decode call.
            newest = Some(unsafe { to_bgrx(&*img) }?);
        }
        Ok(newest)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: initialized in `new`, destroyed once.
        unsafe { vpx_codec_destroy(&mut *self.ctx) };
    }
}

/// # Safety
/// `img` must be a picture libvpx just returned: its planes valid for its
/// strides and size.
unsafe fn to_bgrx(img: &Image) -> anyhow::Result<Picture> {
    anyhow::ensure!(
        img.fmt & IMG_FMT_HIGHBITDEPTH == 0,
        "high bit depth pictures are not supported"
    );
    let (w, h) = (img.d_w as usize, img.d_h as usize);
    let (sx, sy) = match img.fmt {
        IMG_FMT_I420 => (1, 1),
        IMG_FMT_I444 => (0, 0),
        f => anyhow::bail!("pictures in libvpx format {f:#x} are not supported"),
    };
    let rows = |plane: usize, height: usize| -> &[u8] {
        let stride = img.stride[plane] as usize;
        // SAFETY: the caller vouches the plane holds `height` rows of
        // `stride` bytes.
        unsafe { std::slice::from_raw_parts(img.planes[plane], stride * height) }
    };
    let ch = (h + sy) >> sy;
    let planes = Planes {
        y: rows(0, h),
        u: rows(1, ch),
        v: rows(2, ch),
        strides: [
            img.stride[0] as usize,
            img.stride[1] as usize,
            img.stride[2] as usize,
        ],
        shift: (sx, sy),
    };
    let matrix = Matrix::of(img.cs == CS_BT_709, img.range == CR_FULL_RANGE);
    Ok(Picture {
        width: w as u32,
        height: h as u32,
        bgrx: planes.to_bgrx(w, h, matrix),
    })
}

/// Fixed-point YCbCr to RGB coefficients, scaled by 2^16.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Matrix {
    y_off: i32,
    y_mul: i32,
    rv: i32,
    gu: i32,
    gv: i32,
    bu: i32,
}

impl Matrix {
    /// BT.601 unless the stream says BT.709; studio range unless full.
    pub fn of(bt709: bool, full: bool) -> Self {
        let (kr, kb): (f64, f64) = if bt709 {
            (0.2126, 0.0722)
        } else {
            (0.299, 0.114)
        };
        let kg = 1.0 - kr - kb;
        let (y_off, y_scale, c_scale) = if full {
            (0, 1.0, 1.0)
        } else {
            (16, 255.0 / 219.0, 255.0 / 224.0)
        };
        let f = |v: f64| (v * 65536.0).round() as i32;
        Self {
            y_off,
            y_mul: f(y_scale),
            rv: f(2.0 * (1.0 - kr) * c_scale),
            gu: f(2.0 * (1.0 - kb) * kb / kg * c_scale),
            gv: f(2.0 * (1.0 - kr) * kr / kg * c_scale),
            bu: f(2.0 * (1.0 - kb) * c_scale),
        }
    }

    fn pixel(&self, y: u8, u: u8, v: u8) -> [u8; 4] {
        let y = (i32::from(y) - self.y_off) * self.y_mul;
        let (u, v) = (i32::from(u) - 128, i32::from(v) - 128);
        let c = |x: i32| ((x + 32768) >> 16).clamp(0, 255) as u8;
        [
            c(y + self.bu * u),
            c(y - self.gu * u - self.gv * v),
            c(y + self.rv * v),
            255,
        ]
    }
}

/// Planar YUV with chroma subsampled by `shift`.
pub struct Planes<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub strides: [usize; 3],
    pub shift: (usize, usize),
}

impl Planes<'_> {
    pub fn to_bgrx(&self, w: usize, h: usize, m: Matrix) -> Vec<u8> {
        let mut out = vec![0u8; w * h * 4];
        let (sx, sy) = self.shift;
        for (row, line) in out.chunks_exact_mut(w * 4).enumerate() {
            let y = &self.y[row * self.strides[0]..];
            let u = &self.u[(row >> sy) * self.strides[1]..];
            let v = &self.v[(row >> sy) * self.strides[2]..];
            for (col, px) in line.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                px.copy_from_slice(&m.pixel(y[col], u[col >> sx], v[col >> sx]));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn studio_range_greys_and_primaries() {
        let m = Matrix::of(false, false);
        assert_eq!(m.pixel(16, 128, 128), [0, 0, 0, 255]);
        assert_eq!(m.pixel(235, 128, 128), [255, 255, 255, 255]);
        // BT.601 studio red: Y 81, Cb 90, Cr 240.
        let [b, g, r, _] = m.pixel(81, 90, 240);
        assert!(r >= 253 && g <= 2 && b <= 2, "{r} {g} {b}");
        let full = Matrix::of(true, true);
        assert_eq!(full.pixel(0, 128, 128), [0, 0, 0, 255]);
        assert_eq!(full.pixel(255, 128, 128), [255, 255, 255, 255]);
    }

    #[test]
    fn chroma_covers_its_block_with_odd_sizes() {
        // 3x3 luma, 2x2 chroma: the last column and row take the last
        // chroma sample.
        let y = [16u8; 9];
        let u = [128u8, 128, 128, 90];
        let v = [128u8, 128, 128, 240];
        let p = Planes {
            y: &y,
            u: &u,
            v: &v,
            strides: [3, 2, 2],
            shift: (1, 1),
        };
        let out = p.to_bgrx(3, 3, Matrix::of(false, false));
        assert_eq!(out.len(), 36);
        // (2, 2) uses chroma (1, 1); (0, 0) is grey black.
        assert_eq!(&out[0..4], &[0, 0, 0, 255]);
        assert_ne!(&out[32..36], &[0, 0, 0, 255]);
    }

    #[test]
    fn both_decoders_open() {
        assert!(Decoder::new(Codec::Vp8).is_ok());
        assert!(Decoder::new(Codec::Vp9).is_ok());
    }

    #[test]
    fn decodes_a_real_vp9_frame() {
        // One red 64x48 frame, as ffmpeg's libvpx-vp9 encodes it.
        let frame = include_bytes!("testdata/red-64x48.vp9");
        let mut d = Decoder::new(Codec::Vp9).unwrap();
        let p = d.decode(frame).unwrap().expect("a picture");
        assert_eq!((p.width, p.height), (64, 48));
        assert_eq!(p.bgrx.len(), 64 * 48 * 4);
        for px in p.bgrx.as_chunks::<4>().0 {
            let [b, g, r, x] = [px[0], px[1], px[2], px[3]];
            assert!(r >= 250 && g <= 5 && b <= 5 && x == 255, "{r} {g} {b}");
        }
    }

    #[test]
    fn garbage_is_an_error_not_a_crash() {
        let mut d = Decoder::new(Codec::Vp9).unwrap();
        assert!(d.decode(&[0xFF; 64]).is_err());
    }
}
