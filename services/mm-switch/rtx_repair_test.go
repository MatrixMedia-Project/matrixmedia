// SPDX-License-Identifier: Apache-2.0
//
// Regression tests for the 2026-10-08 live-video corruption.
//
// pion/webrtc v4 unwraps RTX and returns the repaired packets from TrackRemote.Read with
// their ORIGINAL sequence numbers, interleaved with the live stream. libwebrtc publishers
// (Firefox, Chrome, the Android and iOS apps) send a lot of RTX that is not repair at all:
// bandwidth probes that re-send packets the switch already has. On the wire, a 1.9 Mbps
// VP8 publisher with ~1.3% loss sent 1.2 Mbps of RTX, at least 98% of it duplicates.
//
// mm-switch forwarded all of it untouched. The recorder glued the duplicates into frames
// (ffmpeg: "Invalid profile", "Header size larger than data"), and the viewer renumbered
// them as fresh contiguous packets, so phones could not tell they were stale and only the
// periodic keyframes ever decoded.

package main

import (
	"bytes"
	"fmt"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/pion/rtcp"
	"github.com/pion/rtp"
	"github.com/prometheus/client_golang/prometheus/testutil"
)

// ── synthetic VP8 stream ────────────────────────────────────────────────────────────────

// vp8Frame is one encoded frame packetized the way a browser does it: a 15-bit PictureID
// in every packet's descriptor, S=1 on the first packet, the marker bit on the last.
type vp8Frame struct {
	key  bool
	ts   uint32
	data []byte // the VP8 bitstream a recorder must write back verbatim
	pkts []*rtp.Packet
}

// buildVP8Stream makes n frames of pktsPerFrame packets each. Frame 0 and every keyEvery-th
// frame after it are keyframes. Each frame's bitstream is unique, so a frame assembled from
// the wrong packets can never compare equal to the original.
func buildVP8Stream(n, pktsPerFrame, keyEvery int, seq0 uint16, ts0 uint32, pid0 uint16) []vp8Frame {
	const chunk = 6
	frames := make([]vp8Frame, n)
	seq := seq0
	for f := 0; f < n; f++ {
		key := f%keyEvery == 0
		data := make([]byte, pktsPerFrame*chunk)
		for i := range data {
			data[i] = byte(f*31 + i*7 + 1)
		}
		// Byte 0 is the VP8 frame tag: its low bit (P) is clear on keyframes only.
		if key {
			data[0] &^= 0x01
		} else {
			data[0] |= 0x01
		}
		pid := (pid0 + uint16(f)) & 0x7FFF
		ts := ts0 + uint32(f)*3750 // 24 fps on the 90 kHz clock
		pkts := make([]*rtp.Packet, pktsPerFrame)
		for p := 0; p < pktsPerFrame; p++ {
			b0 := byte(0x80) // X=1
			if p == 0 {
				b0 |= 0x10 // S=1, PID=0
			}
			desc := []byte{b0, 0x80, 0x80 | byte(pid>>8), byte(pid)} // I=1, M=1: 15-bit PictureID
			pkts[p] = &rtp.Packet{
				Header: rtp.Header{
					Version: 2, PayloadType: 96, SSRC: 0xBEEF,
					SequenceNumber: seq, Timestamp: ts,
					Marker: p == pktsPerFrame-1,
				},
				Payload: append(desc, data[p*chunk:(p+1)*chunk]...),
			}
			seq++
		}
		frames[f] = vp8Frame{key: key, ts: ts, data: data, pkts: pkts}
	}
	return frames
}

// withRTXProbes returns the stream's packets in arrival order as an RTX-probing publisher
// delivers them through pion: every live packet once, plus re-sends of packets the switch
// already has, plus one genuinely late packet. Probes land mid-frame (after the head of
// frame f >= 1 comes the last packet of frame f-1 again) and between frames (after frame
// f >= 2, the middle packet of frame f-2 and the head of frame f-1). The middle packet of
// frame 10 is late: it lands after the head of frame 11.
func withRTXProbes(frames []vp8Frame) []*rtp.Packet {
	var out []*rtp.Packet
	var late *rtp.Packet
	for f, fr := range frames {
		for p, pkt := range fr.pkts {
			if f == 10 && p == 1 {
				late = pkt
				continue
			}
			out = append(out, pkt)
			if p == 0 && f >= 1 {
				prev := frames[f-1].pkts
				out = append(out, prev[len(prev)-1])
			}
			if f == 11 && p == 0 && late != nil {
				out = append(out, late)
			}
		}
		if f >= 2 {
			out = append(out, frames[f-2].pkts[1], frames[f-1].pkts[0])
		}
	}
	return out
}

