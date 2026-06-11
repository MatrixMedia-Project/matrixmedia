// SPDX-License-Identifier: Apache-2.0
//
// ADR-04 Phase 1 flow tests (failure injection, §6.1-6.2 of the doc):
//   - a panicking recorder subscriber must not kill the fan-out
//   - a blocked/erroring writer must not stall delivery and must
//     surface as a real "failed" recording state + metrics
//   - MM_SWITCH_RECORDER_ISOLATION=inline restores legacy behavior

package main

import (
	"errors"
	"fmt"
	"path/filepath"
	"sync/atomic"
	"testing"
	"time"

	"github.com/pion/rtp"
	"github.com/prometheus/client_golang/prometheus/testutil"
)

// newTestWebRTCSource builds a WebRTCSource without a PeerConnection —
// the registries are plain structs, no network needed. RequestKeyframe
// is safe: with no video track it returns before touching s.pc.
func newTestWebRTCSource(id string) *WebRTCSource {
	return &WebRTCSource{
		id:          id,
		subscribers: make(map[string]PacketHandler),
		stopCh:      make(chan struct{}),
	}
}

func makeOpusPkt(seq uint16, ts uint32) *rtp.Packet {
	return &rtp.Packet{
		Header: rtp.Header{
			Version: 2, PayloadType: 111,
			SequenceNumber: seq, Timestamp: ts,
			Marker: true, SSRC: 2,
		},
		Payload: []byte{0xF8, 0x01, 0x02, 0x03},
	}
}

// stubBlockWriter lets tests inject erroring or blocking disk writes.
type stubBlockWriter struct {
	err     error         // returned by every Write if non-nil
	blockCh chan struct{} // if non-nil, Write blocks until closed
	writes  atomic.Int64
}

func (s *stubBlockWriter) Write(keyframe bool, timestamp int64, b []byte) (int, error) {
	if s.blockCh != nil {
		<-s.blockCh
	}
	s.writes.Add(1)
	if s.err != nil {
		return 0, s.err
	}
	return len(b), nil
}

func (s *stubBlockWriter) Close() error { return nil }

func newTestRecorder(t *testing.T, src *WebRTCSource, id string) *WebMRecorder {
	t.Helper()
	rec, err := NewWebMRecorder(id, filepath.Join(t.TempDir(), id+".webm"), src)
	if err != nil {
		t.Fatalf("NewWebMRecorder: %v", err)
	}
	return rec
}

func waitForState(t *testing.T, rec *WebMRecorder, want RecordingState) {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		if rec.State() == want {
			return
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatalf("recorder state = %q, want %q (timed out)", rec.State(), want)
}

// §6.1 — injected recorder panic: fan-out unaffected, panicking
// subscriber quarantined, recording flips to "failed", metric grows.
func TestRecorderPanicDoesNotKillFanout(t *testing.T) {
	oldSwitch := mediaSwitch
	mediaSwitch = NewMediaSwitch()
	defer func() { mediaSwitch = oldSwitch }()

	src := newTestWebRTCSource("src-panic")
	rec := newTestRecorder(t, src, "recA")
	defer rec.Finalise()
	mediaSwitch.RegisterRecorder("src-panic", rec)

	var viewerPkts atomic.Int64
	src.Subscribe("viewer-1", func(kind string, pkt *rtp.Packet) {
		viewerPkts.Add(1)
	})

	// Replace the recorder's real handler (same subscriber ID) with one
	// that panics on the 3rd packet — the injected recorder failure.
	var seen atomic.Int64
	src.Subscribe("recorder-recA", func(kind string, pkt *rtp.Packet) {
		if seen.Add(1) >= 3 {
			panic("injected recorder panic")
		}
	})

	panicsBefore := testutil.ToFloat64(recorderPanicsTotal)

	// Synthetic packet pump through the real fan-out path. If the panic
	// escaped, the test process would die here.
	done := make(chan struct{})
	go func() {
		defer close(done)
		for i := 0; i < 50; i++ {
			src.fanout("audio", makeOpusPkt(uint16(i), uint32(i)*960))
		}
	}()
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("packet pump stalled")
	}

	if got := viewerPkts.Load(); got != 50 {
		t.Fatalf("viewer received %d/50 packets — fan-out was affected by the panic", got)
	}
	src.mu.RLock()
	_, still := src.subscribers["recorder-recA"]
	src.mu.RUnlock()
	if still {
		t.Fatal("panicking subscriber was not quarantined")
	}
	waitForState(t, rec, RecordingFailed)
	if d := testutil.ToFloat64(recorderPanicsTotal) - panicsBefore; d < 1 {
		t.Fatalf("mm_switch_recorder_panics_total delta = %v, want >= 1", d)
	}
	// Finalise must preserve the failed state (mm-core reads it back).
	rec.Finalise()
	if got := rec.State(); got != RecordingFailed {
		t.Fatalf("state after Finalise = %q, want %q", got, RecordingFailed)
	}
}

