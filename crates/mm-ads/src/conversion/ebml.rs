//! Minimal Matroska/WebM walker for the conformance check (spec §5.8). mm-switch's demuxer reads
//! only SimpleBlocks and silently drops BlockGroups (services/mm-switch internal-ebml-fork
//! webm.go:107-118), so we count both, per track. Not a general parser: unknown-size elements are
//! accepted only for the Segment, which is all ffmpeg writes to a seekable file.

use std::collections::BTreeMap;

const SEGMENT: u64 = 0x1853_8067;
const CLUSTER: u64 = 0x1F43_B675;
const TRACKS: u64 = 0x1654_AE6B;
const TRACK_ENTRY: u64 = 0xAE;
const TRACK_NUMBER: u64 = 0xD7;
const TRACK_TYPE: u64 = 0x83;
const SIMPLE_BLOCK: u64 = 0xA3;
const BLOCK_GROUP: u64 = 0xA0;
const BLOCK: u64 = 0xA1;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TrackBlocks {
    /// Matroska TrackType: 1 video, 2 audio.
    pub track_type: Option<u64>,
    pub simple_blocks: u64,
    pub block_groups: u64,
    pub laced: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EbmlSummary {
    pub tracks: BTreeMap<u64, TrackBlocks>,
}

impl EbmlSummary {
    pub fn video(&self) -> Option<&TrackBlocks> {
        self.tracks.values().find(|t| t.track_type == Some(1))
    }

    pub fn audio(&self) -> Option<&TrackBlocks> {
        self.tracks.values().find(|t| t.track_type == Some(2))
    }
}

/// Element ID: its length (1-4) is the first byte's leading zeros + 1; the marker bit stays in.
fn read_id(buf: &[u8], pos: &mut usize) -> Result<u64, String> {
    let first = *buf.get(*pos).ok_or("truncated element id")?;
    let len = first.leading_zeros() as usize + 1;
    if len > 4 {
        return Err(format!("bad element id at byte {}", *pos));
    }
    let bytes = buf.get(*pos..*pos + len).ok_or("truncated element id")?;
    *pos += len;
    Ok(bytes.iter().fold(0u64, |acc, b| (acc << 8) | *b as u64))
}

/// A size VINT with its marker removed. `None` = "unknown size" (every value bit set).
fn read_vint(buf: &[u8], pos: &mut usize) -> Result<Option<u64>, String> {
    let first = *buf.get(*pos).ok_or("truncated size")?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 {
        return Err(format!("bad size at byte {}", *pos));
    }
    let bytes = buf.get(*pos..*pos + len).ok_or("truncated size")?;
    *pos += len;
    let mask: u8 = if len == 8 { 0 } else { 0xFF >> len };
    let mut v = (bytes[0] & mask) as u64;
    for b in &bytes[1..] {
        v = (v << 8) | *b as u64;
    }
    let all_ones = (1u64 << (7 * len)) - 1;
    Ok(if v == all_ones { None } else { Some(v) })
}

fn read_uint(data: &[u8]) -> u64 {
    data.iter().fold(0, |a, b| (a << 8) | *b as u64)
}

pub fn summarize(buf: &[u8]) -> Result<EbmlSummary, String> {
    let mut out = EbmlSummary::default();
    walk(buf, 0, buf.len(), &mut out)?;
    Ok(out)
}

fn walk(buf: &[u8], mut pos: usize, end: usize, out: &mut EbmlSummary) -> Result<(), String> {
    while pos < end {
        let id = read_id(buf, &mut pos)?;
        let size = read_vint(buf, &mut pos)?;
        let data_end = match size {
            Some(n) => pos
                .checked_add(n as usize)
                .filter(|e| *e <= end)
                .ok_or_else(|| format!("element {id:#x} overruns its parent"))?,
            None if id == SEGMENT => end,
            None => return Err(format!("unknown-size element {id:#x}")),
        };
        match id {
            SEGMENT | CLUSTER | TRACKS => walk(buf, pos, data_end, out)?,
            TRACK_ENTRY => track_entry(&buf[pos..data_end], out)?,
            SIMPLE_BLOCK => block(&buf[pos..data_end], out, false)?,
            BLOCK_GROUP => block_group(&buf[pos..data_end], out)?,
            _ => {}
        }
        pos = data_end;
    }
    Ok(())
}

fn children(data: &[u8], mut f: impl FnMut(u64, &[u8]) -> Result<(), String>) -> Result<(), String> {
    let mut p = 0;
    while p < data.len() {
        let id = read_id(data, &mut p)?;
        let n = read_vint(data, &mut p)?.ok_or("unknown size inside a known-size element")? as usize;
        let v = data.get(p..p + n).ok_or("truncated child element")?;
        f(id, v)?;
        p += n;
    }
    Ok(())
}

fn track_entry(data: &[u8], out: &mut EbmlSummary) -> Result<(), String> {
    let (mut number, mut kind) = (None, None);
    children(data, |id, v| {
        match id {
            TRACK_NUMBER => number = Some(read_uint(v)),
            TRACK_TYPE => kind = Some(read_uint(v)),
            _ => {}
        }
        Ok(())
    })?;
    let number = number.ok_or("TrackEntry without TrackNumber")?;
    out.tracks.entry(number).or_default().track_type = kind;
    Ok(())
}

fn block_group(data: &[u8], out: &mut EbmlSummary) -> Result<(), String> {
    children(data, |id, v| if id == BLOCK { block(v, out, true) } else { Ok(()) })
}

/// Block header: track number (VINT), 2-byte timecode, flags (lacing in bits 1-2).
fn block(data: &[u8], out: &mut EbmlSummary, in_group: bool) -> Result<(), String> {
    let mut p = 0;
    let track = read_vint(data, &mut p)?.ok_or("block without a track number")?;
    let flags = *data.get(p + 2).ok_or("truncated block header")?;
    let t = out.tracks.entry(track).or_default();
    if in_group { t.block_groups += 1 } else { t.simple_blocks += 1 }
    if flags & 0x06 != 0 {
        t.laced += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// id bytes + 8-byte size VINT + payload.
    fn el(id: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.push(0x01);
        v.extend_from_slice(&(payload.len() as u64).to_be_bytes()[1..]);
        v.extend_from_slice(payload);
        v
    }

    fn entry(number: u8, kind: u8) -> Vec<u8> {
        el(&[0xAE], &[el(&[0xD7], &[number]), el(&[0x83], &[kind])].concat())
    }

    fn simple(track: u8, flags: u8) -> Vec<u8> {
        el(&[0xA3], &[0x80 | track, 0x00, 0x00, flags, 0xAA, 0xBB])
    }

    fn group(track: u8) -> Vec<u8> {
        el(&[0xA0], &el(&[0xA1], &[0x80 | track, 0x00, 0x00, 0x00, 0xCC]))
    }

    fn file(cluster: Vec<u8>, segment_unknown_size: bool) -> Vec<u8> {
        let tracks = el(&[0x16, 0x54, 0xAE, 0x6B], &[entry(1, 1), entry(2, 2)].concat());
        let body = [tracks, el(&[0x1F, 0x43, 0xB6, 0x75], &cluster)].concat();
        let mut f = el(&[0x1A, 0x45, 0xDF, 0xA3], &[0x42, 0x86, 0x81, 0x01]);
        if segment_unknown_size {
            f.extend_from_slice(&[0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
            f.extend_from_slice(&body);
        } else {
            f.extend(el(&[0x18, 0x53, 0x80, 0x67], &body));
        }
        f
    }

    #[test]
    fn counts_simple_blocks_and_groups_per_track() {
        let s = summarize(&file([simple(1, 0x80), simple(1, 0x00), simple(2, 0x80), group(2)].concat(), false)).unwrap();
        let v = s.video().unwrap();
        assert_eq!((v.simple_blocks, v.block_groups, v.laced), (2, 0, 0));
        let a = s.audio().unwrap();
        assert_eq!((a.simple_blocks, a.block_groups), (1, 1));
    }

    #[test]
    fn an_unknown_size_segment_is_fine_and_lacing_is_counted() {
        let s = summarize(&file([simple(1, 0x82), simple(2, 0x80)].concat(), true)).unwrap();
        assert_eq!(s.video().unwrap().laced, 1);
    }

    #[test]
    fn an_unknown_size_cluster_or_truncation_is_an_error() {
        let mut f = file(vec![], false);
        f.extend_from_slice(&[0x1F, 0x43, 0xB6, 0x75, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(summarize(&f).is_err());
        let whole = file([simple(1, 0x80)].concat(), false);
        assert!(summarize(&whole[..whole.len() - 3]).is_err());
    }
}
