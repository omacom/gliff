//! HEVC Main encode through VA-API, for pictures larger than H.264 allows.
//!
//! The same low-delay layout as the H.264 encoder: an IDR then P frames,
//! one reference, no reordering, two driver-owned reconstruction surfaces
//! alternating as the reference. Unlike it, the driver writes the slice
//! headers, and replaces the packed VPS, SPS and PPS handed to it with its
//! own, adjusted to the coding tools the hardware uses (Mesa's radeonsi
//! does this), so the decoder parses whatever comes back.

use std::sync::Arc;

use crate::bindings as va;
use crate::context::{Buffer, Config, Context};
use crate::display::{Caps, Display, VaCodec};
use crate::encoder::{packed_header, EncodedPacket, PendingEncode};
use crate::h264::annexb::has_start_code;
use crate::hevc::nal::{nal_units, IDR_W_RADL, TRAIL_R, VPS};
use crate::hevc::parser::{write_parameter_sets, write_slice_header, EncodeParams};
use crate::settings::EncoderSettings;
use crate::surface::{Surface, UsageHint};
use crate::{Error, Result};

const PIC_INIT_QP: u8 = 26;
/// Coding tree blocks of 64x64, coding blocks down to 8x8, transform blocks
/// from 4x4 to 32x32: what AMD's VCN encodes.
const LOG2_MIN_CB_MINUS3: u8 = 0;
const LOG2_DIFF_MAX_MIN_CB: u8 = 3;
const LOG2_MIN_TB_MINUS2: u8 = 0;
const LOG2_DIFF_MAX_MIN_TB: u8 = 3;
const LOG2_MAX_POC_LSB_MINUS4: u32 = 12;
const CTB_SIZE: u32 = 64;
/// Coded sizes are whole minimum coding blocks.
const MIN_CB_SIZE: u32 = 8;

/// HEVC slice_type values (7.4.7.1).
const SLICE_P: u8 = 1;
const SLICE_I: u8 = 2;

pub struct HevcEncoder {
    display: Arc<Display>,
    settings: EncoderSettings,
    _config: Config,
    context: Context,
    input: Surface,
    recon: [Surface; 2],
    coded: Buffer,
    /// What the packed headers declare.
    params: EncodeParams,
    /// VPS, SPS and PPS as packed headers.
    parameter_sets: [Vec<u8>; 3],
    /// The POC of the picture in each reconstruction slot.
    slots: [Option<i32>; 2],
    current_ref: Option<usize>,
    poc: i32,
    started: bool,
    pending_rate: Option<(u32, u32, u32)>,
}

impl HevcEncoder {
    /// The largest coded size the driver encodes.
    pub fn max_coded_extent(caps: &Caps) -> (u32, u32) {
        let or = |v: u32| if v == 0 { 4096 } else { v };
        (or(caps.max_width), or(caps.max_height))
    }

    /// The coded size for a picture: whole minimum coding blocks. The
    /// driver crops back to the picture with a conformance window.
    pub fn coded_extent(width: u32, height: u32) -> (u32, u32) {
        (
            width.div_ceil(MIN_CB_SIZE) * MIN_CB_SIZE,
            height.div_ceil(MIN_CB_SIZE) * MIN_CB_SIZE,
        )
    }

