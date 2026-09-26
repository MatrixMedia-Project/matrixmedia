package main

import (
	"testing"
	"time"

	"github.com/pion/rtp"
	"github.com/pion/webrtc/v4"
)

// A packet with a known marshalled size, so the arithmetic is checkable by eye.
func testPacket(payloadLen int) *rtp.Packet {
	return &rtp.Packet{
		Header:  rtp.Header{Version: 2, SSRC: 1234, SequenceNumber: 1, Timestamp: 1},
		Payload: make([]byte, payloadLen),
	}
}

func syncViewer(t *testing.T, id, source string) *Viewer {
	t.Helper()
	v := &Viewer{id: id, videoTrack: newTestVideoTrack(t), audioTrack: newTestAudioTrack(t)}
	v.mu.Lock()
	v.billingSource = source
	v.mu.Unlock()
	return v
}

// ── What is counted ──────────────────────────────────────────────────────────

// The RTP header and payload, PLUS the transport overhead the provider also bills.
// Counting MarshalSize alone would systematically under-bill by the UDP and IP
// headers and SRTP's auth tag — which is the safe direction, but silently.
func TestEgressCountsMarshalledSizePlusTransportOverhead(t *testing.T) {
	v := syncViewer(t, "v1", "stream-b1")
	pkt := testPacket(1000)

	v.writeTrack("video", pkt)

	want := int64(pkt.MarshalSize()) + egressOverheadBytes
	_, got := v.EgressSnapshot()
	if got != want {
		t.Fatalf("counted %d bytes, want %d (MarshalSize %d + overhead %d)",
			got, want, pkt.MarshalSize(), egressOverheadBytes)
	}
	if egressOverheadBytes == 0 {
		t.Fatal("the overhead must not default to zero: a provider bills the UDP and " +
			"IP headers and the SRTP auth tag whether or not we count them")
	}
}

func TestEgressAccumulatesAcrossPacketsAndTracks(t *testing.T) {
	v := syncViewer(t, "v1", "stream-b1")
	video := testPacket(1000)
	audio := testPacket(80)

	v.writeTrack("video", video)
	v.writeTrack("video", video)
	v.writeTrack("audio", audio)

	want := 2*(int64(video.MarshalSize())+egressOverheadBytes) +
		(int64(audio.MarshalSize()) + egressOverheadBytes)
	if _, got := v.EgressSnapshot(); got != want {
		t.Fatalf("counted %d, want %d", got, want)
	}
}

// THE ONE THAT KEEPS THE BILL DEFENSIBLE. A packet dropped by a full async queue
// never reaches a wire, so billing for it would charge a broadcaster for bytes we
// did not send — the hardest kind of line item to defend.
func TestADroppedPacketIsNotBilled(t *testing.T) {
	v := &Viewer{
		id:         "stalled",
		async:      true,
		queue:      make(chan viewerItem, viewerQueueSize),
		stop:       make(chan struct{}),
		videoTrack: newTestVideoTrack(t),
	}
	v.mu.Lock()
	v.billingSource = "stream-b1"
	v.mu.Unlock()
	// No writeLoop: nothing drains, so everything past the buffer is dropped.

	const overflow = 50
	for i := 0; i < viewerQueueSize+overflow; i++ {
		v.deliver("video", testPacket(1000))
	}

	if got := v.Dropped(); got != int64(overflow) {
		t.Fatalf("expected %d drops, got %d", overflow, got)
	}
	if _, bytes := v.EgressSnapshot(); bytes != 0 {
		t.Fatalf("queued-but-unwritten packets were billed (%d bytes) — nothing has "+
			"reached the transport yet, and the dropped ones never will", bytes)
	}
}

// And the counterpart: async delivery that IS drained bills exactly once, through
// the same chokepoint as sync.
func TestAsyncDeliveryBillsOncePacketsAreActuallyWritten(t *testing.T) {
	v := &Viewer{
		id:         "async",
		async:      true,
		queue:      make(chan viewerItem, viewerQueueSize),
		stop:       make(chan struct{}),
		writerDone: make(chan struct{}),
		videoTrack: newTestVideoTrack(t),
	}
	v.mu.Lock()
	v.billingSource = "stream-b1"
	v.mu.Unlock()
	go v.writeLoop()
	defer close(v.stop)

	pkt := testPacket(1000)
	v.deliver("video", pkt)

	want := int64(pkt.MarshalSize()) + egressOverheadBytes
	deadline := time.Now().Add(2 * time.Second)
	for {
		if _, got := v.EgressSnapshot(); got == want {
			return
		}
		if time.Now().After(deadline) {
			_, got := v.EgressSnapshot()
			t.Fatalf("async write billed %d, want %d", got, want)
		}
		time.Sleep(5 * time.Millisecond)
	}
}

