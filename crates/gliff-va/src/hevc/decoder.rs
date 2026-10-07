//! HEVC Main decode through VA-API.
//!
//! As for H.264, the application supplies what the hardware does not parse:
//! the SPS and PPS fields, the slice header fields, picture order counts,
//! the reference picture set applied to the DPB, and the reference lists.
//! Every DPB slot is a driver-owned surface that is also the decode output;
//! the caller reads them through their exported dmabufs.

use std::sync::Arc;

use crate::bindings as va;
use crate::context::{Buffer, Config, Context};
use crate::display::{Caps, Display, VaCodec};
use crate::hevc::nal::{nal_units, PPS, SPS};
use crate::hevc::parser::{
    parse_pps, parse_slice_header, parse_sps, Pps, SliceHeader, SliceType, Sps,
};
use crate::surface::{Surface, UsageHint};
use crate::{Error, Result};

struct Stream {
    sps: Sps,
    pps: Option<Pps>,
    context: Context,
    surfaces: Vec<Surface>,
    /// The POC of the picture each surface holds while it is a reference.
    slots: Vec<Option<i32>>,
    last_output: Option<usize>,
}

pub struct HevcDecoder {
    display: Arc<Display>,
    config: Config,
    max_width: u32,
    max_height: u32,
    sps_bytes: Vec<u8>,
    pps_bytes: Vec<u8>,
    stream: Option<Stream>,
    generation: u64,
    /// PicOrderCntVal of the previous picture with TemporalId 0, which the
    /// next picture's POC MSB is derived from (8.3.1).
    prev_tid0_poc: i32,
}

impl HevcDecoder {
    pub fn new(display: &Arc<Display>, caps: &Caps) -> Result<Self> {
        if caps.codec != VaCodec::Hevc {
            return Err(Error::Unsupported(format!(
                "HEVC decoder given {} capabilities",
                caps.codec.name()
            )));
        }
        caps.can_decode()?;
        let config = Config::new(
            display,
            va::VAProfileHEVCMain,
            va::VAEntrypointVLD,
            &[(va::VAConfigAttribRTFormat, va::VA_RT_FORMAT_YUV420)],
        )?;
        let or = |v: u32| if v == 0 { 4096 } else { v };
        Ok(Self {
            display: display.clone(),
            config,
            max_width: or(caps.decode_max_width),
            max_height: or(caps.decode_max_height),
            sps_bytes: Vec::new(),
            pps_bytes: Vec::new(),
            stream: None,
            generation: 0,
            prev_tid0_poc: 0,
        })
    }

    /// Display size of the current stream, once an SPS has been seen.
    pub fn display_size(&self) -> Option<(u32, u32)> {
        self.stream.as_ref().map(|s| s.sps.display_size())
    }

