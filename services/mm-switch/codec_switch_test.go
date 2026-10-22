package main

import (
	"encoding/json"
	"fmt"
	"testing"
	"time"

	"github.com/pion/rtp"
)

// ── H264 source switch ────────────────────────────────────────────────────
// The ad-to-live handoff across codecs is the case that used to show black:
// an H264 live stream behind a VP8 ad, with the keyframe gate trusting a VP8
// bit-test that false-positives on H264 bytes ("starting video" on garbage).

func makeH264Pkt(seq uint16, ts uint32, payload ...byte) *rtp.Packet {
	return &rtp.Packet{
		Header: rtp.Header{
			Version: 2, PayloadType: 102,
			SequenceNumber: seq, Timestamp: ts,
			Marker: true, SSRC: 7,
		},
		Payload: payload,
	}
}

// stap builds a STAP-A payload from raw NAL units (headers included).
func stap(nals ...[]byte) []byte {
	var out []byte
	out = append(out, 24)
	for _, n := range nals {
		out = append(out, byte(len(n)>>8), byte(len(n)))
		out = append(out, n...)
	}
	return out
}

var (
	h264SPS = []byte{0x67, 0x42, 0x00, 0x1E}
	h264PPS = []byte{0x68, 0xCE, 0x38, 0x80}
	h264IDR = []byte{0x65, 0x88, 0x84, 0x21, 0xA0}
)

type videoItem struct {
	seq     uint16
	codec   string
	payload []byte
}

func drainVideo(v *Viewer) []videoItem {
	var out []videoItem
	for {
		select {
		case it := <-v.queue:
			if it.kind == "video" {
				out = append(out, videoItem{seq: it.pkt.SequenceNumber, codec: it.codec, payload: it.pkt.Payload})
			}
		default:
			return out
		}
	}
}

// A late joiner to an H264 source must get params before its first IDR, and
// only one copy even when params arrive again mid-stream.
func TestH264ActivationPrependsParamSets(t *testing.T) {
	v := capturingViewer(t, "h264-join", 500)
	src := &codecFake{fakeSource: &fakeSource{typ: "webrtc"}, codec: VideoCodecH264}
	attach(v, "stream-h264", src)

	src.emit("video", makeH264Pkt(10, 9000, 0x61, 0x01)) // delta: dropped
	// STAP-A with params but no IDR: cached, not forwarded.
	src.emit("video", makeH264Pkt(11, 12000, stap(h264SPS, h264PPS)...))
	if got := drainVideo(v); len(got) != 0 {
		t.Fatalf("forwarded %d packets before any keyframe", len(got))
	}
	src.emit("video", makeH264Pkt(12, 15000, h264IDR...)) // IDR

	got := drainVideo(v)
	if len(got) != 2 {
		t.Fatalf("forwarded %d packets, want param-sets + IDR", len(got))
	}
	// The H264 track keeps its own sequence space (fresh from 0): the two
	// packets continue it as 1, 2 — not the VP8 track's numbers.
	if got[0].seq != 1 || got[1].seq != 2 {
		t.Fatalf("seqs = %v, want [1 2]", []uint16{got[0].seq, got[1].seq})
	}
	if got[0].codec != VideoCodecH264 || got[1].codec != VideoCodecH264 {
		t.Fatalf("routed codecs = %q,%q, want H264,H264", got[0].codec, got[1].codec)
	}
	// The first packet must carry the cached params (late-joiner decodes),
	// the second must be the IDR itself.
	if sps, pps, ok := ExtractH264ParamSets(got[0].payload); !ok || len(sps) == 0 || len(pps) == 0 {
		t.Fatal("first packet is not a param-sets packet")
	}
	if !IsH264Keyframe(got[1].payload) {
		t.Fatal("second packet is not the IDR")
	}
	// One copy only: params seen again mid-stream ride along, unrepeated.
	src.emit("video", makeH264Pkt(13, 18000, stap(h264SPS, h264PPS)...))
	src.emit("video", makeH264Pkt(14, 21000, h264IDR...))
	more := drainVideo(v)
	if len(more) != 2 {
		// No synthetic third packet: params are prepended once, for the
		// first IDR only. (more[0] still carries the in-band STAP-A — that
		// is the publisher's own packet, forwarded as-is.)
		t.Fatalf("second GOP forwarded %d packets, want 2", len(more))
	}
	if !IsH264Keyframe(more[1].payload) {
		t.Fatal("second GOP's IDR missing")
	}
	if more[0].seq != 3 || more[1].seq != 4 {
		t.Fatalf("seqs = %v, want [3 4]", []uint16{more[0].seq, more[1].seq})
	}
}

