//! HEVC parameter sets and slice segment headers: a minimal writer for the
//! packed headers the encoder hands the driver, and a parser for what the
//! driver writes back, which the decoder needs.
//!
//! The parser reads the general Main profile syntax up to the fields VA-API
//! decode takes, and rejects the tools gliff never produces (long-term
//! references, weighted prediction, tiles, wavefronts, scaling lists in a
//! slice), so a stream it cannot describe fails loudly rather than decoding
//! to garbage.

use crate::h264::bits::{unescape_rbsp, BitReader};
use crate::h264::writer::{escape_rbsp, BitWriter};
use crate::hevc::nal::{PPS, SPS, VPS};
use crate::{Error, Result};

/// A short-term reference picture set (7.4.8), resolved to POC deltas.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StRps {
    /// Negative deltas, closest first, with their used_by_curr flags.
    pub s0: Vec<(i32, bool)>,
    /// Positive deltas, closest first.
    pub s1: Vec<(i32, bool)>,
}

impl StRps {
    pub fn num_delta_pocs(&self) -> usize {
        self.s0.len() + self.s1.len()
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sps {
    pub chroma_format_idc: u32,
    pub pic_width: u32,
    pub pic_height: u32,
    /// Left, right, top, bottom, in chroma sample units.
    pub conformance_window: [u32; 4],
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub log2_max_poc_lsb_minus4: u32,
    pub max_dec_pic_buffering_minus1: u32,
    pub log2_min_cb_minus3: u32,
    pub log2_diff_max_min_cb: u32,
    pub log2_min_tb_minus2: u32,
    pub log2_diff_max_min_tb: u32,
    pub max_transform_hierarchy_depth_inter: u32,
    pub max_transform_hierarchy_depth_intra: u32,
    pub scaling_list_enabled: bool,
    pub amp_enabled: bool,
    pub sao_enabled: bool,
    pub pcm_enabled: bool,
    pub pcm_bit_depth_luma_minus1: u32,
    pub pcm_bit_depth_chroma_minus1: u32,
    pub log2_min_pcm_cb_minus3: u32,
    pub log2_diff_max_min_pcm_cb: u32,
    pub pcm_loop_filter_disabled: bool,
    pub st_rps: Vec<StRps>,
    pub long_term_ref_pics_present: bool,
    pub temporal_mvp_enabled: bool,
    pub strong_intra_smoothing: bool,
}

impl Sps {
    pub fn log2_ctb_size(&self) -> u32 {
        self.log2_min_cb_minus3 + 3 + self.log2_diff_max_min_cb
    }

    pub fn ctb_size(&self) -> u32 {
        1 << self.log2_ctb_size()
    }

    /// Coding tree blocks in the picture.
    pub fn pic_size_in_ctbs(&self) -> u32 {
        let ctb = self.ctb_size();
        self.pic_width.div_ceil(ctb) * self.pic_height.div_ceil(ctb)
    }

    pub fn max_poc_lsb(&self) -> u32 {
        1 << (self.log2_max_poc_lsb_minus4 + 4)
    }

    /// The coded picture size.
    pub fn coded_size(&self) -> (u32, u32) {
        (self.pic_width, self.pic_height)
    }

    /// The picture after the conformance window crop.
    pub fn display_size(&self) -> (u32, u32) {
        let [l, r, t, b] = self.conformance_window;
        // 4:2:0: offsets count chroma samples, two luma samples each.
        (
            self.pic_width.saturating_sub(2 * (l + r)),
            self.pic_height.saturating_sub(2 * (t + b)),
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pps {
    pub dependent_slice_segments_enabled: bool,
    pub output_flag_present: bool,
    pub num_extra_slice_header_bits: u32,
    pub sign_data_hiding_enabled: bool,
    pub cabac_init_present: bool,
    pub num_ref_idx_l0_default_active_minus1: u32,
    pub num_ref_idx_l1_default_active_minus1: u32,
    pub init_qp_minus26: i32,
    pub constrained_intra_pred: bool,
    pub transform_skip_enabled: bool,
    pub cu_qp_delta_enabled: bool,
    pub diff_cu_qp_delta_depth: u32,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
    pub slice_chroma_qp_offsets_present: bool,
    pub weighted_pred: bool,
    pub weighted_bipred: bool,
    pub transquant_bypass_enabled: bool,
    pub tiles_enabled: bool,
    pub entropy_coding_sync_enabled: bool,
    pub loop_filter_across_slices_enabled: bool,
    pub deblocking_filter_override_enabled: bool,
    pub deblocking_filter_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub lists_modification_present: bool,
    pub log2_parallel_merge_level_minus2: u32,
    pub slice_segment_header_extension_present: bool,
}

/// HEVC slice_type (7.4.7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SliceType {
    B,
    P,
    #[default]
    I,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SliceHeader {
    pub nal_type: u8,
    pub first_slice_segment_in_pic: bool,
    pub dependent_slice_segment: bool,
    pub slice_segment_address: u32,
    pub slice_type: SliceType,
    pub pic_order_cnt_lsb: u32,
    /// The RPS in force: from the SPS or coded in the header.
    pub st_rps: StRps,
    /// Bits of the header's own st_ref_pic_set, 0 when it names an SPS one.
    pub st_rps_bits: u32,
    pub short_term_ref_pic_set_sps_flag: bool,
    pub slice_temporal_mvp_enabled: bool,
    pub sao_luma: bool,
    pub sao_chroma: bool,
    pub num_ref_idx_l0_active_minus1: u32,
    pub num_ref_idx_l1_active_minus1: u32,
    /// list_entry_l0 when the stream modifies list 0.
    pub list_entry_l0: Option<Vec<u32>>,
    pub mvd_l1_zero: bool,
    pub cabac_init: bool,
    pub collocated_from_l0: bool,
    pub collocated_ref_idx: u32,
    pub five_minus_max_num_merge_cand: u32,
    pub slice_qp_delta: i32,
    pub slice_cb_qp_offset: i32,
    pub slice_cr_qp_offset: i32,
    pub deblocking_filter_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub loop_filter_across_slices_enabled: bool,
    /// Bytes from the start of the NAL unit (header included, emulation
    /// prevention bytes counted) to the slice data.
    pub data_byte_offset: usize,
}

impl SliceHeader {
    pub fn is_idr(&self) -> bool {
        matches!(self.nal_type, 19 | 20)
    }

    pub fn is_irap(&self) -> bool {
        (16..=23).contains(&self.nal_type)
    }
}

// ---- Writing ---------------------------------------------------------------

/// What the encoder declares in its packed parameter sets. The driver reads
/// them and writes its own, adjusted to what the hardware codes.
#[derive(Debug, Clone)]
pub struct EncodeParams {
    pub width: u32,
    pub height: u32,
    pub level_idc: u8,
    pub log2_max_poc_lsb_minus4: u32,
    pub log2_min_cb_minus3: u32,
    pub log2_diff_max_min_cb: u32,
    pub log2_min_tb_minus2: u32,
    pub log2_diff_max_min_tb: u32,
    pub max_transform_hierarchy_depth: u32,
    pub amp_enabled: bool,
    pub init_qp: u32,
}

/// A NAL unit as a packed header: start code, header, and the RBSP left
/// unescaped. The driver adds emulation prevention when it writes it out.
fn framed(nal_type: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 1, nal_type << 1, 1];
    out.extend_from_slice(rbsp);
    out
}

/// Escape a packed (unescaped) NAL unit, start code included, into its
/// byte-stream form.
pub fn escape_nal(packed: &[u8]) -> Vec<u8> {
    let mut out = packed[..6].to_vec();
    out.extend(escape_rbsp(&packed[6..]));
    out
}

fn write_profile_tier_level(w: &mut BitWriter, level_idc: u8) {
    w.u(2, 0); // general_profile_space
    w.flag(false); // general_tier_flag
    w.u(5, 1); // general_profile_idc: Main
    w.u(32, 0x6000_0000); // compatible with Main and Main 10
    w.flag(true); // progressive_source
    w.flag(false); // interlaced_source
    w.flag(false); // non_packed_constraint
    w.flag(true); // frame_only_constraint
    w.u(32, 0);
    w.u(12, 0); // general_reserved_zero_43bits + general_inbld_flag
    w.u(8, level_idc as u32);
}

/// VPS, SPS and PPS as packed headers: start codes included, unescaped.
pub fn write_parameter_sets(p: &EncodeParams) -> [Vec<u8>; 3] {
    let mut vps = BitWriter::new();
    vps.u(4, 0); // vps_video_parameter_set_id
    vps.flag(true); // vps_base_layer_internal_flag
    vps.flag(true); // vps_base_layer_available_flag
    vps.u(6, 0); // vps_max_layers_minus1
    vps.u(3, 0); // vps_max_sub_layers_minus1
    vps.flag(true); // vps_temporal_id_nesting_flag
    vps.u(16, 0xffff);
    write_profile_tier_level(&mut vps, p.level_idc);
    vps.flag(true); // vps_sub_layer_ordering_info_present_flag
    vps.ue(1); // vps_max_dec_pic_buffering_minus1: one reference
    vps.ue(0); // vps_max_num_reorder_pics
    vps.ue(0); // vps_max_latency_increase_plus1
    vps.u(6, 0); // vps_max_layer_id
    vps.ue(0); // vps_num_layer_sets_minus1
    vps.flag(false); // vps_timing_info_present_flag
    vps.flag(false); // vps_extension_flag
    vps.trailing_bits();

    let mut sps = BitWriter::new();
    sps.u(4, 0); // sps_video_parameter_set_id
    sps.u(3, 0); // sps_max_sub_layers_minus1
    sps.flag(true); // sps_temporal_id_nesting_flag
    write_profile_tier_level(&mut sps, p.level_idc);
    sps.ue(0); // sps_seq_parameter_set_id
    sps.ue(1); // chroma_format_idc: 4:2:0
    let min_cb = 1 << (p.log2_min_cb_minus3 + 3);
    let (cw, ch) = (
        p.width.div_ceil(min_cb) * min_cb,
        p.height.div_ceil(min_cb) * min_cb,
    );
    sps.ue(cw);
    sps.ue(ch);
    let crop = cw != p.width || ch != p.height;
    sps.flag(crop);
    if crop {
        sps.ue(0);
        sps.ue((cw - p.width) / 2);
        sps.ue(0);
        sps.ue((ch - p.height) / 2);
    }
    sps.ue(0); // bit_depth_luma_minus8
    sps.ue(0); // bit_depth_chroma_minus8
    sps.ue(p.log2_max_poc_lsb_minus4);
    sps.flag(true); // sps_sub_layer_ordering_info_present_flag
    sps.ue(1); // sps_max_dec_pic_buffering_minus1
    sps.ue(0); // sps_max_num_reorder_pics
    sps.ue(0); // sps_max_latency_increase_plus1
    sps.ue(p.log2_min_cb_minus3);
    sps.ue(p.log2_diff_max_min_cb);
    sps.ue(p.log2_min_tb_minus2);
    sps.ue(p.log2_diff_max_min_tb);
    sps.ue(p.max_transform_hierarchy_depth); // inter
    sps.ue(p.max_transform_hierarchy_depth); // intra
    sps.flag(false); // scaling_list_enabled_flag
    sps.flag(p.amp_enabled);
    sps.flag(false); // sample_adaptive_offset_enabled_flag
    sps.flag(false); // pcm_enabled_flag
    sps.ue(0); // num_short_term_ref_pic_sets: each slice codes its own
    sps.flag(false); // long_term_ref_pics_present_flag
    sps.flag(false); // sps_temporal_mvp_enabled_flag
    sps.flag(false); // strong_intra_smoothing_enabled_flag
    sps.flag(false); // vui_parameters_present_flag
    sps.flag(false); // sps_extension_present_flag
    sps.trailing_bits();

    let mut pps = BitWriter::new();
    pps.ue(0); // pps_pic_parameter_set_id
    pps.ue(0); // pps_seq_parameter_set_id
    pps.flag(false); // dependent_slice_segments_enabled_flag
    pps.flag(false); // output_flag_present_flag
    pps.u(3, 0); // num_extra_slice_header_bits
    pps.flag(false); // sign_data_hiding_enabled_flag
    pps.flag(false); // cabac_init_present_flag
    pps.ue(0); // num_ref_idx_l0_default_active_minus1
    pps.ue(0); // num_ref_idx_l1_default_active_minus1
    pps.se(p.init_qp as i32 - 26);
    pps.flag(false); // constrained_intra_pred_flag
    pps.flag(false); // transform_skip_enabled_flag
    pps.flag(true); // cu_qp_delta_enabled_flag
    pps.ue(0); // diff_cu_qp_delta_depth
    pps.se(0); // pps_cb_qp_offset
    pps.se(0); // pps_cr_qp_offset
    pps.flag(false); // pps_slice_chroma_qp_offsets_present_flag
    pps.flag(false); // weighted_pred_flag
    pps.flag(false); // weighted_bipred_flag
    pps.flag(false); // transquant_bypass_enabled_flag
    pps.flag(false); // tiles_enabled_flag
    pps.flag(false); // entropy_coding_sync_enabled_flag
    pps.flag(true); // pps_loop_filter_across_slices_enabled_flag
    pps.flag(false); // deblocking_filter_control_present_flag
    pps.flag(false); // pps_scaling_list_data_present_flag
    pps.flag(false); // lists_modification_present_flag
    pps.ue(0); // log2_parallel_merge_level_minus2
    pps.flag(false); // slice_segment_header_extension_present_flag
    pps.flag(false); // pps_extension_present_flag
    pps.trailing_bits();

    [
        framed(VPS, &vps.into_bytes()),
        framed(SPS, &sps.into_bytes()),
        framed(PPS, &pps.into_bytes()),
    ]
}

/// The slice segment header of one whole-picture slice of gliff's streams,
/// as a packed header: an IDR, or a P picture predicted from the one before
/// it. The driver reads it for the slice parameters and writes its own.
pub fn write_slice_header(p: &EncodeParams, nal_type: u8, poc: i32) -> Vec<u8> {
    let idr = matches!(nal_type, 19 | 20);
    let mut w = BitWriter::new();
    w.flag(true); // first_slice_segment_in_pic_flag
    if (16..=23).contains(&nal_type) {
        w.flag(false); // no_output_of_prior_pics_flag
    }
    w.ue(0); // slice_pic_parameter_set_id
    w.ue(if idr { 2 } else { 1 }); // slice_type: I or P
    if !idr {
        let bits = p.log2_max_poc_lsb_minus4 + 4;
        w.u(bits, (poc as u32) & ((1 << bits) - 1));
        w.flag(false); // short_term_ref_pic_set_sps_flag
        w.ue(1); // num_negative_pics
        w.ue(0); // num_positive_pics
        w.ue(0); // delta_poc_s0_minus1: the picture before
        w.flag(true); // used_by_curr_pic_s0_flag
        w.flag(false); // num_ref_idx_active_override_flag
        w.ue(0); // five_minus_max_num_merge_cand
    }
    w.se(0); // slice_qp_delta
    w.flag(true); // slice_loop_filter_across_slices_enabled_flag
    w.trailing_bits(); // byte_alignment() has the same bits
    framed(nal_type, &w.into_bytes())
}

// ---- Parsing ---------------------------------------------------------------

fn skip_profile_tier_level(r: &mut BitReader, max_sub_layers_minus1: u32) -> Result<()> {
    r.bits(8)?; // profile space, tier, profile idc
    r.bits(32)?; // compatibility flags
    r.bits(32)?; // source flags and the first reserved bits
    r.bits(16)?; // the rest of the 48 constraint bits
    r.bits(8)?; // general_level_idc
    let mut profile_present = [false; 8];
    let mut level_present = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        profile_present[i] = r.bit()?;
        level_present[i] = r.bit()?;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.bits(2)?;
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if profile_present[i] {
            r.bits(32)?;
            r.bits(32)?;
            r.bits(24)?;
        }
        if level_present[i] {
            r.bits(8)?;
        }
    }
    Ok(())
}

/// st_ref_pic_set(idx) (7.3.7), resolved to POC deltas against the sets
/// already parsed (`earlier`), for the SPS (`idx < num`) or a slice
/// (`idx == num`).
fn parse_st_rps(r: &mut BitReader, idx: usize, earlier: &[StRps]) -> Result<StRps> {
    let num = earlier.len();
    let inter = idx != 0 && r.bit()?;
    if inter {
        let delta_idx = if idx == num { r.ue()? as usize + 1 } else { 1 };
        let ref_rps = idx
            .checked_sub(delta_idx)
            .and_then(|i| earlier.get(i))
            .ok_or(Error::Bitstream(
                "st_ref_pic_set predicts from a missing set",
            ))?;
        let sign = r.bit()?;
        let abs = r.ue()? as i32 + 1;
        let delta_rps = if sign { -abs } else { abs };
        let n = ref_rps.num_delta_pocs();
        let mut used = vec![false; n + 1];
        let mut use_delta = vec![true; n + 1];
        for j in 0..=n {
            used[j] = r.bit()?;
            if !used[j] {
                use_delta[j] = r.bit()?;
            }
        }
        // (7-61) and (7-62), with the reference set's deltas in the order the
        // equations walk them.
        let ref_s0: Vec<i32> = ref_rps.s0.iter().map(|p| p.0).collect();
        let ref_s1: Vec<i32> = ref_rps.s1.iter().map(|p| p.0).collect();
        let (n0, n1) = (ref_s0.len(), ref_s1.len());
        let mut s0 = Vec::new();
        for j in (0..n1).rev() {
            let d = ref_s1[j] + delta_rps;
            if d < 0 && use_delta[n0 + j] {
                s0.push((d, used[n0 + j]));
            }
        }
        if delta_rps < 0 && use_delta[n] {
            s0.push((delta_rps, used[n]));
        }
        for j in 0..n0 {
            let d = ref_s0[j] + delta_rps;
            if d < 0 && use_delta[j] {
                s0.push((d, used[j]));
            }
        }
        let mut s1 = Vec::new();
        for j in (0..n0).rev() {
            let d = ref_s0[j] + delta_rps;
            if d > 0 && use_delta[j] {
                s1.push((d, used[j]));
            }
        }
        if delta_rps > 0 && use_delta[n] {
            s1.push((delta_rps, used[n]));
        }
        for j in 0..n1 {
            let d = ref_s1[j] + delta_rps;
            if d > 0 && use_delta[n0 + j] {
                s1.push((d, used[n0 + j]));
            }
        }
        Ok(StRps { s0, s1 })
    } else {
        let num_negative = r.ue()?;
        let num_positive = r.ue()?;
        if num_negative > 16 || num_positive > 16 {
            return Err(Error::Bitstream("st_ref_pic_set too large"));
        }
        let mut s0 = Vec::new();
        let mut poc = 0;
        for _ in 0..num_negative {
            poc -= r.ue()? as i32 + 1;
            s0.push((poc, r.bit()?));
        }
        let mut s1 = Vec::new();
        poc = 0;
        for _ in 0..num_positive {
            poc += r.ue()? as i32 + 1;
            s1.push((poc, r.bit()?));
        }
        Ok(StRps { s0, s1 })
    }
}

/// The RBSP of a NAL unit: its body after the two-byte header, unescaped.
fn rbsp(nal: &[u8]) -> Result<Vec<u8>> {
    if nal.len() < 3 {
        return Err(Error::Bitstream("NAL unit too short"));
    }
    Ok(unescape_rbsp(&nal[2..]))
}

pub fn parse_sps(nal: &[u8]) -> Result<Sps> {
    let data = rbsp(nal)?;
    let mut r = BitReader::new(&data);
    r.bits(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.bits(3)?;
    r.bit()?; // sps_temporal_id_nesting_flag
    skip_profile_tier_level(&mut r, max_sub_layers_minus1)?;
    if r.ue()? != 0 {
        return Err(Error::Bitstream("only SPS 0 is supported"));
    }
    let mut s = Sps {
        chroma_format_idc: r.ue()?,
        ..Default::default()
    };
    if s.chroma_format_idc != 1 {
        return Err(Error::Bitstream("only 4:2:0 HEVC is supported"));
    }
    s.pic_width = r.ue()?;
    s.pic_height = r.ue()?;
    if r.bit()? {
        s.conformance_window = [r.ue()?, r.ue()?, r.ue()?, r.ue()?];
    }
    s.bit_depth_luma_minus8 = r.ue()?;
    s.bit_depth_chroma_minus8 = r.ue()?;
    if s.bit_depth_luma_minus8 != 0 || s.bit_depth_chroma_minus8 != 0 {
        return Err(Error::Bitstream("only 8-bit HEVC is supported"));
    }
    s.log2_max_poc_lsb_minus4 = r.ue()?;
    let ordering_all = r.bit()?;
    let first = if ordering_all {
        0
    } else {
        max_sub_layers_minus1
    };
    for _ in first..=max_sub_layers_minus1 {
        // The highest sub-layer's values are the ones that apply.
        s.max_dec_pic_buffering_minus1 = r.ue()?;
        r.ue()?; // sps_max_num_reorder_pics
        r.ue()?; // sps_max_latency_increase_plus1
    }
    s.log2_min_cb_minus3 = r.ue()?;
    s.log2_diff_max_min_cb = r.ue()?;
    s.log2_min_tb_minus2 = r.ue()?;
    s.log2_diff_max_min_tb = r.ue()?;
    s.max_transform_hierarchy_depth_inter = r.ue()?;
    s.max_transform_hierarchy_depth_intra = r.ue()?;
    s.scaling_list_enabled = r.bit()?;
    if s.scaling_list_enabled && r.bit()? {
        return Err(Error::Bitstream("SPS scaling lists are not supported"));
    }
    s.amp_enabled = r.bit()?;
    s.sao_enabled = r.bit()?;
    s.pcm_enabled = r.bit()?;
    if s.pcm_enabled {
        s.pcm_bit_depth_luma_minus1 = r.bits(4)?;
        s.pcm_bit_depth_chroma_minus1 = r.bits(4)?;
        s.log2_min_pcm_cb_minus3 = r.ue()?;
        s.log2_diff_max_min_pcm_cb = r.ue()?;
        s.pcm_loop_filter_disabled = r.bit()?;
    }
    let num_sets = r.ue()? as usize;
    if num_sets > 64 {
        return Err(Error::Bitstream("too many short-term RPS"));
    }
    for i in 0..num_sets {
        let set = parse_st_rps(&mut r, i, &s.st_rps)?;
        s.st_rps.push(set);
    }
    s.long_term_ref_pics_present = r.bit()?;
    if s.long_term_ref_pics_present {
        return Err(Error::Bitstream("long-term references are not supported"));
    }
    s.temporal_mvp_enabled = r.bit()?;
    s.strong_intra_smoothing = r.bit()?;
    // The VUI and extensions carry nothing the decoder needs.
    Ok(s)
}

pub fn parse_pps(nal: &[u8]) -> Result<Pps> {
    let data = rbsp(nal)?;
    let mut r = BitReader::new(&data);
    if r.ue()? != 0 || r.ue()? != 0 {
        return Err(Error::Bitstream("only PPS 0 on SPS 0 is supported"));
    }
    let mut p = Pps {
        dependent_slice_segments_enabled: r.bit()?,
        output_flag_present: r.bit()?,
        num_extra_slice_header_bits: r.bits(3)?,
        sign_data_hiding_enabled: r.bit()?,
        cabac_init_present: r.bit()?,
        num_ref_idx_l0_default_active_minus1: r.ue()?,
        num_ref_idx_l1_default_active_minus1: r.ue()?,
        init_qp_minus26: r.se()?,
        constrained_intra_pred: r.bit()?,
        transform_skip_enabled: r.bit()?,
        cu_qp_delta_enabled: r.bit()?,
        ..Default::default()
    };
    if p.cu_qp_delta_enabled {
        p.diff_cu_qp_delta_depth = r.ue()?;
    }
    p.cb_qp_offset = r.se()?;
    p.cr_qp_offset = r.se()?;
    p.slice_chroma_qp_offsets_present = r.bit()?;
    p.weighted_pred = r.bit()?;
    p.weighted_bipred = r.bit()?;
    p.transquant_bypass_enabled = r.bit()?;
    p.tiles_enabled = r.bit()?;
    p.entropy_coding_sync_enabled = r.bit()?;
    if p.tiles_enabled || p.entropy_coding_sync_enabled {
        return Err(Error::Bitstream(
            "HEVC tiles and wavefronts are not supported",
        ));
    }
    if p.weighted_pred || p.weighted_bipred {
        return Err(Error::Bitstream(
            "HEVC weighted prediction is not supported",
        ));
    }
    p.loop_filter_across_slices_enabled = r.bit()?;
    if r.bit()? {
        // deblocking_filter_control_present_flag
        p.deblocking_filter_override_enabled = r.bit()?;
        p.deblocking_filter_disabled = r.bit()?;
        if !p.deblocking_filter_disabled {
            p.beta_offset_div2 = r.se()?;
            p.tc_offset_div2 = r.se()?;
        }
    }
    if r.bit()? {
        return Err(Error::Bitstream("PPS scaling lists are not supported"));
    }
    p.lists_modification_present = r.bit()?;
    p.log2_parallel_merge_level_minus2 = r.ue()?;
    p.slice_segment_header_extension_present = r.bit()?;
    Ok(p)
}

/// The number of bits needed for values below `n` (Ceil(Log2(n))).
fn ceil_log2(n: u32) -> u32 {
    if n <= 1 {
        0
    } else {
        32 - (n - 1).leading_zeros()
    }
}

/// Bytes of `nal` (escaped) that hold the first `rbsp_bytes` bytes of its
/// unescaped body, counted from the start of the NAL unit.
fn escaped_offset(nal: &[u8], rbsp_bytes: usize) -> usize {
    let mut zeros = 0;
    let mut taken = 0;
    let mut i = 2;
    while i < nal.len() && taken < rbsp_bytes {
        let b = nal[i];
        if zeros >= 2 && b == 3 {
            zeros = 0;
            i += 1;
            continue;
        }
        taken += 1;
        zeros = if b == 0 { zeros + 1 } else { 0 };
        i += 1;
    }
    // An emulation prevention byte right after the header belongs to it.
    if zeros >= 2 && nal.get(i) == Some(&3) {
        i += 1;
    }
    i
}

/// A slice segment header (7.3.6.1). `prev` is the header of the
/// independent segment a dependent one continues.
pub fn parse_slice_header(
    nal: &[u8],
    sps: &Sps,
    pps: &Pps,
    prev: Option<&SliceHeader>,
) -> Result<SliceHeader> {
    let nal_type = (nal[0] >> 1) & 0x3f;
    let data = rbsp(nal)?;
    let mut r = BitReader::new(&data);
    let mut h = SliceHeader {
        nal_type,
        first_slice_segment_in_pic: r.bit()?,
        ..Default::default()
    };
    if h.is_irap() {
        r.bit()?; // no_output_of_prior_pics_flag
    }
    if r.ue()? != 0 {
        return Err(Error::Bitstream("only PPS 0 is supported"));
    }
    if !h.first_slice_segment_in_pic {
        if pps.dependent_slice_segments_enabled {
            h.dependent_slice_segment = r.bit()?;
        }
        h.slice_segment_address = r.bits(ceil_log2(sps.pic_size_in_ctbs()))?;
    }
    if h.dependent_slice_segment {
        let prev = prev.ok_or(Error::Bitstream("dependent slice segment comes first"))?;
        let mut d = SliceHeader {
            nal_type,
            first_slice_segment_in_pic: false,
            dependent_slice_segment: true,
            slice_segment_address: h.slice_segment_address,
            ..prev.clone()
        };
        d.data_byte_offset = finish_header(&mut r, nal, pps)?;
        return Ok(d);
    }
    for _ in 0..pps.num_extra_slice_header_bits {
        r.bit()?;
    }
    h.slice_type = match r.ue()? {
        0 => SliceType::B,
        1 => SliceType::P,
        2 => SliceType::I,
        _ => return Err(Error::Bitstream("bad slice_type")),
    };
    if h.slice_type == SliceType::B {
        return Err(Error::Bitstream("B slices are not supported"));
    }
    if pps.output_flag_present {
        r.bit()?; // pic_output_flag
    }
    if !h.is_idr() {
        h.pic_order_cnt_lsb = r.bits(sps.log2_max_poc_lsb_minus4 + 4)?;
        h.short_term_ref_pic_set_sps_flag = r.bit()?;
        if !h.short_term_ref_pic_set_sps_flag {
            let start = r.position();
            h.st_rps = parse_st_rps(&mut r, sps.st_rps.len(), &sps.st_rps)?;
            h.st_rps_bits = (r.position() - start) as u32;
        } else {
            let idx = if sps.st_rps.len() > 1 {
                r.bits(ceil_log2(sps.st_rps.len() as u32))? as usize
            } else {
                0
            };
            h.st_rps = sps
                .st_rps
                .get(idx)
                .cloned()
                .ok_or(Error::Bitstream("slice names a missing RPS"))?;
        }
        if sps.temporal_mvp_enabled {
            h.slice_temporal_mvp_enabled = r.bit()?;
        }
    }
    if sps.sao_enabled {
        h.sao_luma = r.bit()?;
        h.sao_chroma = r.bit()?;
    }
    h.num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
    h.num_ref_idx_l1_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
    if h.slice_type == SliceType::P {
        if r.bit()? {
            // num_ref_idx_active_override_flag
            h.num_ref_idx_l0_active_minus1 = r.ue()?;
        }
        let num_pic_total_curr = h
            .st_rps
            .s0
            .iter()
            .chain(&h.st_rps.s1)
            .filter(|p| p.1)
            .count();
        if pps.lists_modification_present && num_pic_total_curr > 1 && r.bit()? {
            let bits = ceil_log2(num_pic_total_curr as u32);
            let mut entries = Vec::new();
            for _ in 0..=h.num_ref_idx_l0_active_minus1 {
                entries.push(r.bits(bits)?);
            }
            h.list_entry_l0 = Some(entries);
        }
        if pps.cabac_init_present {
            h.cabac_init = r.bit()?;
        }
        h.collocated_from_l0 = true;
        if h.slice_temporal_mvp_enabled && h.num_ref_idx_l0_active_minus1 > 0 {
            h.collocated_ref_idx = r.ue()?;
        }
        h.five_minus_max_num_merge_cand = r.ue()?;
    }
    h.slice_qp_delta = r.se()?;
    if pps.slice_chroma_qp_offsets_present {
        h.slice_cb_qp_offset = r.se()?;
        h.slice_cr_qp_offset = r.se()?;
    }
    let override_flag = pps.deblocking_filter_override_enabled && r.bit()?;
    h.deblocking_filter_disabled = pps.deblocking_filter_disabled;
    h.beta_offset_div2 = pps.beta_offset_div2;
    h.tc_offset_div2 = pps.tc_offset_div2;
    if override_flag {
        h.deblocking_filter_disabled = r.bit()?;
        if !h.deblocking_filter_disabled {
            h.beta_offset_div2 = r.se()?;
            h.tc_offset_div2 = r.se()?;
        }
    }
    h.loop_filter_across_slices_enabled = pps.loop_filter_across_slices_enabled;
    if pps.loop_filter_across_slices_enabled
        && (h.sao_luma || h.sao_chroma || !h.deblocking_filter_disabled)
    {
        h.loop_filter_across_slices_enabled = r.bit()?;
    }
    h.data_byte_offset = finish_header(&mut r, nal, pps)?;
    Ok(h)
}

/// The tail every slice segment header shares: entry points (none without
/// tiles or wavefronts), the header extension, and the byte alignment.
/// Returns the escaped byte offset of the slice data.
fn finish_header(r: &mut BitReader, nal: &[u8], pps: &Pps) -> Result<usize> {
    if pps.slice_segment_header_extension_present {
        let len = r.ue()?;
        for _ in 0..len {
            r.bits(8)?;
        }
    }
    // byte_alignment(): a one bit, then zeros to the byte boundary.
    if !r.bit()? {
        return Err(Error::Bitstream("slice header alignment bit missing"));
    }
    while !r.position().is_multiple_of(8) {
        r.bit()?;
    }
    Ok(escaped_offset(nal, r.position() / 8))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hevc::nal::nal_units;

    fn params() -> EncodeParams {
        EncodeParams {
            width: 6016,
            height: 3384,
            level_idc: 183,
            log2_max_poc_lsb_minus4: 12,
            log2_min_cb_minus3: 0,
            log2_diff_max_min_cb: 3,
            log2_min_tb_minus2: 0,
            log2_diff_max_min_tb: 3,
            max_transform_hierarchy_depth: 3,
            amp_enabled: true,
            init_qp: 26,
        }
    }

    #[test]
    fn written_parameter_sets_parse_back() {
        let [vps, sps, pps] = write_parameter_sets(&params()).map(|n| escape_nal(&n));
        let units: Vec<_> = [&vps, &sps, &pps]
            .iter()
            .flat_map(|n| nal_units(n))
            .map(|n| n.nal_type)
            .collect();
        assert_eq!(units, vec![VPS, SPS, PPS]);
        let s = parse_sps(nal_units(&sps)[0].data).unwrap();
        assert_eq!(s.coded_size(), (6016, 3384));
        assert_eq!(s.display_size(), (6016, 3384));
        assert_eq!(s.ctb_size(), 64);
        assert_eq!(s.pic_size_in_ctbs(), 94 * 53);
        assert_eq!(s.max_poc_lsb(), 1 << 16);
        assert!(s.amp_enabled && !s.sao_enabled && !s.temporal_mvp_enabled);
        let p = parse_pps(nal_units(&pps)[0].data).unwrap();
        assert!(p.cu_qp_delta_enabled && p.loop_filter_across_slices_enabled);
        assert_eq!(p.init_qp_minus26, 0);
    }

    #[test]
    fn crops_an_odd_size_with_a_conformance_window() {
        let p = EncodeParams {
            width: 1366,
            height: 766,
            ..params()
        };
        let [_, sps, _] = write_parameter_sets(&p).map(|n| escape_nal(&n));
        let s = parse_sps(nal_units(&sps)[0].data).unwrap();
        assert_eq!(s.coded_size(), (1368, 768));
        assert_eq!(s.display_size(), (1366, 766));
    }

    /// Write a P slice header the way Mesa's VCN encoder does for gliff's
    /// streams (one reference, the RPS coded in the slice) and parse it.
    #[test]
    fn parses_a_p_slice_header_with_its_own_rps() {
        let [_, sps_nal, pps_nal] = write_parameter_sets(&params()).map(|n| escape_nal(&n));
        let sps = parse_sps(nal_units(&sps_nal)[0].data).unwrap();
        let pps = parse_pps(nal_units(&pps_nal)[0].data).unwrap();
        let mut w = BitWriter::new();
        w.flag(true); // first_slice_segment_in_pic_flag
        w.ue(0); // slice_pic_parameter_set_id
        w.ue(1); // slice_type P
        w.u(16, 7); // slice_pic_order_cnt_lsb
        w.flag(false); // short_term_ref_pic_set_sps_flag
        w.ue(1); // num_negative_pics
        w.ue(0); // num_positive_pics
        w.ue(0); // delta_poc_s0_minus1
        w.flag(true); // used_by_curr_pic_s0_flag
        w.flag(false); // num_ref_idx_active_override_flag
        w.ue(0); // five_minus_max_num_merge_cand
        w.se(-2); // slice_qp_delta
        w.flag(true); // slice_loop_filter_across_slices_enabled_flag
        w.trailing_bits(); // byte_alignment has the same shape
        let mut nal = vec![TRAIL_R_HEADER, 1];
        nal.extend(escape_rbsp(&w.into_bytes()));
        let h = parse_slice_header(&nal, &sps, &pps, None).unwrap();
        assert_eq!(h.slice_type, SliceType::P);
        assert_eq!(h.pic_order_cnt_lsb, 7);
        assert_eq!(h.st_rps.s0, vec![(-1, true)]);
        assert!(h.st_rps.s1.is_empty());
        // ue(1), ue(0), ue(0), then the used flag.
        assert_eq!(h.st_rps_bits, 3 + 1 + 1 + 1);
        assert_eq!(h.slice_qp_delta, -2);
        assert_eq!(h.five_minus_max_num_merge_cand, 0);
        assert_eq!(h.data_byte_offset, nal.len());
    }

    const TRAIL_R_HEADER: u8 = 1 << 1;

    #[test]
    fn written_slice_headers_parse_back() {
        let p = params();
        let [_, sps_nal, pps_nal] = write_parameter_sets(&p).map(|n| escape_nal(&n));
        let sps = parse_sps(nal_units(&sps_nal)[0].data).unwrap();
        let pps = parse_pps(nal_units(&pps_nal)[0].data).unwrap();
        let idr = escape_nal(&write_slice_header(&p, 19, 0));
        let h = parse_slice_header(nal_units(&idr)[0].data, &sps, &pps, None).unwrap();
        assert!(h.is_idr());
        assert_eq!(h.slice_type, SliceType::I);
        let p_nal = escape_nal(&write_slice_header(&p, 1, 70_000));
        let h = parse_slice_header(nal_units(&p_nal)[0].data, &sps, &pps, None).unwrap();
        assert_eq!(h.slice_type, SliceType::P);
        assert_eq!(h.pic_order_cnt_lsb, 70_000 % 65_536);
        assert_eq!(h.st_rps.s0, vec![(-1, true)]);
        assert!(h.loop_filter_across_slices_enabled);
    }

    #[test]
    fn inter_rps_prediction_resolves_deltas() {
        // Set 0: one picture back. Set 1 predicted from it with deltaRps -1,
        // keeping the reference and adding the predicting picture: two back
        // and one back.
        let mut w = BitWriter::new();
        w.ue(1);
        w.ue(0);
        w.ue(0);
        w.flag(true);
        w.flag(true); // inter_ref_pic_set_prediction_flag
        w.flag(true); // delta_rps_sign: negative
        w.ue(0); // abs_delta_rps_minus1: deltaRps = -1
        w.flag(true); // used_by_curr_pic_flag[0]: the -1 picture becomes -2
        w.flag(true); // used_by_curr_pic_flag[1]: deltaRps itself, -1
        w.trailing_bits();
        let data = w.into_bytes();
        let mut r = BitReader::new(&data);
        let first = parse_st_rps(&mut r, 0, &[]).unwrap();
        let second = parse_st_rps(&mut r, 1, std::slice::from_ref(&first)).unwrap();
        assert_eq!(first.s0, vec![(-1, true)]);
        assert_eq!(second.s0, vec![(-1, true), (-2, true)]);
    }

    #[test]
    fn escaped_offset_counts_emulation_prevention() {
        // Header, then RBSP 00 00 01 written as 00 00 03 01.
        let nal = [2, 1, 0, 0, 3, 1, 0x80];
        assert_eq!(escaped_offset(&nal, 0), 2);
        assert_eq!(escaped_offset(&nal, 2), 5);
        assert_eq!(escaped_offset(&nal, 3), 6);
    }

    #[test]
    fn ceil_log2_matches_the_spec() {
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(5), 3);
        assert_eq!(ceil_log2(4982), 13);
    }
}
