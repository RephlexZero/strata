//! Minimal MPEG-TS scanner that flags buffers carrying H.26x keyframe (IDR)
//! access-unit data.
//!
//! The bonding sink sits *downstream* of `mpegtsmux`, so it can no longer see
//! the GStreamer `DELTA_UNIT`/keyframe buffer flags — by the time bytes reach
//! the sink they are an interleaved MPEG-TS multiplex. The only remaining
//! in-band keyframe signal is the TS **random_access_indicator (RAI)** bit,
//! which `mpegtsmux` sets on the first packet of a keyframe access unit — the
//! *same* signal the receiver's `tsdemux` uses to flag keyframes. This scanner
//! learns the video PID from PAT → PMT (`stream_type` 0x1B = H.264, 0x24 =
//! HEVC) and then marks every TS packet of an RAI-started access unit on that
//! PID as keyframe data, so the sink can raise those packets to
//! `Priority::Critical` for keyframe-protected scheduling / redundancy.
//!
//! It is deliberately conservative: until it has learned the video PID it
//! reports nothing (the caller falls back to the `HEADER`-flag heuristic), so a
//! mux that never sets RAI cannot make things worse than before. All parsing is
//! bounds-checked — a malformed packet yields `false`, never a panic (a panic
//! in the sink `render()` path would tear down the pipeline).

/// Stateful scanner; one instance per stream (the multiplex is sequential).
#[derive(Debug, Default)]
pub struct TsKeyframeScanner {
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    /// True while the current access unit on the video PID is a keyframe AU
    /// (carried across `scan` calls because one AU spans many TS packets / many
    /// `render()` buffers).
    in_keyframe_au: bool,
}

impl TsKeyframeScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// True once the video PID has been learned from the PMT.
    pub fn locked_on(&self) -> bool {
        self.video_pid.is_some()
    }

    /// Scan a buffer of MPEG-TS (one or more 188-byte packets, possibly
    /// mis-aligned). Returns `true` if any packet in it belongs to a keyframe
    /// access unit on the video PID.
    pub fn scan(&mut self, data: &[u8]) -> bool {
        let mut critical = false;
        let mut off = 0usize;
        while off + TS_PACKET_LEN <= data.len() {
            if data[off] != SYNC_BYTE {
                // Lost alignment (rare for mpegtsmux output) — resync byte-wise.
                off += 1;
                continue;
            }
            self.scan_packet(&data[off..off + TS_PACKET_LEN], &mut critical);
            off += TS_PACKET_LEN;
        }
        critical
    }

    fn scan_packet(&mut self, p: &[u8], critical: &mut bool) {
        // p.len() == TS_PACKET_LEN and p[0] == SYNC_BYTE (guaranteed by caller).
        let pusi = (p[1] & 0x40) != 0;
        let pid = (((p[1] & 0x1F) as u16) << 8) | p[2] as u16;
        let afc = (p[3] >> 4) & 0x3; // adaptation_field_control
        let has_af = afc == 0b10 || afc == 0b11;
        let has_payload = afc == 0b01 || afc == 0b11;

        let mut rai = false;
        let mut payload_off = 4usize;
        if has_af {
            let af_len = p[4] as usize;
            if af_len > 0 {
                // First adaptation-field flags byte; RAI is bit 6 (0x40).
                rai = (p[5] & 0x40) != 0;
            }
            payload_off = 5 + af_len;
        }

        if pid == PAT_PID {
            if has_payload {
                self.parse_pat(p, pusi, payload_off);
            }
            return;
        }
        if Some(pid) == self.pmt_pid {
            if has_payload {
                self.parse_pmt(p, pusi, payload_off);
            }
            return;
        }
        if Some(pid) == self.video_pid {
            if pusi {
                // A PUSI on the video PID starts a new access unit; it is a
                // keyframe AU iff this packet carries the random-access flag.
                self.in_keyframe_au = rai;
            }
            if self.in_keyframe_au {
                *critical = true;
            }
        }
    }

    /// Locate the start of a PSI section in a TS packet payload, honoring the
    /// `pointer_field` that precedes a section when PUSI is set.
    fn section<'a>(&self, p: &'a [u8], pusi: bool, payload_off: usize) -> Option<&'a [u8]> {
        if !pusi || payload_off >= TS_PACKET_LEN {
            return None;
        }
        let ptr = p[payload_off] as usize;
        let start = payload_off + 1 + ptr;
        p.get(start..TS_PACKET_LEN)
    }

    fn parse_pat(&mut self, p: &[u8], pusi: bool, payload_off: usize) {
        let Some(s) = self.section(p, pusi, payload_off) else {
            return;
        };
        // table_id 0x00, then 12-bit section_length at s[1..3].
        if s.len() < 12 || s[0] != 0x00 {
            return;
        }
        let section_length = (((s[1] & 0x0F) as usize) << 8) | s[2] as usize;
        let end = (3 + section_length).min(s.len());
        // 8-byte section header, then (program_number, PID) pairs, 4-byte CRC.
        let mut i = 8usize;
        while i + 4 <= end.saturating_sub(4) {
            let program_number = ((s[i] as u16) << 8) | s[i + 1] as u16;
            let pid = (((s[i + 2] & 0x1F) as u16) << 8) | s[i + 3] as u16;
            if program_number != 0 {
                self.pmt_pid = Some(pid);
                return;
            }
            i += 4;
        }
    }

    fn parse_pmt(&mut self, p: &[u8], pusi: bool, payload_off: usize) {
        let Some(s) = self.section(p, pusi, payload_off) else {
            return;
        };
        if s.len() < 12 || s[0] != 0x02 {
            return;
        }
        let section_length = (((s[1] & 0x0F) as usize) << 8) | s[2] as usize;
        let end = (3 + section_length).min(s.len());
        let program_info_length = (((s[10] & 0x0F) as usize) << 8) | s[11] as usize;
        let mut i = 12 + program_info_length;
        // ES loop: stream_type(1), elementary_PID(2), ES_info_length(2), descs.
        while i + 5 <= end.saturating_sub(4) {
            let stream_type = s[i];
            let pid = (((s[i + 1] & 0x1F) as u16) << 8) | s[i + 2] as u16;
            let es_info_length = (((s[i + 3] & 0x0F) as usize) << 8) | s[i + 4] as usize;
            if matches!(stream_type, ST_H264 | ST_HEVC) {
                self.video_pid = Some(pid);
                return;
            }
            i += 5 + es_info_length;
        }
    }
}