    pub fn new(display: &Arc<Display>, caps: &Caps, settings: EncoderSettings) -> Result<Self> {
        if caps.codec != VaCodec::Hevc {
            return Err(Error::Unsupported(format!(
                "HEVC encoder given {} capabilities",
                caps.codec.name()
            )));
        }
        caps.can_encode()?;
        let entrypoint = caps.encode_entrypoint.expect("checked by can_encode");
        let (cw, ch) = Self::coded_extent(settings.width, settings.height);
        let (max_w, max_h) = Self::max_coded_extent(caps);
        if cw > max_w || ch > max_h {
            return Err(Error::Unsupported(format!(
                "{cw}x{ch} exceeds the HEVC encoder maximum {max_w}x{max_h}"
            )));
        }
        let config = Config::new(
            display,
            va::VAProfileHEVCMain,
            entrypoint,
            &[
                (va::VAConfigAttribRTFormat, va::VA_RT_FORMAT_YUV420),
                (va::VAConfigAttribRateControl, va::VA_RC_CBR),
                (
                    va::VAConfigAttribEncPackedHeaders,
                    va::VA_ENC_PACKED_HEADER_SEQUENCE
                        | va::VA_ENC_PACKED_HEADER_PICTURE
                        | va::VA_ENC_PACKED_HEADER_SLICE,
                ),
            ],
        )?;
        // The surfaces cover whole coding tree blocks: the hardware reads
        // and writes them in 64x64 units.
        let (sw, sh) = (cw.next_multiple_of(CTB_SIZE), ch.next_multiple_of(CTB_SIZE));
        let input = Surface::new_nv12(display, sw, sh, UsageHint::Encoder)?;
        let recon = [
            Surface::new_nv12(display, sw, sh, UsageHint::Encoder)?,
            Surface::new_nv12(display, sw, sh, UsageHint::Encoder)?,
        ];
        let context = Context::new(display, &config, cw, ch, &[&input, &recon[0], &recon[1]])?;
        let size = ((cw * ch * 2) as usize).max(1 << 20).next_multiple_of(4096);
        let coded = Buffer::coded(&context, size)?;
        tracing::debug!(
            entrypoint = crate::display::entrypoint_name(entrypoint),
            coded = format!("{cw}x{ch}"),
            coded_buffer = size,
            "va-api hevc encoder"
        );
        let params = EncodeParams {
            width: settings.width,
            height: settings.height,
            level_idc: level_idc(cw, ch),
            log2_max_poc_lsb_minus4: LOG2_MAX_POC_LSB_MINUS4,
            log2_min_cb_minus3: LOG2_MIN_CB_MINUS3 as u32,
            log2_diff_max_min_cb: LOG2_DIFF_MAX_MIN_CB as u32,
            log2_min_tb_minus2: LOG2_MIN_TB_MINUS2 as u32,
            log2_diff_max_min_tb: LOG2_DIFF_MAX_MIN_TB as u32,
            max_transform_hierarchy_depth: LOG2_DIFF_MAX_MIN_TB as u32,
            amp_enabled: true,
            init_qp: PIC_INIT_QP as u32,
        };
        let parameter_sets = write_parameter_sets(&params);
        Ok(Self {
            display: display.clone(),
            settings,
            _config: config,
            context,
            input,
            recon,
            coded,
            params,
            parameter_sets,
            slots: [None, None],
            current_ref: None,
            poc: 0,
            started: false,
            pending_rate: None,
        })
    }

    pub fn settings(&self) -> &EncoderSettings {
        &self.settings
    }

    /// The surface the caller fills with the picture to encode. It may be
    /// larger than the picture; the picture goes in its top-left corner.
    pub fn input(&self) -> &Surface {
        &self.input
    }

    /// Change the CBR target, the frame rate it is spread over and the
    /// rate-control buffer, from the next frame on.
    pub fn set_rate(&mut self, bitrate: u32, framerate: u32, vbv_ms: u32) {
        if bitrate != self.settings.bitrate
            || framerate != self.settings.framerate
            || vbv_ms != self.settings.vbv_ms
        {
            self.pending_rate = Some((bitrate, framerate.max(1), vbv_ms.max(20)));
        }
    }

    /// Encode the input surface. The caller must have finished writing it.
    pub fn submit(&mut self, force_keyframe: bool) -> Result<PendingEncode> {
        let rate_change = self.pending_rate.take();
        if let Some((b, f, v)) = rate_change {
            self.settings.bitrate = b;
            self.settings.framerate = f;
            self.settings.vbv_ms = v;
        }
        let idr = force_keyframe || !self.started || self.current_ref.is_none();
        if idr {
            self.poc = 0;
            self.current_ref = None;
        }
        let setup_slot = self.current_ref.map_or(0, |r| 1 - r);
        let ref_slot = if idr { None } else { self.current_ref };
        let ctx = &self.context;
        let mut buffers = Vec::new();
        if idr || !self.started {
            buffers.push(Buffer::new(
                ctx,
                va::VAEncSequenceParameterBufferType,
                &self.sequence_params(),
            )?);
        }
        if !self.started || rate_change.is_some() {
            buffers.extend(self.rate_params()?);
        }
        if idr {
            // The driver replaces these with its own and writes them ahead
            // of the slice.
            let [vps, sps, pps] = &self.parameter_sets;
            for (kind, nal) in [
                (va::VAEncPackedHeaderSequence, vps),
                (va::VAEncPackedHeaderSequence, sps),
                (va::VAEncPackedHeaderPicture, pps),
            ] {
                buffers.extend(packed_header(ctx, kind, nal, None)?);
            }
        }
        // The slice header too: without it the driver writes no parameter
        // sets at all.
        let nal_type = if idr { IDR_W_RADL } else { TRAIL_R };
        buffers.extend(packed_header(
            ctx,
            va::VAEncPackedHeaderSlice,
            &write_slice_header(&self.params, nal_type, self.poc),
            None,
        )?);
        buffers.push(Buffer::new(
            ctx,
            va::VAEncPictureParameterBufferType,
            &self.picture_params(idr, setup_slot, ref_slot),
        )?);
        buffers.push(Buffer::new(
            ctx,
            va::VAEncSliceParameterBufferType,
            &self.slice_params(idr, ref_slot),
        )?);
        ctx.render(&self.input, &buffers)?;

        self.slots[setup_slot] = Some(self.poc);
        self.current_ref = Some(setup_slot);
        self.poc += 1;
        self.started = true;
        if let Some((b, f, v)) = rate_change {
            tracing::info!(
                bitrate = b,
                framerate = f,
                vbv_ms = v,
                "encoder rate changed"
            );
        }
        Ok(PendingEncode::new(idr, buffers))
    }