// ── Attribution ──────────────────────────────────────────────────────────────

// An ad source id is `ad-{user}-{ts}` — it names the VIEWER, not the broadcast — so
// billing it would produce bytes attributable to nobody. Bytes sent during a break
// must bill the programme the break interrupted.
func TestOnlyAProgrammeSourceMovesBillingAttribution(t *testing.T) {
	if !isProgrammeSourceID("stream-abc123") {
		t.Fatal("a programme source must be recognised")
	}
	for _, id := range []string{"ad-user-1700000000", "", "slate-1", "streamish-1"} {
		if isProgrammeSourceID(id) {
			t.Errorf("%q must not be treated as a programme source", id)
		}
	}
}

// THE STRING CONTRACT WITH mm-core. `client.rs` registers a stream's programme as
// `stream-{id}` and hands the same value to clients as `switch_source_id`. If that
// shape changes on either side alone, every viewer's billingSource stays empty,
// every byte is attributed to nothing, and egress silently stops being metered —
// no error, no log line, just a meter reading zero.
func TestTheProgrammeSourcePrefixMatchesMmCore(t *testing.T) {
	if programmeSourcePrefix != "stream-" {
		t.Fatalf("prefix is %q; mm-core builds `format!(\"stream-{}\", stream.id)` in "+
			"client.rs and ads.rs. Changing one side alone makes egress metering "+
			"silently return zero for every broadcast", programmeSourcePrefix)
	}
}

// ── Aggregation, and surviving a disconnect ──────────────────────────────────

func TestSnapshotSumsLiveViewersBySource(t *testing.T) {
	resetClosedEgress(t)
	a := syncViewer(t, "a", "stream-b1")
	b := syncViewer(t, "b", "stream-b1")
	c := syncViewer(t, "c", "stream-b2")
	pkt := testPacket(100)
	per := int64(pkt.MarshalSize()) + egressOverheadBytes

	a.writeTrack("video", pkt)
	b.writeTrack("video", pkt)
	b.writeTrack("video", pkt)
	c.writeTrack("video", pkt)

	rep := egressSnapshot([]*Viewer{a, b, c})
	got := map[string]int64{}
	for _, s := range rep.Sources {
		got[s.Source] = s.Bytes
	}
	if got["stream-b1"] != 3*per {
		t.Errorf("stream-b1 = %d, want %d", got["stream-b1"], 3*per)
	}
	if got["stream-b2"] != per {
		t.Errorf("stream-b2 = %d, want %d", got["stream-b2"], per)
	}
}

// A viewer disconnecting is the NORMAL end of its life, not an exception. Without
// folding its counter into a running total, the meter would only ever show bytes for
// people who happened to still be watching when mm-core polled.
func TestADepartedViewersBytesSurviveInTheTotal(t *testing.T) {
	resetClosedEgress(t)
	v := syncViewer(t, "gone", "stream-b1")
	pkt := testPacket(500)
	per := int64(pkt.MarshalSize()) + egressOverheadBytes
	v.writeTrack("video", pkt)

	// What removeViewer does after Close.
	src, bytes := v.EgressSnapshot()
	recordClosedViewerEgress(src, bytes)

	rep := egressSnapshot(nil) // nobody live
	if len(rep.Sources) != 1 || rep.Sources[0].Source != "stream-b1" || rep.Sources[0].Bytes != per {
		t.Fatalf("a departed viewer's bytes were lost: %+v", rep.Sources)
	}
}

func TestClosedAndLiveTotalsAddRatherThanReplace(t *testing.T) {
	resetClosedEgress(t)
	pkt := testPacket(500)
	per := int64(pkt.MarshalSize()) + egressOverheadBytes

	recordClosedViewerEgress("stream-b1", per)
	live := syncViewer(t, "live", "stream-b1")
	live.writeTrack("video", pkt)

	rep := egressSnapshot([]*Viewer{live})
	if len(rep.Sources) != 1 || rep.Sources[0].Bytes != 2*per {
		t.Fatalf("closed and live totals must add, got %+v", rep.Sources)
	}
}

