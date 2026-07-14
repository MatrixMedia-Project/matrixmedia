//! Minimal media duration probe for WebM and MP4 files.
//!
//! Downloads the first 128KB of a media URL and extracts the duration
//! from the container header. No external tools (ffprobe) required.

/// Probe a media URL and return its duration in seconds.
/// Supports WebM (Matroska) and MP4 containers.
/// Returns None if probing fails (unknown format, network error, etc.).
pub async fn probe_duration(url: &str) -> Option<i32> {
    let client = mm_core::http::shared();
    // Only need the first 128KB for container headers
    let resp = client
        .get(url)
        .header("Range", "bytes=0-131071")
        .send()
        .await
        .ok()?;

    let data = if resp.status().is_success() || resp.status().as_u16() == 206 {
        resp.bytes().await.ok()?
    } else {
        // Range not supported — download whole file (small ad files are OK)
        let resp2 = client.get(url).send().await.ok()?;
        resp2.bytes().await.ok()?
    };

    if data.len() < 8 {
        return None;
    }

    // Detect format by magic bytes
    if &data[..4] == b"\x1a\x45\xdf\xa3" {
        // EBML header → WebM/Matroska
        probe_webm_duration(&data)
    } else if &data[4..8] == b"ftyp" || &data[4..8] == b"moov" || &data[4..8] == b"free" {
        // MP4/MOV
        probe_mp4_duration(&data)
    } else {
        None
    }
}

/// Parse WebM EBML to find Segment > Info > Duration.
fn probe_webm_duration(data: &[u8]) -> Option<i32> {
    let mut timecode_scale: u64 = 1_000_000; // default 1ms
    let mut duration_val: Option<f64> = None;

    // Simple scan: look for known EBML element IDs in the byte stream.
    // Not a full parser but works for well-formed files.
    let mut i = 0;
    while i + 12 < data.len() {
        // TimecodeScale: ID = 0x2A_D7_B1 (3 bytes)
        if i + 3 < data.len() && data[i] == 0x2A && data[i + 1] == 0xD7 && data[i + 2] == 0xB1 {
            if let Some((val, _)) = read_ebml_uint(&data[i + 3..]) {
                timecode_scale = val;
            }
        }
        // Duration: ID = 0x44_89 (2 bytes)
        if i + 2 < data.len() && data[i] == 0x44 && data[i + 1] == 0x89 {
            if let Some((val, _)) = read_ebml_float(&data[i + 2..]) {
                duration_val = Some(val);
            }
        }
        i += 1;
    }

    let dur = duration_val?;
    // Duration is in timecode_scale units.
    // seconds = dur * timecode_scale / 1e9
    let secs = dur * (timecode_scale as f64) / 1_000_000_000.0;
    Some(secs.round() as i32)
}

/// Parse MP4 to find mvhd box → duration / timescale.
fn probe_mp4_duration(data: &[u8]) -> Option<i32> {
    // Scan for 'mvhd' box
    let mut i = 0;
    while i + 8 < data.len() {
        let box_size = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        let box_type = &data[i + 4..i + 8];

        if box_type == b"mvhd" && i + 24 < data.len() {
            let version = data[i + 8];
            if version == 0 && i + 28 < data.len() {
                // version 0: timescale at offset 12 (4 bytes), duration at 16 (4 bytes)
                let ts = u32::from_be_bytes([data[i + 20], data[i + 21], data[i + 22], data[i + 23]]);
                let dur = u32::from_be_bytes([data[i + 24], data[i + 25], data[i + 26], data[i + 27]]);
                if ts > 0 {
                    return Some((dur as f64 / ts as f64).round() as i32);
                }
            } else if version == 1 && i + 36 < data.len() {
                // version 1: timescale at offset 20 (4 bytes), duration at 24 (8 bytes)
                let ts = u32::from_be_bytes([data[i + 28], data[i + 29], data[i + 30], data[i + 31]]);
                let dur = u64::from_be_bytes([
                    data[i + 32], data[i + 33], data[i + 34], data[i + 35],
                    data[i + 36], data[i + 37], data[i + 38], data[i + 39],
                ]);
                if ts > 0 {
                    return Some((dur as f64 / ts as f64).round() as i32);
                }
            }
        }

        // Recurse into container boxes
        if matches!(box_type, b"moov" | b"trak" | b"mdia") {
            // Skip box header, search inside
            i += 8;
            continue;
        }

        if box_size < 8 || i + box_size > data.len() {
            break;
        }
        i += box_size;
    }
    None
}

/// Read an EBML variable-size unsigned integer.
fn read_ebml_uint(data: &[u8]) -> Option<(u64, usize)> {
    if data.is_empty() {
        return None;
    }
    // First byte encodes the size length via leading zeros
    let size_len = read_ebml_size(data)?;
    let data_len = size_len.0 as usize;
    let offset = size_len.1;

    if offset + data_len > data.len() || data_len > 8 {
        return None;
    }

    let mut val: u64 = 0;
    for j in 0..data_len {
        val = (val << 8) | data[offset + j] as u64;
    }
    Some((val, offset + data_len))
}

/// Read an EBML float (after the element ID, at the size+data position).
fn read_ebml_float(data: &[u8]) -> Option<(f64, usize)> {
    if data.is_empty() {
        return None;
    }
    let size_info = read_ebml_size(data)?;
    let float_len = size_info.0 as usize;
    let offset = size_info.1;

    if offset + float_len > data.len() {
        return None;
    }

    if float_len == 4 {
        let bytes = [data[offset], data[offset + 1], data[offset + 2], data[offset + 3]];
        Some((f32::from_be_bytes(bytes) as f64, offset + 4))
    } else if float_len == 8 {
        let bytes = [
            data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
            data[offset + 4], data[offset + 5], data[offset + 6], data[offset + 7],
        ];
        Some((f64::from_be_bytes(bytes), offset + 8))
    } else {
        None
    }
}

/// Read EBML variable-length size. Returns (value, bytes_consumed).
fn read_ebml_size(data: &[u8]) -> Option<(u64, usize)> {
    if data.is_empty() {
        return None;
    }
    let first = data[0];
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || len > data.len() {
        return None;
    }
    let mut val = (first & (0xFF >> len)) as u64;
    for j in 1..len {
        val = (val << 8) | data[j] as u64;
    }
    Some((val, len))
}
