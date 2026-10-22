package main

// Video codec names, as they appear in SDP/RTP capability negotiation
// (webrtc.MimeTypeVP8 == "video/VP8", webrtc.MimeTypeH264 == "video/H264").
const (
	VideoCodecVP8  = "VP8"
	VideoCodecH264 = "H264"
)

// VideoCoded is implemented by sources that know the codec of the video
// packets they fan out. The switch forwards original RTP untouched, so a
// viewer can only decode a source whose codec its track negotiated —
// routing is per-codec, and the keyframe that starts decoding is detected
// per codec below. Sources that predate codec awareness do not implement
// it; neither do most test fakes.
type VideoCoded interface {
	VideoCodec() string
}

// SourceVideoCodec reports the codec a source's video packets carry.
// Unknown sources default to VP8: that was the only codec the viewer path
// ever forwarded, so the default preserves yesterday's behavior exactly.
// Callers must still handle "H264" (second track) and anything else
// (no track: video is skipped, audio still flows) — never assume VP8.
func SourceVideoCodec(src Source) string {
	if vc, ok := src.(VideoCoded); ok {
		switch vc.VideoCodec() {
		case VideoCodecVP8, VideoCodecH264:
			return vc.VideoCodec()
		}
	}
	return VideoCodecVP8
}

// CodecMimeSubtype maps a pion SDP mime type ("video/VP8") to the short
// name above, or "" when it is not a viewer-supported video codec.
func CodecMimeSubtype(mime string) string {
	switch mime {
	case "video/VP8":
		return VideoCodecVP8
	case "video/H264":
		return VideoCodecH264
	default:
		return ""
	}
}

// ── H264 NAL inspection ───────────────────────────────────────────────
// WebRTC carries H264 in AVCC/RFC 6184 RTP packetization (not Annex B):
// single NAL units, STAP-A aggregation, and FU-A fragmentation. Only the
// first payload byte(s) are ever inspected — never the frame body — so
// these helpers stay cheap on the fan-out path.

// H264 NAL unit types (ITU-T H.264 Table 7-1, first byte low 5 bits).
const (
	h264NALSingleMax = 23 // 1..23: single NAL unit packets
	h264NALSTAPA     = 24 // aggregation
	h264NALFUA       = 28 // fragmentation
	h264NALTypeIDR   = 5  // coded slice of an IDR picture
	h264NALTypeSPS   = 7
	h264NALTypePPS   = 8
)

// IsH264Keyframe reports whether an RTP payload starts a decodable H264
// access unit: a single-NAL IDR, a STAP-A containing an IDR, or the first
// fragment of a fragmented IDR. Anything shorter than a NAL header, or a
// forbidden_zero_bit violation, is not a keyframe — feed it to a decoder
// and it drops the stream, so when in doubt this returns false.
func IsH264Keyframe(payload []byte) bool {
	if len(payload) < 1 || payload[0]&0x80 != 0 {
		return false
	}
	nalType := payload[0] & 0x1F
	switch {
	case nalType >= 1 && nalType <= h264NALSingleMax:
		return nalType == h264NALTypeIDR
	case nalType == h264NALSTAPA:
		return stapAHasIDR(payload[1:])
	case nalType == h264NALFUA:
		// FU indicator (1 byte) + FU header (1 byte): S bit set and the
		// fragmented NAL is an IDR. Later fragments never start decoding.
		return len(payload) >= 2 && payload[1]&0x80 != 0 && payload[1]&0x1F == h264NALTypeIDR
	default:
		return false
	}
}

// stapAHasIDR scans a STAP-A body (everything after the indicator byte)
// for an IDR NAL. Malformed lengths stop the scan — a truncated aggregate
// is damage, not a keyframe.
func stapAHasIDR(body []byte) bool {
	for len(body) >= 2 {
		size := int(body[0])<<8 | int(body[1])
		body = body[2:]
		if size <= 0 || size > len(body) {
			return false
		}
		nal := body[:size]
		body = body[size:]
		if len(nal) > 0 && nal[0]&0x80 == 0 && nal[0]&0x1F == h264NALTypeIDR {
			return true
		}
	}
	return false
}

// ExtractH264ParamSets pulls SPS and PPS NALs (with headers, without
// lengths) out of a STAP-A payload, for caching: a late joiner needs them
// before its first IDR. Returns ok=false when either is missing —
// signalled or cached half-sets make decoders wait forever.
func ExtractH264ParamSets(payload []byte) (sps, pps []byte, ok bool) {
	if len(payload) < 1 || payload[0]&0x1F != h264NALSTAPA {
		return nil, nil, false
	}
	body := payload[1:]
	for len(body) >= 2 {
		size := int(body[0])<<8 | int(body[1])
		body = body[2:]
		if size <= 0 || size > len(body) {
			break
		}
		nal := body[:size]
		body = body[size:]
		if len(nal) == 0 || nal[0]&0x80 != 0 {
			continue
		}
		switch nal[0] & 0x1F {
		case h264NALTypeSPS:
			sps = append([]byte(nil), nal...)
		case h264NALTypePPS:
			pps = append([]byte(nil), nal...)
		}
	}
	if len(sps) == 0 || len(pps) == 0 {
		return nil, nil, false
	}
	return sps, pps, true
}

// BuildH264ParamSetsPacket packs cached SPS+PPS into one STAP-A payload,
// ready to write ahead of a late joiner's first IDR — the same role the
// file source's cached VP8 keyframe plays. The caller stamps sequence and
// timestamp.
func BuildH264ParamSetsPacket(sps, pps []byte) []byte {
	out := make([]byte, 0, 5+len(sps)+len(pps))
	out = append(out, h264NALSTAPA)
	for _, nal := range [][]byte{sps, pps} {
		out = append(out, byte(len(nal)>>8), byte(len(nal)))
		out = append(out, nal...)
	}
	return out
}