// §6.2a — persistent write errors (e.g. ENOSPC) flip the recording to
// "failed" after the threshold instead of being swallowed forever.
func TestWriteErrorsMarkRecordingFailed(t *testing.T) {
	src := newTestWebRTCSource("src-werr")
	rec := newTestRecorder(t, src, "recB")
	defer rec.Finalise()
	if !rec.async {
		t.Fatal("default isolation mode should be async")
	}

	stub := &stubBlockWriter{err: errors.New("no space left on device")}
	rec.mu.Lock()
	rec.audioTrack = stub
	rec.mu.Unlock()

	errsBefore := testutil.ToFloat64(recorderWriteErrorsTotal)

	for i := 0; i < recorderWriteErrorThreshold+5; i++ {
		rec.onPacket("audio", makeOpusPkt(uint16(i), uint32(i)*960))
	}

	waitForState(t, rec, RecordingFailed)
	if d := testutil.ToFloat64(recorderWriteErrorsTotal) - errsBefore; d < recorderWriteErrorThreshold {
		t.Fatalf("mm_switch_recorder_write_errors_total delta = %v, want >= %d",
			d, recorderWriteErrorThreshold)
	}
	// markFailed must have unsubscribed the recorder from the source.
	src.mu.RLock()
	_, still := src.subscribers["recorder-recB"]
	src.mu.RUnlock()
	if still {
		t.Fatal("failed recorder is still subscribed to the source")
	}
}

// §6.2b — a writer blocked forever (stalled volume) must not stall the
// fan-out goroutine: enqueue stays non-blocking, overflow is dropped
// and counted.
func TestBlockedWriterDropsInsteadOfStalling(t *testing.T) {
	src := newTestWebRTCSource("src-block")
	rec := newTestRecorder(t, src, "recC")
	stub := &stubBlockWriter{blockCh: make(chan struct{})}
	rec.mu.Lock()
	rec.audioTrack = stub
	rec.mu.Unlock()

	droppedBefore := testutil.ToFloat64(recorderDroppedPacketsTotal)

	const burst = recorderQueueSize + 200
	start := time.Now()
	for i := 0; i < burst; i++ {
		rec.onPacket("audio", makeOpusPkt(uint16(i), uint32(i)*960))
	}
	elapsed := time.Since(start)

	// Pre-Phase-1 this loop would block on the first stalled Write.
	// Now it is bounded by channel-send cost only.
	if elapsed > 2*time.Second {
		t.Fatalf("fan-out-side delivery stalled behind blocked writer: %v for %d packets",
			elapsed, burst)
	}
	if d := testutil.ToFloat64(recorderDroppedPacketsTotal) - droppedBefore; d < 100 {
		t.Fatalf("mm_switch_recorder_dropped_packets_total delta = %v, want >= 100", d)
	}
	if depth := testutil.ToFloat64(recorderQueueDepth.WithLabelValues("recC")); depth <= 0 {
		t.Fatalf("mm_switch_recorder_queue_depth = %v, want > 0 while writer is blocked", depth)
	}

	// Unblock so Finalise can drain within its timeout, then clean up.
	close(stub.blockCh)
	rec.Finalise()
	if got := rec.State(); got != RecordingFinished {
		t.Fatalf("state after Finalise = %q, want %q", got, RecordingFinished)
	}
}