    /// Wait for a submitted encode and collect its access unit.
    pub fn finish(&mut self, pending: PendingEncode) -> Result<EncodedPacket> {
        let (data, status) = self.coded.read_coded()?;
        let failed = va::VA_CODED_BUF_STATUS_BAD_BITSTREAM
            | va::VA_CODED_BUF_STATUS_FRAME_SIZE_OVERFLOW
            | va::VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK;
        if status & failed != 0 {
            return Err(Error::Unsupported(format!(
                "encode failed with coded buffer status {status:#x}"
            )));
        }
        if !has_start_code(&data[..data.len().min(5)]) {
            return Err(Error::Bitstream("HEVC encoder output is not Annex B"));
        }
        // The decoder needs the parameter sets with every keyframe.
        if pending.keyframe() && !nal_units(&data).iter().any(|n| n.nal_type == VPS) {
            return Err(Error::Bitstream(
                "HEVC encoder wrote a keyframe without parameter sets",
            ));
        }
        Ok(EncodedPacket {
            keyframe: pending.keyframe(),
            data,
        })
    }

    fn coded_extent_of(&self) -> (u32, u32) {
        Self::coded_extent(self.settings.width, self.settings.height)
    }

    fn sequence_params(&self) -> va::VAEncSequenceParameterBufferHEVC {
        let (cw, ch) = self.coded_extent_of();
        // SAFETY: a plain C struct; every field accepts zero.
        let mut seq: va::VAEncSequenceParameterBufferHEVC = unsafe { std::mem::zeroed() };
        seq.general_profile_idc = 1;
        seq.general_level_idc = level_idc(cw, ch);
        seq.general_tier_flag = 0;
        seq.intra_period = 0;
        seq.intra_idr_period = 0;
        seq.ip_period = 1;
        seq.bits_per_second = self.settings.bitrate;
        seq.pic_width_in_luma_samples = cw as u16;
        seq.pic_height_in_luma_samples = ch as u16;
        seq.log2_min_luma_coding_block_size_minus3 = LOG2_MIN_CB_MINUS3;
        seq.log2_diff_max_min_luma_coding_block_size = LOG2_DIFF_MAX_MIN_CB;
        seq.log2_min_transform_block_size_minus2 = LOG2_MIN_TB_MINUS2;
        seq.log2_diff_max_min_transform_block_size = LOG2_DIFF_MAX_MIN_TB;
        seq.max_transform_hierarchy_depth_inter = LOG2_DIFF_MAX_MIN_TB;
        seq.max_transform_hierarchy_depth_intra = LOG2_DIFF_MAX_MIN_TB;
        // SAFETY: writing bitfields of a zeroed union member.
        unsafe {
            let b = &mut seq.seq_fields.bits;
            b.set_chroma_format_idc(1);
            b.set_amp_enabled_flag(1);
            b.set_sample_adaptive_offset_enabled_flag(0);
            // VCN cannot predict motion vectors from the previous picture.
            b.set_sps_temporal_mvp_enabled_flag(0);
            b.set_low_delay_seq(1);
        }
        seq
    }

    fn rate_params(&self) -> Result<Vec<Buffer>> {
        crate::encoder::rate_params(&self.context, &self.settings)
    }

    fn picture(&self, slot: usize) -> va::VAPictureHEVC {
        va::VAPictureHEVC {
            picture_id: self.recon[slot].id,
            pic_order_cnt: self.slots[slot].expect("slot holds a picture"),
            flags: 0,
            va_reserved: [0; 4],
        }
    }