// ── capturing block writer ──────────────────────────────────────────────────────────────

type writtenBlock struct {
	key  bool
	tsMs int64
	data []byte
}

// captureBlockWriter records every block the recorder hands to the muxer.
type captureBlockWriter struct {
	mu     sync.Mutex
	blocks []writtenBlock
}

func (c *captureBlockWriter) Write(keyframe bool, timestamp int64, b []byte) (int, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.blocks = append(c.blocks, writtenBlock{keyframe, timestamp, append([]byte(nil), b...)})
	return len(b), nil
}

func (c *captureBlockWriter) Close() error { return nil }

func (c *captureBlockWriter) snapshot() []writtenBlock {
	c.mu.Lock()
	defer c.mu.Unlock()
	return append([]writtenBlock(nil), c.blocks...)
}

// recorderWithCapture builds a recorder whose video blocks land in a captureBlockWriter.
func recorderWithCapture(t *testing.T, src *WebRTCSource, id string) (*WebMRecorder, *captureBlockWriter) {
	t.Helper()
	rec := newTestRecorder(t, src, id)
	capture := &captureBlockWriter{}
	rec.mu.Lock()
	rec.videoTrack = capture
	rec.mu.Unlock()
	return rec, capture
}

// assertFramesWritten checks the recording holds exactly want, in order, byte for byte.
func assertFramesWritten(t *testing.T, got []writtenBlock, want []vp8Frame) {
	t.Helper()
	if len(got) != len(want) {
		t.Errorf("recorder wrote %d video blocks, want %d", len(got), len(want))
	}
	for i := 0; i < len(got) && i < len(want); i++ {
		w := want[i]
		wantTs := int64(w.ts-want[0].ts) * 1000 / 90000
		if !bytes.Equal(got[i].data, w.data) || got[i].key != w.key || got[i].tsMs != wantTs {
			t.Errorf("block %d: key=%v ts=%dms len=%d, want key=%v ts=%dms len=%d (frame assembled from the wrong packets)",
				i, got[i].key, got[i].tsMs, len(got[i].data), w.key, wantTs, len(w.data))
		}
	}
}

// ── recorder ────────────────────────────────────────────────────────────────────────────

// The incident, replayed into the recorder alone: duplicates and a late packet must not
// change a single byte of what is written. Run in both isolation modes.
func TestRecorderSurvivesRTXDuplicatesAndReordering(t *testing.T) {
	for _, mode := range []string{recorderModeInline, recorderModeAsync} {
		t.Run(mode, func(t *testing.T) {
			t.Setenv(recorderIsolationEnv, mode)
			src := newTestWebRTCSource("rtx-rec-" + mode)
			rec, capture := recorderWithCapture(t, src, "rtx-rec-"+mode)

			frames := buildVP8Stream(30, 3, 12, 40000, 1_000_000, 300)
			for _, pkt := range withRTXProbes(frames) {
				rec.onPacket("video", pkt)
			}
			rec.Finalise()

			assertFramesWritten(t, capture.snapshot(), frames)
		})
	}
}

// A packet that is lost for good breaks VP8's reference chain: every inter-frame after it
// decodes against a picture the decoder never had. The recording must skip from the broken
// frame to the next keyframe, not write smeared garbage in between.
func TestRecorderSkipsToNextKeyframeAfterUnrepairedLoss(t *testing.T) {
	t.Setenv(recorderIsolationEnv, recorderModeInline)
	src := newTestWebRTCSource("rtx-loss")
	rec, capture := recorderWithCapture(t, src, "rtx-loss")

	frames := buildVP8Stream(20, 3, 10, 65500, 5000, 32760) // seq and PictureID both wrap
	for f, fr := range frames {
		for p, pkt := range fr.pkts {
			if f == 4 && p == 1 {
				continue // never arrives, never repaired
			}
			rec.onPacket("video", pkt)
		}
	}
	rec.Finalise()

	want := append(append([]vp8Frame(nil), frames[0:4]...), frames[10:]...)
	assertFramesWritten(t, capture.snapshot(), want)
}