func TestAViewerWithNoBillingSourceIsNotCounted(t *testing.T) {
	resetClosedEgress(t)
	// A viewer that has never been attached to a programme — its bytes belong to no
	// broadcast, and inventing one would bill the wrong wallet.
	v := syncViewer(t, "unattached", "")
	v.writeTrack("video", testPacket(500))

	rep := egressSnapshot([]*Viewer{v})
	if len(rep.Sources) != 0 {
		t.Fatalf("bytes with no billable source must be omitted, not attributed: %+v", rep.Sources)
	}
}

// ── The epoch, which is what makes a delta safe ───────────────────────────────

// mm-core subtracts two readings to get usage. It may only do that when the epoch
// matches — otherwise the process restarted, the counters went to zero, and the
// difference is either negative or a spurious total.
func TestEveryReadingCarriesTheSameEpochWithinAProcess(t *testing.T) {
	resetClosedEgress(t)
	first := egressSnapshot(nil)
	second := egressSnapshot(nil)

	if first.Epoch == "" {
		t.Fatal("an empty epoch would let a reader subtract across a restart")
	}
	if first.Epoch != second.Epoch {
		t.Fatalf("the epoch changed within one process (%q -> %q); every delta would "+
			"be discarded as if the node had restarted", first.Epoch, second.Epoch)
	}
	if first.Since.IsZero() {
		t.Fatal("a reading must say how much history it covers")
	}
	if first.OverheadBytesPerPacket != egressOverheadBytes {
		t.Fatal("a reading must state the overhead it included, or it cannot be " +
			"recomputed after recalibration")
	}
}

func TestTwoEpochsAreNeverEqual(t *testing.T) {
	// The epoch's whole job is uniqueness across process lifetimes. A collision means
	// mm-core subtracts across a restart and bills the difference as usage.
	seen := map[string]bool{}
	for i := 0; i < 1000; i++ {
		e := newEgressEpoch()
		if seen[e] {
			t.Fatalf("epoch collision at iteration %d: %q", i, e)
		}
		seen[e] = true
	}
}

// ── Overhead configuration ───────────────────────────────────────────────────

func TestOverheadDefaultsWhenUnset(t *testing.T) {
	t.Setenv(egressOverheadEnv, "")
	if got := loadEgressOverheadBytes(); got != defaultEgressOverheadBytes {
		t.Fatalf("got %d, want the default %d", got, defaultEgressOverheadBytes)
	}
}

func TestOverheadIsConfigurableForCalibration(t *testing.T) {
	// The default is an estimate; the real figure comes from comparing against the
	// provider's own egress counter on the first node.
	t.Setenv(egressOverheadEnv, "54")
	if got := loadEgressOverheadBytes(); got != 54 {
		t.Fatalf("got %d, want 54", got)
	}
	t.Setenv(egressOverheadEnv, "0")
	if got := loadEgressOverheadBytes(); got != 0 {
		t.Fatalf("an explicit zero must be honoured (some transports really are 0), got %d", got)
	}
}

// A bad value must not make the meter lie AND must not stop a node serving. It
// falls back loudly: an unparseable overhead is a config error, not a reason to
// refuse to fan out video.
func TestABadOverheadValueFallsBackLoudly(t *testing.T) {
	var logged string
	orig := logEgressOverheadProblem
	logEgressOverheadProblem = func(raw string) { logged = raw }
	defer func() { logEgressOverheadProblem = orig }()

	for _, bad := range []string{"abc", "-1", "1.5"} {
		logged = ""
		t.Setenv(egressOverheadEnv, bad)
		if got := loadEgressOverheadBytes(); got != defaultEgressOverheadBytes {
			t.Errorf("%q gave %d, want the default", bad, got)
		}
		if logged != bad {
			t.Errorf("%q was not reported (logged %q) — a silently wrong overhead is a "+
				"systematic billing error nobody would notice", bad, logged)
		}
	}
}

// resetClosedEgress clears the process-wide closed-viewer totals between tests.
// Go runs tests in one process, so without this each test inherits the last one's
// bytes and the aggregation assertions become order-dependent.
func resetClosedEgress(t *testing.T) {
	t.Helper()
	closedEgress.Lock()
	closedEgress.bytes = make(map[string]int64)
	closedEgress.Unlock()
}

func newTestAudioTrack(t *testing.T) *webrtc.TrackLocalStaticRTP {
	t.Helper()
	tr, err := webrtc.NewTrackLocalStaticRTP(
		webrtc.RTPCodecCapability{MimeType: webrtc.MimeTypeOpus}, "audio", "mm-test",
	)
	if err != nil {
		t.Fatalf("track: %v", err)
	}
	return tr
}