// An IDR that arrives with no params cached is undecodable: drop it and ask
// for a fresh keyframe rather than "start video" on garbage.
func TestH264IDRWithoutParamsAsksAgain(t *testing.T) {
	v := capturingViewer(t, "h264-noparams", 0)
	src := &codecFake{fakeSource: &fakeSource{typ: "webrtc"}, codec: VideoCodecH264}
	attach(v, "stream-h264", src)

	src.emit("video", makeH264Pkt(10, 9000, h264IDR...))
	if got := drainVideo(v); len(got) != 0 {
		t.Fatalf("forwarded %d packets without params", len(got))
	}
	if n := src.fakeSource.keyframes.Load(); n == 0 {
		t.Fatal("no keyframe requested after undecodable IDR")
	}
}

// Switching codecs mid-programme routes each source to its own track with
// its own sequence space; neither track punches gaps into the other.
func TestCodecSwitchKeepsPerTrackContinuity(t *testing.T) {
	v := capturingViewer(t, "switch", 1000)
	vp8 := &fakeSource{typ: "webrtc"}
	attach(v, "ad-vp8", vp8)
	vp8.emit("video", makeVP8Pkt(true, 1, 0))
	vp8.emit("video", makeVP8Pkt(false, 2, 3000))
	if n := len(drainVideo(v)); n != 2 {
		t.Fatalf("vp8 forwarded %d, want 2", n)
	}
	if v.videoVP8.seq != 1002 {
		t.Fatalf("vp8 track seq = %d, want 1002", v.videoVP8.seq)
	}

	h264 := &codecFake{fakeSource: &fakeSource{typ: "webrtc"}, codec: VideoCodecH264}
	attach(v, "stream-h264", h264)
	h264.emit("video", makeH264Pkt(50, 9000, stap(h264SPS, h264PPS, h264IDR)...))
	got := len(drainVideo(v))
	// STAP-A carries params+IDR in one packet: params prepended + IDR = 2.
	if got != 2 {
		t.Fatalf("h264 forwarded %d, want 2 (params + IDR)", got)
	}
	if v.videoH264.seq != 2 {
		t.Fatalf("h264 track seq = %d, want 2 (own space from 0)", v.videoH264.seq)
	}
	if v.videoVP8.seq != 1002 {
		t.Fatalf("vp8 track seq moved to %d while h264 played", v.videoVP8.seq)
	}

	// Back to VP8: continues where the VP8 track left off.
	attach(v, "ad-vp8-2", vp8)
	vp8.emit("video", makeVP8Pkt(true, 9, 6000))
	if n := len(drainVideo(v)); n != 1 {
		t.Fatalf("vp8 re-activation forwarded %d, want 1", n)
	}
	if v.videoVP8.seq != 1003 {
		t.Fatalf("vp8 track seq = %d, want 1003", v.videoVP8.seq)
	}
}

// The control channel hears about codec changes only: nothing on a steady
// VP8 programme, one message per switch.
func TestCodecChangeSignalledOnSwitch(t *testing.T) {
	ms := NewMediaSwitch()
	vp8 := &fakeSource{typ: "webrtc"}
	h264 := &codecFake{fakeSource: &fakeSource{typ: "webrtc"}, codec: VideoCodecH264}
	ms.AddSource("ad", vp8)
	ms.AddSource("stream", h264)
	app := connectViewerApp(t, ms, "viewer-codec--ann", true)

	watch(t, ms, app, "ad")
	select {
	case raw := <-app.messages:
		t.Fatalf("steady VP8 programme signalled %q, want silence", raw)
	case <-time.After(300 * time.Millisecond):
	}

	watch(t, ms, app, "stream")
	expectCodec(t, app, VideoCodecH264)
	watch(t, ms, app, "ad")
	expectCodec(t, app, VideoCodecVP8)
	// Re-subscribing the same codec is not a change: silence.
	watch(t, ms, app, "ad")
	select {
	case raw := <-app.messages:
		t.Fatalf("same-codec re-subscribe signalled %q, want silence", raw)
	case <-time.After(300 * time.Millisecond):
	}
}

func expectCodec(t *testing.T, app *viewerApp, want string) {
	t.Helper()
	select {
	case raw := <-app.messages:
		var m map[string]string
		if err := json.Unmarshal([]byte(raw), &m); err != nil {
			t.Fatalf("control message %q is not JSON: %v", raw, err)
		}
		if m["type"] != "codec" || m["video"] != want {
			t.Fatalf("control message = %q, want type=codec video=%s", raw, want)
		}
	case <-time.After(5 * time.Second):
		t.Fatalf("no codec message, want video=%s", want)
	}
}

var _ = fmt.Sprint