// Padding-only packets carry no VP8 descriptor. With PictureIDs in the stream the recorder
// must not mistake them for loss and throw away good frames.
func TestRecorderIgnoresPaddingWhenPictureIDsAreContinuous(t *testing.T) {
	t.Setenv(recorderIsolationEnv, recorderModeInline)
	src := newTestWebRTCSource("rtx-pad")
	rec, capture := recorderWithCapture(t, src, "rtx-pad")

	frames := buildVP8Stream(8, 2, 100, 1000, 0, 7)
	// Re-number so there is room for a padding packet between frames 3 and 4.
	seq := uint16(1000)
	for f := range frames {
		if f == 4 {
			rec.onPacket("video", &rtp.Packet{Header: rtp.Header{
				Version: 2, PayloadType: 96, SSRC: 0xBEEF, SequenceNumber: seq,
				Timestamp: frames[3].ts, Padding: true, PaddingSize: 200,
			}})
			seq++
		}
		for _, pkt := range frames[f].pkts {
			pkt.SequenceNumber = seq
			seq++
			rec.onPacket("video", pkt)
		}
	}
	rec.Finalise()

	assertFramesWritten(t, capture.snapshot(), frames)
}

// ── source ──────────────────────────────────────────────────────────────────────────────

// The source lets each sequence number through once: probe re-sends are dropped (and
// counted), a late repair for a packet that really was missing still gets through.
func TestWebRTCSourceDropsDuplicatePackets(t *testing.T) {
	src := newTestWebRTCSource("rtx-src")
	var got []uint16
	src.Subscribe("collector", func(kind string, pkt *rtp.Packet) {
		got = append(got, pkt.SequenceNumber)
	})

	before := testutil.ToFloat64(sourceDuplicatePacketsTotal.WithLabelValues("video"))
	var win seqWindow
	for _, seq := range []uint16{100, 101, 103, 101, 102, 103, 102, 104, 100} {
		src.ingest(&win, "video", &rtp.Packet{Header: rtp.Header{SequenceNumber: seq}})
	}

	want := []uint16{100, 101, 103, 102, 104}
	if fmt.Sprint(got) != fmt.Sprint(want) {
		t.Fatalf("subscriber saw %v, want %v", got, want)
	}
	if d := testutil.ToFloat64(sourceDuplicatePacketsTotal.WithLabelValues("video")) - before; d != 4 {
		t.Fatalf("mm_switch_source_duplicate_packets_total{kind=video} grew by %v, want 4", d)
	}
}

// End to end through the source: the incident's packet sequence, fanned out to a recorder,
// produces a clean recording.
func TestIncidentReplayThroughSourceProducesCleanRecording(t *testing.T) {
	src := newTestWebRTCSource("rtx-e2e")
	rec, capture := recorderWithCapture(t, src, "rtx-e2e")

	frames := buildVP8Stream(40, 4, 15, 12000, 77, 1)
	var win seqWindow
	for _, pkt := range withRTXProbes(frames) {
		src.ingest(&win, "video", pkt)
	}
	rec.Finalise()

	assertFramesWritten(t, capture.snapshot(), frames)
}

// ── viewer ──────────────────────────────────────────────────────────────────────────────

// fakeSource is a Source whose Type and keyframe requests a test controls.
type fakeSource struct {
	typ       string
	mu        sync.Mutex
	handler   PacketHandler
	keyframes atomic.Int64
}

func (f *fakeSource) Type() string     { return f.typ }
func (f *fakeSource) IsActive() bool   { return true }
func (f *fakeSource) RequestKeyframe() { f.keyframes.Add(1) }
func (f *fakeSource) Stop()            {}
func (f *fakeSource) Subscribe(id string, h PacketHandler) func() {
	f.mu.Lock()
	f.handler = h
	f.mu.Unlock()
	return func() {}
}
func (f *fakeSource) emit(kind string, pkt *rtp.Packet) {
	f.mu.Lock()
	h := f.handler
	f.mu.Unlock()
	h(kind, pkt)
}

// capturingViewer is a viewer whose outgoing packets queue up for inspection: async mode
// with no writer goroutine.
func capturingViewer(t *testing.T, id string, lastSeq uint16) *Viewer {
	t.Helper()
	return &Viewer{
		id:         id,
		async:      true,
		queue:      make(chan viewerItem, 4096),
		stop:       make(chan struct{}),
		videoTrack: newTestVideoTrack(t),
		videoSeq:   lastSeq,
	}
}