    /// The surfaces pictures land in; replaced when `generation` changes.
    pub fn surfaces(&self) -> &[Surface] {
        self.stream.as_ref().map_or(&[], |s| s.surfaces.as_slice())
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The surface the last `decode` returned, if any.
    pub fn last_output(&self) -> Option<usize> {
        self.stream.as_ref().and_then(|s| s.last_output)
    }

    /// Decode one access unit. Returns the index of the output surface once
    /// the decode has been submitted; `sync` it before reading. `None` for
    /// an access unit with no picture.
    pub fn decode(&mut self, access_unit: &[u8]) -> Result<Option<usize>> {
        let nals = nal_units(access_unit);
        let mut slices: Vec<(&[u8], SliceHeader)> = Vec::new();
        for nal in &nals {
            match nal.nal_type {
                SPS => {
                    if nal.data != self.sps_bytes.as_slice() {
                        let sps = parse_sps(nal.data)?;
                        self.sps_bytes = nal.data.to_vec();
                        self.pps_bytes.clear();
                        self.open_stream(sps)?;
                    }
                }
                PPS => {
                    if nal.data != self.pps_bytes.as_slice() {
                        let pps = parse_pps(nal.data)?;
                        self.pps_bytes = nal.data.to_vec();
                        self.stream
                            .as_mut()
                            .ok_or(Error::Bitstream("PPS before SPS"))?
                            .pps = Some(pps);
                    }
                }
                _ if nal.is_slice() => {
                    let stream = self
                        .stream
                        .as_ref()
                        .ok_or(Error::Bitstream("slice before parameter sets"))?;
                    let pps = stream
                        .pps
                        .as_ref()
                        .ok_or(Error::Bitstream("slice before PPS"))?;
                    let prev = slices.last().map(|(_, h)| h);
                    let header = parse_slice_header(nal.data, &stream.sps, pps, prev)?;
                    slices.push((nal.data, header));
                }
                _ => {}
            }
        }
        let Some((_, first)) = slices.first() else {
            return Ok(None);
        };
        let first = first.clone();
        if !first.first_slice_segment_in_pic {
            return Err(Error::Bitstream("access unit starts mid-picture"));
        }
        let stream = self.stream.as_mut().ok_or(Error::Bitstream("no stream"))?;
        let pps = stream.pps.clone().ok_or(Error::Bitstream("no PPS"))?;

        let poc = derive_poc(&stream.sps, &first, self.prev_tid0_poc);
        // TRAIL_R and the other reference types, all at TemporalId 0 here.
        self.prev_tid0_poc = poc;

        // Apply the RPS (8.3.2): pictures it names stay; the rest leave.
        let refs = if first.is_idr() {
            stream.slots.iter_mut().for_each(|s| *s = None);
            Vec::new()
        } else {
            let wanted: Vec<(i32, bool)> = first
                .st_rps
                .s0
                .iter()
                .chain(&first.st_rps.s1)
                .map(|&(delta, used)| (poc + delta, used))
                .collect();
            for slot in stream.slots.iter_mut() {
                if slot.is_some_and(|p| !wanted.iter().any(|w| w.0 == p)) {
                    *slot = None;
                }
            }
            let mut refs = Vec::new();
            for &(ref_poc, used) in &wanted {
                let slot = stream
                    .slots
                    .iter()
                    .position(|s| *s == Some(ref_poc))
                    .ok_or(Error::Bitstream("reference picture missing from the DPB"))?;
                refs.push(Reference {
                    slot,
                    poc: ref_poc,
                    flags: if !used {
                        0
                    } else if ref_poc < poc {
                        va::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE
                    } else {
                        va::VA_PICTURE_HEVC_RPS_ST_CURR_AFTER
                    },
                });
            }
            refs
        };
        let output = stream
            .slots
            .iter()
            .position(Option::is_none)
            .ok_or(Error::Bitstream("no free DPB slot"))?;

        let intra = slices.iter().all(|(_, h)| h.slice_type == SliceType::I);
        let mut buffers = vec![Buffer::new(
            &stream.context,
            va::VAPictureParameterBufferType,
            &picture_params(stream, &pps, &first, &refs, output, poc, intra),
        )?];
        let count = slices.len();
        for (i, (nal, h)) in slices.iter().enumerate() {
            buffers.push(Buffer::new(
                &stream.context,
                va::VASliceParameterBufferType,
                &slice_params(nal, h, &refs, i + 1 == count)?,
            )?);
            buffers.push(Buffer::data(
                &stream.context,
                va::VASliceDataBufferType,
                nal,
            )?);
        }
        stream.context.render(&stream.surfaces[output], &buffers)?;
        stream.slots[output] = Some(poc);
        stream.last_output = Some(output);
        Ok(Some(output))
    }

    fn open_stream(&mut self, sps: Sps) -> Result<()> {
        let (cw, ch) = sps.coded_size();
        if cw == 0 || ch == 0 || cw > self.max_width || ch > self.max_height {
            return Err(Error::Unsupported(format!(
                "stream size {cw}x{ch} exceeds the HEVC decoder limit {}x{}",
                self.max_width, self.max_height
            )));
        }
        // The DPB the stream declares (current picture included), plus one
        // so the last output stays readable while the next decodes.
        let slots = (sps.max_dec_pic_buffering_minus1 as usize + 2).clamp(3, 17);
        self.stream = None;
        let surfaces = (0..slots)
            .map(|_| Surface::new_nv12(&self.display, cw, ch, UsageHint::Decoder))
            .collect::<Result<Vec<_>>>()?;
        let targets: Vec<&Surface> = surfaces.iter().collect();
        let context = Context::new(&self.display, &self.config, cw, ch, &targets)?;
        tracing::info!(
            coded = format!("{cw}x{ch}"),
            dpb_slots = slots,
            "va-api hevc decoder stream opened"
        );
        self.stream = Some(Stream {
            sps,
            pps: None,
            context,
            surfaces,
            slots: vec![None; slots],
            last_output: None,
        });
        self.generation += 1;
        self.prev_tid0_poc = 0;
        Ok(())
    }
}

/// A DPB picture in the current picture's RPS.
#[derive(Clone, Copy)]
struct Reference {
    slot: usize,
    poc: i32,
    /// `VA_PICTURE_HEVC_RPS_ST_CURR_*`, or 0 for a picture kept only for
    /// later pictures.
    flags: u32,
}

/// PicOrderCntVal (8.3.1) for a picture whose previous TemporalId-0
/// picture had `prev_poc`.
fn derive_poc(sps: &Sps, h: &SliceHeader, prev_poc: i32) -> i32 {
    if h.is_idr() {
        return 0;
    }
    let max = sps.max_poc_lsb() as i32;
    let prev_lsb = prev_poc.rem_euclid(max);
    let prev_msb = prev_poc - prev_lsb;
    let lsb = h.pic_order_cnt_lsb as i32;
    let msb = if lsb < prev_lsb && prev_lsb - lsb >= max / 2 {
        prev_msb + max
    } else if lsb > prev_lsb && lsb - prev_lsb > max / 2 {
        prev_msb - max
    } else {
        prev_msb
    };
    msb + lsb
}

fn va_picture(surface: &Surface, poc: i32, flags: u32) -> va::VAPictureHEVC {
    va::VAPictureHEVC {
        picture_id: surface.id,
        pic_order_cnt: poc,
        flags,
        va_reserved: [0; 4],
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

fn picture_params(
    stream: &Stream,
    pps: &Pps,
    h: &SliceHeader,
    refs: &[Reference],
    output: usize,
    poc: i32,
    intra: bool,
) -> va::VAPictureParameterBufferHEVC {
    let sps = &stream.sps;
    // SAFETY: a plain C struct; every field accepts zero.
    let mut pic: va::VAPictureParameterBufferHEVC = unsafe { std::mem::zeroed() };
    pic.CurrPic = va_picture(&stream.surfaces[output], poc, 0);
    for r in pic.ReferenceFrames.iter_mut() {
        *r = invalid_picture();
    }
    for (dst, r) in pic.ReferenceFrames.iter_mut().zip(refs) {
        *dst = va_picture(&stream.surfaces[r.slot], r.poc, r.flags);
    }
    pic.pic_width_in_luma_samples = sps.pic_width as u16;
    pic.pic_height_in_luma_samples = sps.pic_height as u16;
    pic.sps_max_dec_pic_buffering_minus1 = sps.max_dec_pic_buffering_minus1 as u8;
    pic.bit_depth_luma_minus8 = sps.bit_depth_luma_minus8 as u8;
    pic.bit_depth_chroma_minus8 = sps.bit_depth_chroma_minus8 as u8;
    pic.pcm_sample_bit_depth_luma_minus1 = sps.pcm_bit_depth_luma_minus1 as u8;
    pic.pcm_sample_bit_depth_chroma_minus1 = sps.pcm_bit_depth_chroma_minus1 as u8;
    pic.log2_min_luma_coding_block_size_minus3 = sps.log2_min_cb_minus3 as u8;
    pic.log2_diff_max_min_luma_coding_block_size = sps.log2_diff_max_min_cb as u8;
    pic.log2_min_transform_block_size_minus2 = sps.log2_min_tb_minus2 as u8;
    pic.log2_diff_max_min_transform_block_size = sps.log2_diff_max_min_tb as u8;
    pic.log2_min_pcm_luma_coding_block_size_minus3 = sps.log2_min_pcm_cb_minus3 as u8;
    pic.log2_diff_max_min_pcm_luma_coding_block_size = sps.log2_diff_max_min_pcm_cb as u8;
    pic.max_transform_hierarchy_depth_intra = sps.max_transform_hierarchy_depth_intra as u8;
    pic.max_transform_hierarchy_depth_inter = sps.max_transform_hierarchy_depth_inter as u8;
    pic.init_qp_minus26 = pps.init_qp_minus26 as i8;
    pic.diff_cu_qp_delta_depth = pps.diff_cu_qp_delta_depth as u8;
    pic.pps_cb_qp_offset = pps.cb_qp_offset as i8;
    pic.pps_cr_qp_offset = pps.cr_qp_offset as i8;
    pic.log2_parallel_merge_level_minus2 = pps.log2_parallel_merge_level_minus2 as u8;
    pic.log2_max_pic_order_cnt_lsb_minus4 = sps.log2_max_poc_lsb_minus4 as u8;
    pic.num_short_term_ref_pic_sets = sps.st_rps.len() as u8;
    pic.num_long_term_ref_pic_sps = 0;
    pic.num_ref_idx_l0_default_active_minus1 = pps.num_ref_idx_l0_default_active_minus1 as u8;
    pic.num_ref_idx_l1_default_active_minus1 = pps.num_ref_idx_l1_default_active_minus1 as u8;
    pic.pps_beta_offset_div2 = pps.beta_offset_div2 as i8;
    pic.pps_tc_offset_div2 = pps.tc_offset_div2 as i8;
    pic.num_extra_slice_header_bits = pps.num_extra_slice_header_bits as u8;
    pic.st_rps_bits = h.st_rps_bits;
    // SAFETY: writing bitfields of zeroed union members.
    unsafe {
        let f = &mut pic.pic_fields.bits;
        f.set_chroma_format_idc(sps.chroma_format_idc);
        f.set_pcm_enabled_flag(sps.pcm_enabled as u32);
        f.set_scaling_list_enabled_flag(sps.scaling_list_enabled as u32);
        f.set_transform_skip_enabled_flag(pps.transform_skip_enabled as u32);
        f.set_amp_enabled_flag(sps.amp_enabled as u32);
        f.set_strong_intra_smoothing_enabled_flag(sps.strong_intra_smoothing as u32);
        f.set_sign_data_hiding_enabled_flag(pps.sign_data_hiding_enabled as u32);
        f.set_constrained_intra_pred_flag(pps.constrained_intra_pred as u32);
        f.set_cu_qp_delta_enabled_flag(pps.cu_qp_delta_enabled as u32);
        f.set_transquant_bypass_enabled_flag(pps.transquant_bypass_enabled as u32);
        f.set_pps_loop_filter_across_slices_enabled_flag(
            pps.loop_filter_across_slices_enabled as u32,
        );
        f.set_pcm_loop_filter_disabled_flag(sps.pcm_loop_filter_disabled as u32);
        f.set_NoPicReorderingFlag(1);
        let s = &mut pic.slice_parsing_fields.bits;
        s.set_lists_modification_present_flag(pps.lists_modification_present as u32);
        s.set_long_term_ref_pics_present_flag(sps.long_term_ref_pics_present as u32);
        s.set_sps_temporal_mvp_enabled_flag(sps.temporal_mvp_enabled as u32);
        s.set_cabac_init_present_flag(pps.cabac_init_present as u32);
        s.set_output_flag_present_flag(pps.output_flag_present as u32);
        s.set_dependent_slice_segments_enabled_flag(pps.dependent_slice_segments_enabled as u32);
        s.set_pps_slice_chroma_qp_offsets_present_flag(pps.slice_chroma_qp_offsets_present as u32);
        s.set_sample_adaptive_offset_enabled_flag(sps.sao_enabled as u32);
        s.set_deblocking_filter_override_enabled_flag(
            pps.deblocking_filter_override_enabled as u32,
        );
        s.set_pps_disable_deblocking_filter_flag(pps.deblocking_filter_disabled as u32);
        s.set_slice_segment_header_extension_present_flag(
            pps.slice_segment_header_extension_present as u32,
        );
        s.set_RapPicFlag(h.is_irap() as u32);
        s.set_IdrPicFlag(h.is_idr() as u32);
        s.set_IntraPicFlag(intra as u32);
    }
    pic
}

/// RefPicList0 (8.3.4) as indices into the picture parameters'
/// ReferenceFrames: the current-before pictures, then current-after,
/// repeated to fill the active list, then the stream's modification.
fn ref_list0(h: &SliceHeader, refs: &[Reference]) -> Result<Vec<u8>> {
    let mut temp: Vec<u8> = Vec::new();
    for flag in [
        va::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE,
        va::VA_PICTURE_HEVC_RPS_ST_CURR_AFTER,
    ] {
        // ReferenceFrames lists the RPS in s0 order (closest first), then s1.
        temp.extend(
            refs.iter()
                .enumerate()
                .filter(|(_, r)| r.flags == flag)
                .map(|(i, _)| i as u8),
        );
    }
    if temp.is_empty() {
        return Err(Error::Bitstream("P slice with no current reference"));
    }
    let active = h.num_ref_idx_l0_active_minus1 as usize + 1;
    let filled: Vec<u8> = temp
        .iter()
        .copied()
        .cycle()
        .take(active.max(temp.len()))
        .collect();
    Ok(match &h.list_entry_l0 {
        Some(entries) => entries
            .iter()
            .map(|&e| {
                filled
                    .get(e as usize)
                    .copied()
                    .ok_or(Error::Bitstream("list_entry_l0 out of range"))
            })
            .collect::<Result<_>>()?,
        None => filled.into_iter().take(active).collect(),
    })
}

fn slice_params(
    nal: &[u8],
    h: &SliceHeader,
    refs: &[Reference],
    last: bool,
) -> Result<va::VASliceParameterBufferHEVC> {
    // SAFETY: a plain C struct; every field accepts zero.
    let mut sl: va::VASliceParameterBufferHEVC = unsafe { std::mem::zeroed() };
    sl.slice_data_size = nal.len() as u32;
    sl.slice_data_offset = 0;
    sl.slice_data_flag = 0; // VA_SLICE_DATA_FLAG_ALL: the whole slice is here
    sl.slice_data_byte_offset = h.data_byte_offset as u32;
    sl.slice_segment_address = h.slice_segment_address;
    for list in sl.RefPicList.iter_mut() {
        list.fill(0xff);
    }
    if h.slice_type == SliceType::P {
        for (dst, src) in sl.RefPicList[0].iter_mut().zip(ref_list0(h, refs)?) {
            *dst = src;
        }
    }
    sl.collocated_ref_idx = if h.slice_temporal_mvp_enabled {
        h.collocated_ref_idx as u8
    } else {
        0xff
    };
    sl.num_ref_idx_l0_active_minus1 = h.num_ref_idx_l0_active_minus1 as u8;
    sl.num_ref_idx_l1_active_minus1 = h.num_ref_idx_l1_active_minus1 as u8;
    sl.slice_qp_delta = h.slice_qp_delta as i8;
    sl.slice_cb_qp_offset = h.slice_cb_qp_offset as i8;
    sl.slice_cr_qp_offset = h.slice_cr_qp_offset as i8;
    sl.slice_beta_offset_div2 = h.beta_offset_div2 as i8;
    sl.slice_tc_offset_div2 = h.tc_offset_div2 as i8;
    sl.five_minus_max_num_merge_cand = h.five_minus_max_num_merge_cand as u8;
    // SAFETY: writing bitfields of a zeroed union member.
    unsafe {
        let f = &mut sl.LongSliceFlags.fields;
        f.set_LastSliceOfPic(last as u32);
        f.set_dependent_slice_segment_flag(h.dependent_slice_segment as u32);
        f.set_slice_type(match h.slice_type {
            SliceType::B => 0,
            SliceType::P => 1,
            SliceType::I => 2,
        });
        f.set_slice_sao_luma_flag(h.sao_luma as u32);
        f.set_slice_sao_chroma_flag(h.sao_chroma as u32);
        f.set_mvd_l1_zero_flag(h.mvd_l1_zero as u32);
        f.set_cabac_init_flag(h.cabac_init as u32);
        f.set_slice_temporal_mvp_enabled_flag(h.slice_temporal_mvp_enabled as u32);
        f.set_slice_deblocking_filter_disabled_flag(h.deblocking_filter_disabled as u32);
        f.set_collocated_from_l0_flag(h.collocated_from_l0 as u32);
        f.set_slice_loop_filter_across_slices_enabled_flag(
            h.loop_filter_across_slices_enabled as u32,
        );
    }
    Ok(sl)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sps() -> Sps {
        Sps {
            log2_max_poc_lsb_minus4: 0, // max POC LSB 16
            ..Default::default()
        }
    }

    fn header(nal_type: u8, lsb: u32) -> SliceHeader {
        SliceHeader {
            nal_type,
            pic_order_cnt_lsb: lsb,
            ..Default::default()
        }
    }

    #[test]
    fn poc_counts_on_across_lsb_wraps() {
        let s = sps();
        assert_eq!(derive_poc(&s, &header(19, 0), 40), 0);
        assert_eq!(derive_poc(&s, &header(1, 1), 0), 1);
        // 15 then 0: the LSB wrapped forward.
        assert_eq!(derive_poc(&s, &header(1, 0), 15), 16);
        assert_eq!(derive_poc(&s, &header(1, 3), 33), 35);
    }

    #[test]
    fn list0_puts_before_then_after_and_cycles() {
        let r = |slot, poc, flags| Reference { slot, poc, flags };
        let refs = [
            r(0, 9, va::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE),
            r(1, 8, 0),
            r(2, 11, va::VA_PICTURE_HEVC_RPS_ST_CURR_AFTER),
        ];
        let mut h = header(1, 10);
        h.slice_type = SliceType::P;
        h.num_ref_idx_l0_active_minus1 = 2;
        assert_eq!(ref_list0(&h, &refs).unwrap(), vec![0, 2, 0]);
        h.list_entry_l0 = Some(vec![1, 0, 1]);
        assert_eq!(ref_list0(&h, &refs).unwrap(), vec![2, 0, 2]);
    }
}
