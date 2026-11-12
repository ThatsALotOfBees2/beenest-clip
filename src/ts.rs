//! Minimal MPEG-TS scanner: finds the video stream and its keyframes.
//!
//! We never decode or rewrite the stream; we only need to know where a clip
//! may start, and to keep a copy of the tables a player needs to read it.

pub const PACKET: usize = 188;
pub const SYNC: u8 = 0x47;

pub struct Scanner {
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    pat: Option<[u8; PACKET]>,
    pmt: Option<[u8; PACKET]>,
}

impl Scanner {
    pub fn new() -> Self {
        Scanner { pmt_pid: None, video_pid: None, pat: None, pmt: None }
    }

    /// Inspect one packet. Returns true if it begins a video keyframe.
    pub fn scan(&mut self, pkt: &[u8; PACKET]) -> bool {
        if pkt[0] != SYNC {
            return false;
        }
        let pid = ((pkt[1] as u16 & 0x1f) << 8) | pkt[2] as u16;
        let pusi = pkt[1] & 0x40 != 0;
        let has_af = pkt[3] & 0x20 != 0;
        let has_payload = pkt[3] & 0x10 != 0;

        if pid == 0 {
            if let Some(sec) = section(pkt, pusi, has_af, has_payload) {
                self.pat = Some(*pkt);
                self.pmt_pid = parse_pat(sec).or(self.pmt_pid);
            }
            return false;
        }
        if Some(pid) == self.pmt_pid {
            if let Some(sec) = section(pkt, pusi, has_af, has_payload) {
                self.pmt = Some(*pkt);
                self.video_pid = parse_pmt(sec).or(self.video_pid);
            }
            return false;
        }
        // The muxer sets the random-access flag on the first packet of a
        // keyframe. Audio packets carry it too, hence the PID check.
        Some(pid) == self.video_pid && pusi && has_af && pkt[4] > 0 && pkt[5] & 0x40 != 0
    }

    /// PAT + PMT packets, to be written ahead of a clip's first keyframe.
    pub fn tables(&self) -> Option<[u8; PACKET * 2]> {
        let mut out = [0u8; PACKET * 2];
        out[..PACKET].copy_from_slice(self.pat.as_ref()?);
        out[PACKET..].copy_from_slice(self.pmt.as_ref()?);
        Some(out)
    }
}

/// Payload of a packet that starts a PSI section, positioned at table_id.
fn section(pkt: &[u8; PACKET], pusi: bool, has_af: bool, has_payload: bool) -> Option<&[u8]> {
    if !pusi || !has_payload {
        return None;
    }
    let mut off = 4;
    if has_af {
        off += 1 + pkt[4] as usize;
    }
    let pointer = *pkt.get(off)? as usize;
    pkt.get(off + 1 + pointer..)
}

/// End of the section body, excluding the CRC, clamped to this packet.
fn body_end(sec: &[u8]) -> Option<usize> {
    let len = ((*sec.get(1)? as usize & 0x0f) << 8) | *sec.get(2)? as usize;
    Some((3 + len).saturating_sub(4).min(sec.len()))
}

fn parse_pat(sec: &[u8]) -> Option<u16> {
    if *sec.first()? != 0x00 {
        return None;
    }
    let end = body_end(sec)?;
    sec.get(8..end)?.chunks_exact(4).find_map(|e| {
        let program = (e[0] as u16) << 8 | e[1] as u16;
        (program != 0).then(|| (e[2] as u16 & 0x1f) << 8 | e[3] as u16)
    })
}

fn parse_pmt(sec: &[u8]) -> Option<u16> {
    if *sec.first()? != 0x02 {
        return None;
    }
    let end = body_end(sec)?;
    let info_len = ((*sec.get(10)? as usize & 0x0f) << 8) | *sec.get(11)? as usize;
    let mut i = 12 + info_len;
    while i + 5 <= end {
        let stream_type = sec[i];
        let pid = (sec[i + 1] as u16 & 0x1f) << 8 | sec[i + 2] as u16;
        let es_len = ((sec[i + 3] as usize & 0x0f) << 8) | sec[i + 4] as usize;
        // MPEG-2, MPEG-4, H.264, HEVC
        if matches!(stream_type, 0x02 | 0x10 | 0x1b | 0x24) {
            return Some(pid);
        }
        i += 5 + es_len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(pid: u16, pusi: bool, af: Option<u8>, payload: &[u8]) -> [u8; PACKET] {
        let mut p = [0xffu8; PACKET];
        p[0] = SYNC;
        p[1] = (if pusi { 0x40 } else { 0 }) | (pid >> 8) as u8;
        p[2] = pid as u8;
        let mut off = 4;
        p[3] = 0x10;
        if let Some(flags) = af {
            p[3] |= 0x20;
            p[4] = 1;
            p[5] = flags;
            off = 6;
        }
        p[off..off + payload.len()].copy_from_slice(payload);
        p
    }

    fn pat(pmt_pid: u16) -> [u8; PACKET] {
        // pointer, table_id, section_length=13, tsid(2), ver, sec, last, entry(4), crc(4)
        let s = [0, 0x00, 0xb0, 13, 0, 1, 0xc1, 0, 0, 0, 1, 0xe0 | (pmt_pid >> 8) as u8, pmt_pid as u8, 0, 0, 0, 0];
        packet(0, true, None, &s)
    }

    fn pmt(pid: u16, streams: &[(u8, u16)]) -> [u8; PACKET] {
        let len = 9 + streams.len() * 5 + 4;
        let mut s = vec![0, 0x02, 0xb0, len as u8, 0, 1, 0xc1, 0, 0, 0xe1, 0, 0xf0, 0];
        for &(ty, p) in streams {
            s.extend_from_slice(&[ty, 0xe0 | (p >> 8) as u8, p as u8, 0xf0, 0]);
        }
        s.extend_from_slice(&[0; 4]);
        packet(pid, true, None, &s)
    }

    #[test]
    fn finds_video_keyframes_only() {
        let mut sc = Scanner::new();
        // Audio listed first, to check we do not just take the first stream.
        assert!(!sc.scan(&pat(0x1000)));
        assert!(!sc.scan(&pmt(0x1000, &[(0x0f, 0x101), (0x1b, 0x100)])));
        assert!(sc.tables().is_some());

        assert!(sc.scan(&packet(0x100, true, Some(0x50), &[0, 0, 1])));
        // Video frame start without the random-access flag.
        assert!(!sc.scan(&packet(0x100, true, Some(0x10), &[0, 0, 1])));
        // Continuation packet of a keyframe.
        assert!(!sc.scan(&packet(0x100, false, None, &[1, 2, 3])));
        // Audio sets the flag on every frame; must be ignored.
        assert!(!sc.scan(&packet(0x101, true, Some(0x50), &[0, 0, 1])));
    }

    #[test]
    fn no_keyframes_before_tables() {
        let mut sc = Scanner::new();
        assert!(!sc.scan(&packet(0x100, true, Some(0x50), &[0, 0, 1])));
        assert!(sc.tables().is_none());
    }
}
