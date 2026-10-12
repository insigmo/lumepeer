//! Telling a decoder that an H.264 stream never reorders (ADR 0154).
//!
//! A decoder may hand out a picture only once it knows no later one is to be
//! shown before it. A sequence parameter set says so in its VUI
//! (`bitstream_restriction_flag` with `max_num_reorder_frames` 0); one that
//! does not leaves the decoder assuming the worst, and it holds every picture
//! until its picture buffer fills. Every stream this host makes is free of
//! reordering (no B-frames anywhere), but not every encoder says so:
//! VideoToolbox writes a VUI with timing and HRD and no restriction. A guest
//! then showed a Mac's picture several frames late, and on a still screen the
//! last change — or, after a new screen size, the whole new picture — not at
//! all, until something else moved. `linux_vaapi` writes its own SPS and says
//! it there; this does the same for a parameter set somebody else wrote.

/// `nal_unit_type` of a sequence parameter set.
const NAL_SPS: u8 = 7;

/// `profile_idc` values whose SPS carries `chroma_format_idc` and the rest of
/// the high-profile fields (H.264 7.3.2.1.1).
const HIGH_PROFILES: [u32; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// `log2_max_mv_length_*` at its maximum: no promise about motion vectors,
/// as in `linux_vaapi`'s own VUI.
const LOG2_MAX_MV_LENGTH_UNRESTRICTED: u32 = 16;

/// `annexb` with every SPS saying no picture is reordered, or `None` when
/// there was nothing to change: no SPS, one that already says it (or says
/// the opposite, which is the encoder's to know), or one this cannot read.
#[must_use]
pub fn without_reordering(annexb: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(annexb.len() + 8);
    let mut changed = false;
    let mut copied = 0;
    for (start, end) in nal_units(annexb) {
        let nal = &annexb[start..end];
        let Some(rewritten) = nal
            .first()
            .filter(|header| *header & 0x1f == NAL_SPS)
            .and_then(|_| sps_without_reordering(nal))
        else {
            continue;
        };
        out.extend_from_slice(&annexb[copied..start]);
        out.extend_from_slice(&rewritten);
        copied = end;
        changed = true;
    }
    if !changed {
        return None;
    }
    out.extend_from_slice(&annexb[copied..]);
    Some(out)
}

/// The NAL units of an Annex-B stream, as byte ranges after each start code
/// and before the next one (trailing zero bytes left with the next start
/// code, where they belong).
fn nal_units(stream: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut at = 0;
    while at + 3 <= stream.len() {
        if stream[at] == 0 && stream[at + 1] == 0 && stream[at + 2] == 1 {
            starts.push((at, at + 3));
            at += 3;
        } else {
            at += 1;
        }
    }
    let mut units = Vec::with_capacity(starts.len());
    for (i, &(_, payload)) in starts.iter().enumerate() {
        let mut end = starts.get(i + 1).map_or(stream.len(), |&(code, _)| code);
        // A four-byte start code's leading zero belongs to it, not to the
        // unit before.
        while end > payload && stream[end - 1] == 0 && i + 1 < starts.len() {
            end -= 1;
        }
        units.push((payload, end));
    }
    units
}

/// One SPS NAL unit (header byte included, start code not) rewritten to say
/// no picture is reordered, or `None` when it needs no change or cannot be
/// read.
fn sps_without_reordering(nal: &[u8]) -> Option<Vec<u8>> {
    let (&header, payload) = nal.split_first()?;
    let rbsp = unescape(payload);
    let mut reader = BitReader::new(&rbsp);
    let layout = read_sps(&mut reader)?;
    let mut writer = BitWriter::default();
    match layout.restriction {
        // Said already, either way: the encoder knows its own stream.
        Restriction::Present => return None,
        Restriction::Absent { flag_at } => {
            writer.copy(&rbsp, flag_at);
            writer.flag(true);
        }
        Restriction::NoVui { flag_at } => {
            writer.copy(&rbsp, flag_at);
            writer.flag(true); // vui_parameters_present_flag
            writer.flag(false); // aspect_ratio_info_present_flag
            writer.flag(false); // overscan_info_present_flag
            writer.flag(false); // video_signal_type_present_flag
            writer.flag(false); // chroma_loc_info_present_flag
            writer.flag(false); // timing_info_present_flag
            writer.flag(false); // nal_hrd_parameters_present_flag
            writer.flag(false); // vcl_hrd_parameters_present_flag
            writer.flag(false); // pic_struct_present_flag
            writer.flag(true); // bitstream_restriction_flag
        }
    }
    writer.flag(true); // motion_vectors_over_pic_boundaries_flag
    writer.ue(0); // max_bytes_per_pic_denom: no limit
    writer.ue(0); // max_bits_per_mb_denom: no limit
    writer.ue(LOG2_MAX_MV_LENGTH_UNRESTRICTED);
    writer.ue(LOG2_MAX_MV_LENGTH_UNRESTRICTED);
    writer.ue(0); // max_num_reorder_frames
    writer.ue(layout.max_num_ref_frames.max(1)); // max_dec_frame_buffering
    let mut nal = vec![header];
    nal.extend(escape(&writer.finish()));
    Some(nal)
}

/// Where an SPS's bitstream restriction is, or why there is none to add.
enum Restriction {
    /// The VUI has one already.
    Present,
    /// The VUI ends without one; `flag_at` is the bit of its flag.
    Absent { flag_at: usize },
    /// There is no VUI at all; `flag_at` is the bit of its presence flag.
    NoVui { flag_at: usize },
}

/// What a rewrite needs out of an SPS.
struct SpsLayout {
    max_num_ref_frames: u32,
    restriction: Restriction,
}

/// Reads an SPS RBSP (H.264 7.3.2.1.1, E.1.1) as far as the bitstream
/// restriction, or `None` for one this does not understand.
fn read_sps(r: &mut BitReader<'_>) -> Option<SpsLayout> {
    let profile_idc = r.bits(8)?;
    r.bits(16)?; // constraint flags, reserved bits, level_idc
    r.ue()?; // seq_parameter_set_id
    if HIGH_PROFILES.contains(&profile_idc) {
        let chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.flag()?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.flag()?; // qpprime_y_zero_transform_bypass_flag
        if r.flag()? {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for list in 0..lists {
                if r.flag()? {
                    skip_scaling_list(r, if list < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.flag()?; // delta_pic_order_always_zero_flag
            r.se()?; // offset_for_non_ref_pic
            r.se()?; // offset_for_top_to_bottom_field
            for _ in 0..r.ue()? {
                r.se()?; // offset_for_ref_frame
            }
        }
        _ => {}
    }
    let max_num_ref_frames = r.ue()?;
    r.flag()?; // gaps_in_frame_num_value_allowed_flag
    r.ue()?; // pic_width_in_mbs_minus1
    r.ue()?; // pic_height_in_map_units_minus1
    if !r.flag()? {
        r.flag()?; // mb_adaptive_frame_field_flag
    }
    r.flag()?; // direct_8x8_inference_flag
    if r.flag()? {
        for _ in 0..4 {
            r.ue()?; // frame_crop_*_offset
        }
    }
    let vui_at = r.position();
    if !r.flag()? {
        return Some(SpsLayout {
            max_num_ref_frames,
            restriction: Restriction::NoVui { flag_at: vui_at },
        });
    }
    if r.flag()? && r.bits(8)? == 255 {
        r.bits(32)?; // sar_width, sar_height
    }
    if r.flag()? {
        r.flag()?; // overscan_appropriate_flag
    }
    if r.flag()? {
        r.bits(4)?; // video_format, video_full_range_flag
        if r.flag()? {
            r.bits(24)?; // colour_primaries, transfer, matrix
        }
    }
    if r.flag()? {
        r.ue()?; // chroma_sample_loc_type_top_field
        r.ue()?; // chroma_sample_loc_type_bottom_field
    }
    if r.flag()? {
        r.bits(32)?; // num_units_in_tick
        r.bits(32)?; // time_scale
        r.flag()?; // fixed_frame_rate_flag
    }
    let nal_hrd = r.flag()?;
    if nal_hrd {
        skip_hrd(r)?;
    }
    let vcl_hrd = r.flag()?;
    if vcl_hrd {
        skip_hrd(r)?;
    }
    if nal_hrd || vcl_hrd {
        r.flag()?; // low_delay_hrd_flag
    }
    r.flag()?; // pic_struct_present_flag
    let flag_at = r.position();
    let restriction = if r.flag()? {
        Restriction::Present
    } else {
        Restriction::Absent { flag_at }
    };
    Some(SpsLayout {
        max_num_ref_frames,
        restriction,
    })
}

/// Skips one `scaling_list` of `size` entries (H.264 7.3.2.1.1.1).
fn skip_scaling_list(r: &mut BitReader<'_>, size: usize) -> Option<()> {
    let (mut last, mut next) = (8_i64, 8_i64);
    for _ in 0..size {
        if next != 0 {
            next = (last + i64::from(r.se()?) + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// Skips `hrd_parameters` (H.264 E.1.2).
fn skip_hrd(r: &mut BitReader<'_>) -> Option<()> {
    let count = r.ue()?.checked_add(1)?;
    r.bits(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..count {
        r.ue()?; // bit_rate_value_minus1
        r.ue()?; // cpb_size_value_minus1
        r.flag()?; // cbr_flag
    }
    r.bits(20)?; // four delay and length fields
    Some(())
}

/// A NAL payload with its emulation prevention bytes taken out.
fn unescape(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0;
    for &byte in payload {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// An RBSP with emulation prevention bytes put in, ready to be a NAL payload.
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 4);
    let mut zeros = 0;
    for &byte in rbsp {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// Reads bits, most significant first.
struct BitReader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> BitReader<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    const fn position(&self) -> usize {
        self.at
    }

    fn flag(&mut self) -> Option<bool> {
        let byte = self.data.get(self.at / 8)?;
        let bit = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Some(bit == 1)
    }

    fn bits(&mut self, count: u32) -> Option<u32> {
        let mut value = 0_u32;
        for _ in 0..count {
            value = (value << 1) | u32::from(self.flag()?);
        }
        Some(value)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)?;
        (1_u32 << zeros).checked_sub(1)?.checked_add(rest)
    }

    fn se(&mut self) -> Option<i32> {
        let code = i64::from(self.ue()?);
        let value = if code % 2 == 1 {
            (code + 1) / 2
        } else {
            -(code / 2)
        };
        i32::try_from(value).ok()
    }
}

/// Writes bits, most significant first.
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    used: usize,
}

impl BitWriter {
    fn flag(&mut self, bit: bool) {
        if self.used.is_multiple_of(8) {
            self.bytes.push(0);
        }
        if bit && let Some(last) = self.bytes.last_mut() {
            *last |= 0x80 >> (self.used % 8);
        }
        self.used += 1;
    }

    /// Only the tests write fixed-width fields: a rewrite copies those.
    #[cfg(test)]
    fn bits(&mut self, value: u32, count: u32) {
        for shift in (0..count).rev() {
            self.flag((value >> shift) & 1 == 1);
        }
    }

    fn ue(&mut self, value: u32) {
        let coded = u64::from(value) + 1;
        let length = 64 - coded.leading_zeros();
        for _ in 1..length {
            self.flag(false);
        }
        for shift in (0..length).rev() {
            self.flag((coded >> shift) & 1 == 1);
        }
    }

    /// The first `count` bits of `data`, as they are.
    fn copy(&mut self, data: &[u8], count: usize) {
        for at in 0..count {
            let byte = data.get(at / 8).copied().unwrap_or(0);
            self.flag((byte >> (7 - at % 8)) & 1 == 1);
        }
    }

    /// The bits written, closed with `rbsp_trailing_bits`.
    fn finish(mut self) -> Vec<u8> {
        self.flag(true);
        while !self.used.is_multiple_of(8) {
            self.flag(false);
        }
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
            .collect()
    }

    /// What a decoder reads out of the first SPS in `stream`:
    /// `max_num_ref_frames`, and `max_num_reorder_frames` /
    /// `max_dec_frame_buffering` when the VUI says.
    fn reorder_of(stream: &[u8]) -> (u32, Option<(u32, u32)>) {
        let (start, end) = nal_units(stream)
            .into_iter()
            .find(|&(start, _)| stream[start] & 0x1f == NAL_SPS)
            .unwrap();
        let rbsp = unescape(&stream[start + 1..end]);
        let mut r = BitReader::new(&rbsp);
        let layout = read_sps(&mut r).unwrap();
        let said = match layout.restriction {
            Restriction::Present => {
                r.flag().unwrap(); // motion_vectors_over_pic_boundaries_flag
                for _ in 0..4 {
                    r.ue().unwrap();
                }
                Some((r.ue().unwrap(), r.ue().unwrap()))
            }
            _ => None,
        };
        (layout.max_num_ref_frames, said)
    }

    /// A keyframe as VideoToolbox wrote it on the Mac in the e2e matrix
    /// (Main, level 3.0, two reference frames, a VUI with HRD and no
    /// restriction), followed by its PPS and the first bytes of a slice.
    fn videotoolbox_keyframe() -> Vec<u8> {
        let mut stream = vec![0, 0, 0, 1];
        stream.extend(hex("274d401ea9181806bf2602d4080808c2b5ef7c04"));
        stream.extend([0, 0, 0, 1]);
        stream.extend(hex("28fe09c8"));
        stream.extend([0, 0, 1, 0x25, 0x88, 0x84, 0x00, 0x33]);
        stream
    }

    /// ADR 0154: VideoToolbox's SPS gains a restriction saying nothing is
    /// reordered, and everything else in the stream stays byte for byte.
    #[test]
    fn a_videotoolbox_sps_says_nothing_is_reordered() {
        let stream = videotoolbox_keyframe();
        assert_eq!(reorder_of(&stream), (2, None));
        let fixed = without_reordering(&stream).unwrap();
        assert_eq!(reorder_of(&fixed), (2, Some((0, 2))));
        let tail = &stream[stream.len() - 16..];
        assert!(fixed.ends_with(tail), "the PPS and the slice changed");
        assert!(fixed.starts_with(&[0, 0, 0, 1, 0x27, 0x4d, 0x40, 0x1e]));
        // Said once is enough.
        assert!(without_reordering(&fixed).is_none());
    }

    /// An SPS that has a restriction already — what the Windows encoder
    /// writes — is left exactly as it is, and so is a stream with no SPS.
    #[test]
    fn a_stream_that_says_it_already_or_has_no_sps_is_left_alone() {
        let stream = videotoolbox_keyframe();
        let fixed = without_reordering(&stream).unwrap();
        assert!(without_reordering(&fixed).is_none());
        assert!(without_reordering(&[0, 0, 0, 1, 0x41, 0x9a, 0x00, 0x10]).is_none());
        assert!(without_reordering(&[]).is_none());
    }

    /// An SPS with no VUI at all — what a minimal encoder writes — gets one
    /// that says only this, and the fields before it are kept.
    #[test]
    fn an_sps_without_a_vui_gets_one() {
        // Baseline, level 3.0, 1 reference frame, 640x360, no cropping, no
        // VUI: written field by field.
        let mut w = BitWriter::default();
        w.bits(66, 8);
        w.bits(0xc0, 8);
        w.bits(30, 8);
        w.ue(0); // seq_parameter_set_id
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(2); // pic_order_cnt_type
        w.ue(1); // max_num_ref_frames
        w.flag(false);
        w.ue(39); // 640 / 16 - 1
        w.ue(22); // 368 / 16 - 1
        w.flag(true); // frame_mbs_only_flag
        w.flag(true); // direct_8x8_inference_flag
        w.flag(true); // frame_cropping_flag
        for offset in [0, 0, 0, 4] {
            w.ue(offset);
        }
        w.flag(false); // vui_parameters_present_flag
        let mut stream = vec![0, 0, 0, 1, 0x67];
        stream.extend(escape(&w.finish()));
        assert_eq!(reorder_of(&stream), (1, None));
        let fixed = without_reordering(&stream).unwrap();
        assert_eq!(reorder_of(&fixed), (1, Some((0, 1))));
    }

    /// Emulation prevention survives the round trip: an RBSP with runs of
    /// zeros comes back with its escapes where a decoder expects them.
    #[test]
    fn emulation_prevention_round_trips() {
        let rbsp = [0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x03];
        let escaped = escape(&rbsp);
        assert_eq!(
            escaped,
            [
                0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03, 0x00, 0x05, 0x00, 0x00, 0x03, 0x03
            ]
        );
        assert_eq!(unescape(&escaped), rbsp);
    }
}