// Rollback flag: MM_SWITCH_RECORDER_ISOLATION=inline restores the
// legacy synchronous path — writes on the calling goroutine, write
// errors logged/counted but never failing the recording.
func TestInlineModeRestoresLegacyBehavior(t *testing.T) {
	t.Setenv(recorderIsolationEnv, recorderModeInline)

	src := newTestWebRTCSource("src-inline")
	rec := newTestRecorder(t, src, "recD")
	defer rec.Finalise()
	if rec.async {
		t.Fatal("inline mode requested but recorder is async")
	}

	stub := &stubBlockWriter{}
	rec.mu.Lock()
	rec.audioTrack = stub
	rec.mu.Unlock()

	rec.onPacket("audio", makeOpusPkt(1, 960))
	if got := stub.writes.Load(); got != 1 {
		t.Fatalf("inline mode: %d writes after 1 packet, want 1 (synchronous)", got)
	}

	// Erroring writes must NOT fail the recording in inline mode.
	failing := &stubBlockWriter{err: fmt.Errorf("disk error")}
	rec.mu.Lock()
	rec.audioTrack = failing
	rec.mu.Unlock()
	for i := 0; i < recorderWriteErrorThreshold*2; i++ {
		rec.onPacket("audio", makeOpusPkt(uint16(i+2), uint32(i+2)*960))
	}
	if got := rec.State(); got != RecordingActive {
		t.Fatalf("inline mode state = %q after write errors, want %q (legacy log-only)",
			got, RecordingActive)
	}
}

// Sanity: a clean async recording still finalises as "finished" and
// flushes queued packets before closing the file.
func TestAsyncFinaliseFlushesAndFinishes(t *testing.T) {
	src := newTestWebRTCSource("src-clean")
	rec := newTestRecorder(t, src, "recE")
	stub := &stubBlockWriter{}
	rec.mu.Lock()
	rec.audioTrack = stub
	rec.mu.Unlock()

	const n = 25
	for i := 0; i < n; i++ {
		rec.onPacket("audio", makeOpusPkt(uint16(i), uint32(i)*960))
	}
	rec.Finalise()
	if got := rec.State(); got != RecordingFinished {
		t.Fatalf("state = %q, want %q", got, RecordingFinished)
	}
	if got := stub.writes.Load(); got != n {
		t.Fatalf("writer flushed %d/%d packets before close", got, n)
	}
}

// makeVP8Pkt builds a single-packet VP8 access unit. The payload is a VP8
// descriptor (0x10: S=1, PID=0) + a frame whose first byte's P-bit encodes
// keyframe (clear) vs inter-frame (set) — matching handleVP8's detection.
func makeVP8Pkt(keyframe bool, seq uint16, ts uint32) *rtp.Packet {
	first := byte(0x01) // P-bit set -> inter-frame (P-frame)
	if keyframe {
		first = 0x00 // P-bit clear -> keyframe
	}
	return &rtp.Packet{
		Header: rtp.Header{
			Version: 2, PayloadType: 96,
			SequenceNumber: seq, Timestamp: ts,
			Marker: true, SSRC: 1,
		},
		Payload: []byte{0x10, first, 0x00, 0x00, 0x00},
	}
}

// Regression: a recording must NOT begin the file on an inter-frame — a
// VOD that starts on a P-frame is undecodable from frame 1 (black screen,
// the reported bug). Leading P-frames are dropped until the first keyframe.
func TestRecorderDropsLeadingInterFramesUntilKeyframe(t *testing.T) {
	t.Setenv(recorderIsolationEnv, recorderModeInline) // synchronous + deterministic
	src := newTestWebRTCSource("kf-gate")
	rec := newTestRecorder(t, src, "kf-gate")
	defer rec.Finalise()

	// Two leading P-frames arrive before any keyframe — both must be dropped.
	rec.onPacket("video", makeVP8Pkt(false, 1, 1000))
	rec.onPacket("video", makeVP8Pkt(false, 2, 4000))
	rec.mu.Lock()
	wrote := rec.vp8WroteKey
	rec.mu.Unlock()
	if wrote {
		t.Fatal("file began on a P-frame — VOD would be a black screen")
	}

	// The first keyframe opens the file.
	rec.onPacket("video", makeVP8Pkt(true, 3, 7000))
	rec.mu.Lock()
	wrote = rec.vp8WroteKey
	rec.mu.Unlock()
	if !wrote {
		t.Fatal("first keyframe did not open the file")
	}

	// Subsequent inter-frames now write normally (decoder has a reference).
	rec.onPacket("video", makeVP8Pkt(false, 4, 10000))
}
