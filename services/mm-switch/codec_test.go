package main

import (
	"bytes"
	"testing"
)

// A single-NAL IDR: forbidden_zero_bit=0, nal_ref_idc=3, type=5.
func TestIsH264KeyframeSingleIDR(t *testing.T) {
	if !IsH264Keyframe([]byte{0x65, 0x01, 0x02}) {
		t.Fatal("single-NAL IDR not recognised")
	}
}

func TestIsH264KeyframeRejects(t *testing.T) {
	for name, p := range map[string][]byte{
		"empty":               {},
		"forbidden bit set":   {0xE5},
		"non-IDR slice":       {0x61},
		"SPS alone":           {0x67},
		"SEI":                 {0x06},
		"FU-A non-start IDR":  {0x7C, 0x05},
		"FU-A start non-IDR":  {0x7C, 0x81},
		"unknown type 30":     {0x7E},
		"truncated FU-A":      {0x7C},
		"truncated STAP-A":    {0x18, 0x00},
		"STAP-A bad length":   {0x18, 0x00, 0xFF, 0x65},
		"STAP-A SPS only":     {0x18, 0x00, 0x02, 0x67, 0x42},
		"VP8 keyframe header": {0x10, 0x02, 0x00, 0x9D, 0x01, 0x2A}, // S bit set: must NOT pass as H264
	} {
		if IsH264Keyframe(p) {
			t.Fatalf("%s accepted as a keyframe", name)
		}
	}
}

func TestIsH264KeyframeAggregatedAndFragmented(t *testing.T) {
	// STAP-A: [len=2 SPS][len=3 IDR].
	stap := []byte{0x18, 0x00, 0x02, 0x67, 0x42, 0x00, 0x03, 0x65, 0x88, 0x84}
	if !IsH264Keyframe(stap) {
		t.Fatal("STAP-A containing an IDR not recognised")
	}
	// FU-A first fragment of an IDR (S=1, type=5).
	if !IsH264Keyframe([]byte{0x7C, 0x85, 0x00}) {
		t.Fatal("FU-A start of IDR not recognised")
	}
}

func TestExtractH264ParamSets(t *testing.T) {
	sps := []byte{0x67, 0x42, 0x00, 0x1E}
	pps := []byte{0x68, 0xCE, 0x38, 0x80}
	stap := []byte{0x18, 0x00, 0x04}
	stap = append(stap, sps...)
	stap = append(stap, 0x00, 0x04)
	stap = append(stap, pps...)
	stap = append(stap, 0x00, 0x03, 0x65, 0x88, 0x84) // trailing IDR is fine
	gotSPS, gotPPS, ok := ExtractH264ParamSets(stap)
	if !ok {
		t.Fatal("param sets not extracted")
	}
	if !bytes.Equal(gotSPS, sps) || !bytes.Equal(gotPPS, pps) {
		t.Fatalf("wrong NALs extracted: %x %x", gotSPS, gotPPS)
	}
	// The extraction copies: mutating the packet must not corrupt the cache.
	stap[4] ^= 0xFF
	if bytes.Equal(gotSPS, stap[4:8]) {
		t.Fatal("extracted NAL aliases the packet")
	}
	if _, _, ok := ExtractH264ParamSets([]byte{0x18, 0x00, 0x02, 0x67, 0x42}); ok {
		t.Fatal("SPS without PPS accepted")
	}
	if _, _, ok := ExtractH264ParamSets([]byte{0x65}); ok {
		t.Fatal("non-STAP-A accepted")
	}
}

func TestBuildH264ParamSetsPacketRoundTrip(t *testing.T) {
	sps := []byte{0x67, 0x42, 0x00, 0x1E}
	pps := []byte{0x68, 0xCE, 0x38, 0x80}
	pkt := BuildH264ParamSetsPacket(sps, pps)
	pkt = append(pkt, 0x00, 0x03, 0x65, 0x88, 0x84) // trailing IDR NAL
	if !IsH264Keyframe(pkt) {
		t.Fatal("built param-sets packet does not parse as STAP-A")
	}
	gotSPS, gotPPS, ok := ExtractH264ParamSets(pkt)
	if !ok || !bytes.Equal(gotSPS, sps) || !bytes.Equal(gotPPS, pps) {
		t.Fatal("built packet does not round-trip")
	}
}

// codecFake is a fakeSource that additionally names its video codec.
type codecFake struct {
	*fakeSource
	codec string
}

func (f codecFake) VideoCodec() string { return f.codec }

func TestSourceVideoCodecDefaultsVP8(t *testing.T) {
	if got := SourceVideoCodec(&fakeSource{}); got != VideoCodecVP8 {
		t.Fatalf("codec-unaware source mapped to %q, want VP8", got)
	}
	if got := SourceVideoCodec(codecFake{fakeSource: &fakeSource{}, codec: VideoCodecH264}); got != VideoCodecH264 {
		t.Fatalf("H264 source mapped to %q", got)
	}
	if got := CodecMimeSubtype("video/H264"); got != VideoCodecH264 {
		t.Fatalf("mime mapped to %q", got)
	}
	if got := CodecMimeSubtype("video/AV1"); got != "" {
		t.Fatalf("unsupported mime mapped to %q", got)
	}
}