func attach(v *Viewer, id string, src Source) {
	v.mu.Lock()
	v.pendingSource = src
	v.mu.Unlock()
	v.activateSource(id, src)
}

func drainVideoSeqs(v *Viewer) []uint16 {
	var seqs []uint16
	for {
		select {
		case it := <-v.queue:
			if it.kind == "video" {
				seqs = append(seqs, it.pkt.SequenceNumber)
			}
		default:
			return seqs
		}
	}
}

// A live publisher's numbering must survive the trip through the viewer: a gap stays a gap
// (so the phone knows a packet is missing and can wait for the repair), a late repair keeps
// its place, and nothing older than the first forwarded packet leaks into the previous
// source's numbering. Checked across the 16-bit wrap.
func TestViewerPreservesPublisherSequenceGaps(t *testing.T) {
	for _, last := range []uint16{0, 500, 65534} {
		t.Run(fmt.Sprint(last), func(t *testing.T) {
			v := capturingViewer(t, "gaps", last)
			src := &fakeSource{typ: "webrtc"}
			attach(v, "stream-gaps", src)

			src.emit("video", makeVP8Pkt(true, 100, 9000)) // keyframe opens the viewer
			src.emit("video", makeVP8Pkt(false, 101, 12000))
			src.emit("video", makeVP8Pkt(false, 103, 18000)) // 102 missing for now
			src.emit("video", makeVP8Pkt(false, 102, 15000)) // the repair
			src.emit("video", makeVP8Pkt(false, 99, 6000))   // older than the keyframe
			src.emit("video", makeVP8Pkt(false, 104, 21000))

			want := []uint16{last + 1, last + 2, last + 4, last + 3, last + 5}
			if got := drainVideoSeqs(v); fmt.Sprint(got) != fmt.Sprint(want) {
				t.Fatalf("viewer sent seqs %v, want %v", got, want)
			}
			if v.videoSeq != last+5 {
				t.Fatalf("viewer's last seq = %d, want %d (the next source must continue after the highest sent)",
					v.videoSeq, last+5)
			}
		})
	}
}

// The ad path is server-packetized and replays its cached keyframe as seqs 1..n before
// restarting at 1, so it must keep the plain counter that has always worked for it.
func TestViewerKeepsContiguousNumberingForFileSources(t *testing.T) {
	v := capturingViewer(t, "ad", 41)
	src := &fakeSource{typ: "file"}
	attach(v, "ad--someone-1", src)

	src.emit("video", makeVP8Pkt(true, 1, 0))
	src.emit("video", makeVP8Pkt(false, 2, 3000))
	src.emit("video", makeVP8Pkt(false, 1, 3000))
	src.emit("video", makeVP8Pkt(false, 2, 6000))

	want := []uint16{42, 43, 44, 45}
	if got := drainVideoSeqs(v); fmt.Sprint(got) != fmt.Sprint(want) {
		t.Fatalf("ad viewer sent seqs %v, want %v", got, want)
	}
}

// A phone that cannot decode asks for a keyframe (PLI or FIR). That request must reach the
// publisher, rate-limited so a struggling decoder cannot flood the broadcaster.
func TestViewerForwardsKeyframeRequestsRateLimited(t *testing.T) {
	v := capturingViewer(t, "pli", 0)
	src := &fakeSource{typ: "webrtc"}
	attach(v, "stream-pli", src)
	base := src.keyframes.Load()

	v.handleRTCP([]rtcp.Packet{&rtcp.PictureLossIndication{MediaSSRC: 1}})
	v.handleRTCP([]rtcp.Packet{&rtcp.FullIntraRequest{MediaSSRC: 1}})
	v.handleRTCP([]rtcp.Packet{&rtcp.ReceiverReport{}})
	if got := src.keyframes.Load() - base; got != 1 {
		t.Fatalf("forwarded %d keyframe requests in a burst, want 1", got)
	}

	v.mu.Lock()
	v.lastKeyframeRequest = time.Now().Add(-2 * viewerKeyframeRequestInterval)
	v.mu.Unlock()
	v.handleRTCP([]rtcp.Packet{&rtcp.FullIntraRequest{MediaSSRC: 1}})
	if got := src.keyframes.Load() - base; got != 2 {
		t.Fatalf("forwarded %d keyframe requests after the interval, want 2", got)
	}
}