    fn picture_params(
        &self,
        idr: bool,
        setup_slot: usize,
        ref_slot: Option<usize>,
    ) -> va::VAEncPictureParameterBufferHEVC {
        // SAFETY: a plain C struct; every field accepts zero.
        let mut pic: va::VAEncPictureParameterBufferHEVC = unsafe { std::mem::zeroed() };
        pic.decoded_curr_pic = va::VAPictureHEVC {
            picture_id: self.recon[setup_slot].id,
            pic_order_cnt: self.poc,
            flags: 0,
            va_reserved: [0; 4],
        };
        for r in pic.reference_frames.iter_mut() {
            *r = invalid_picture();
        }
        if let Some(r) = ref_slot {
            pic.reference_frames[0] = self.picture(r);
        }
        pic.coded_buf = self.coded.id;
        pic.collocated_ref_pic_index = 0xff;
        pic.pic_init_qp = PIC_INIT_QP;
        pic.diff_cu_qp_delta_depth = 0;
        pic.num_ref_idx_l0_default_active_minus1 = 0;
        pic.num_ref_idx_l1_default_active_minus1 = 0;
        pic.nal_unit_type = if idr { IDR_W_RADL } else { TRAIL_R };
        // SAFETY: writing bitfields of a zeroed union member.
        unsafe {
            let b = &mut pic.pic_fields.bits;
            b.set_idr_pic_flag(idr as u32);
            // coding_type: 1 = I, 2 = P.
            b.set_coding_type(if idr { 1 } else { 2 });
            b.set_reference_pic_flag(1);
            b.set_cu_qp_delta_enabled_flag(1);
            b.set_pps_loop_filter_across_slices_enabled_flag(1);
        }
        pic
    }

    fn slice_params(
        &self,
        idr: bool,
        ref_slot: Option<usize>,
    ) -> va::VAEncSliceParameterBufferHEVC {
        let (cw, ch) = self.coded_extent_of();
        // SAFETY: a plain C struct; every field accepts zero.
        let mut sl: va::VAEncSliceParameterBufferHEVC = unsafe { std::mem::zeroed() };
        sl.slice_segment_address = 0;
        sl.num_ctu_in_slice = cw.div_ceil(CTB_SIZE) * ch.div_ceil(CTB_SIZE);
        sl.slice_type = if idr { SLICE_I } else { SLICE_P };
        sl.slice_pic_parameter_set_id = 0;
        sl.num_ref_idx_l0_active_minus1 = 0;
        sl.num_ref_idx_l1_active_minus1 = 0;
        for r in sl
            .ref_pic_list0
            .iter_mut()
            .chain(sl.ref_pic_list1.iter_mut())
        {
            *r = invalid_picture();
        }
        if let Some(r) = ref_slot {
            sl.ref_pic_list0[0] = self.picture(r);
        }
        sl.max_num_merge_cand = 5;
        // SAFETY: writing bitfields of a zeroed union member.
        unsafe {
            let b = &mut sl.slice_fields.bits;
            b.set_last_slice_of_pic_flag(1);
            b.set_slice_loop_filter_across_slices_enabled_flag(1);
        }
        sl
    }
}

impl Drop for HevcEncoder {
    fn drop(&mut self) {
        let _ = self.input.sync();
        let _ = &self.display;
    }
}

fn invalid_picture() -> va::VAPictureHEVC {
    va::VAPictureHEVC {
        picture_id: va::VA_INVALID_SURFACE,
        pic_order_cnt: 0,
        flags: va::VA_PICTURE_HEVC_INVALID,
        va_reserved: [0; 4],
    }
}

/// `general_level_idc` (30 x level) for a picture size at up to 60 fps:
/// level 5.1 up to 4096x2176, level 6.1 above.
fn level_idc(width: u32, height: u32) -> u8 {
    // MaxLumaPs for levels 5.x (A.4.1).
    if width as u64 * height as u64 <= 8_912_896 {
        153
    } else {
        183
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coded_extent_rounds_up_to_minimum_coding_blocks() {
        assert_eq!(HevcEncoder::coded_extent(6016, 3384), (6016, 3384));
        assert_eq!(HevcEncoder::coded_extent(5121, 2881), (5128, 2888));
    }

    #[test]
    fn level_fits_the_picture() {
        assert_eq!(level_idc(3840, 2160), 153);
        assert_eq!(level_idc(6016, 3384), 183);
    }
}