const TS_PACKET_LEN: usize = 188;
const SYNC_BYTE: u8 = 0x47;
const PAT_PID: u16 = 0x0000;
const ST_H264: u8 = 0x1B;
const ST_HEVC: u8 = 0x24;

#[cfg(test)]
mod tests {
    use super::*;

    // Build a 188-byte TS packet: pid, pusi, optional RAI, payload bytes.
    fn ts_packet(pid: u16, pusi: bool, rai: bool, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; TS_PACKET_LEN];
        p[0] = SYNC_BYTE;
        p[1] = ((pusi as u8) << 6) | ((pid >> 8) as u8 & 0x1F);
        p[2] = (pid & 0xFF) as u8;
        let payload_start;
        if rai {
            // adaptation_field_control = 0b11 (AF + payload), af_len=1, RAI set.
            p[3] = 0x30;
            p[4] = 1; // adaptation_field_length
            p[5] = 0x40; // random_access_indicator
            payload_start = 6;
        } else {
            p[3] = 0x10; // payload only
            payload_start = 4;
        }
        let n = payload.len().min(TS_PACKET_LEN - payload_start);
        p[payload_start..payload_start + n].copy_from_slice(&payload[..n]);
        p
    }

    // Minimal PAT pointing program 1 at PMT pid 0x1000.
    fn pat(pmt_pid: u16) -> Vec<u8> {
        // pointer_field(0), table_id, section_length, ..., program loop, CRC(4)
        let mut sec = vec![0x00u8]; // pointer_field
        let body = {
            let mut b = Vec::new();
            b.push(0x00); // table_id PAT
            // section_length filled later (covers bytes after this 2-byte field)
            b.push(0xB0);
            b.push(0x00);
            b.extend_from_slice(&[0x00, 0x01, 0xC1, 0x00, 0x00]); // tsid, version, sec nums
            b.extend_from_slice(&[0x00, 0x01]); // program_number = 1
            b.extend_from_slice(&[0xE0 | ((pmt_pid >> 8) as u8), (pmt_pid & 0xFF) as u8]);
            b.extend_from_slice(&[0, 0, 0, 0]); // CRC placeholder
            b
        };
        let section_length = (body.len() - 3) as u16; // bytes after section_length field
        let mut body = body;
        body[1] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
        body[2] = (section_length & 0xFF) as u8;
        sec.extend_from_slice(&body);
        ts_packet(PAT_PID, true, false, &sec)
    }

    // Minimal PMT for program 1 declaring HEVC video on `video_pid`.
    fn pmt(pmt_pid: u16, video_pid: u16) -> Vec<u8> {
        let mut sec = vec![0x00u8]; // pointer_field
        let mut body = Vec::new();
        body.push(0x02); // table_id PMT
        body.push(0xB0);
        body.push(0x00); // section_length placeholder
        body.extend_from_slice(&[0x00, 0x01, 0xC1, 0x00, 0x00]); // program_number=1, ver, secs
        body.extend_from_slice(&[0xE0 | ((video_pid >> 8) as u8), (video_pid & 0xFF) as u8]); // PCR PID
        body.extend_from_slice(&[0xF0, 0x00]); // program_info_length = 0
        // ES entry: HEVC
        body.push(ST_HEVC);
        body.extend_from_slice(&[0xE0 | ((video_pid >> 8) as u8), (video_pid & 0xFF) as u8]);
        body.extend_from_slice(&[0xF0, 0x00]); // ES_info_length = 0
        body.extend_from_slice(&[0, 0, 0, 0]); // CRC placeholder
        let section_length = (body.len() - 3) as u16;
        body[1] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
        body[2] = (section_length & 0xFF) as u8;
        sec.extend_from_slice(&body);
        ts_packet(pmt_pid, true, false, &sec)
    }

    #[test]
    fn learns_video_pid_and_flags_keyframe_au() {
        let pmt_pid = 0x1000u16;
        let video_pid = 0x0100u16;
        let mut sc = TsKeyframeScanner::new();

        assert!(!sc.scan(&pat(pmt_pid)), "PAT alone carries no keyframe");
        assert!(!sc.locked_on());
        assert!(
            !sc.scan(&pmt(pmt_pid, video_pid)),
            "PMT alone carries no keyframe"
        );
        assert!(sc.locked_on(), "video PID learned from PMT");

        // Keyframe AU start (RAI) then a continuation packet (PUSI=0): both critical.
        assert!(
            sc.scan(&ts_packet(video_pid, true, true, &[0xAA])),
            "RAI start = keyframe"
        );
        assert!(
            sc.scan(&ts_packet(video_pid, false, false, &[0xBB])),
            "continuation of keyframe AU still critical"
        );
        // Next AU starts without RAI (a P-frame): not critical.
        assert!(
            !sc.scan(&ts_packet(video_pid, true, false, &[0xCC])),
            "non-RAI AU start = delta, not critical"
        );
        assert!(
            !sc.scan(&ts_packet(video_pid, false, false, &[0xDD])),
            "continuation of delta AU not critical"
        );
    }

    #[test]
    fn audio_or_unknown_pid_never_flagged_before_lock() {
        let mut sc = TsKeyframeScanner::new();
        // An RAI packet on some PID before PAT/PMT seen — must not be flagged
        // (we have not identified the video PID yet).
        assert!(!sc.scan(&ts_packet(0x0101, true, true, &[1, 2, 3])));
        assert!(!sc.locked_on());
    }

    #[test]
    fn multi_packet_buffer_and_resync() {
        let pmt_pid = 0x1000u16;
        let video_pid = 0x0100u16;
        let mut sc = TsKeyframeScanner::new();
        sc.scan(&pat(pmt_pid));
        sc.scan(&pmt(pmt_pid, video_pid));

        // Concatenate an audio packet + a video keyframe packet in one buffer.
        let mut buf = ts_packet(0x0101, true, true, &[9]); // "audio", RAI set
        buf.extend_from_slice(&ts_packet(video_pid, true, true, &[7]));
        assert!(
            sc.scan(&buf),
            "video keyframe packet in a multi-packet buffer is detected"
        );
    }
}
