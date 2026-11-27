// SPDX-License-Identifier: Apache-2.0

package main

import (
	"io"
	"sync"
	"testing"
	"time"

	"github.com/pion/interceptor"
	"github.com/pion/rtcp"
	"github.com/pion/rtp"
	"github.com/pion/webrtc/v4"
	"github.com/prometheus/client_golang/prometheus/testutil"
)

const harnessSSRC = 0x51A7

// resendHarness plays a viewer against the interceptor chain every mm-switch
// PeerConnection is built with. Packets written to it are the programme; ask()
// is the viewer saying "I did not get these, send them again"; the harness
// counts how often each sequence number reaches the network.
type resendHarness struct {
	t      *testing.T
	writer interceptor.RTPWriter
	reader interceptor.RTCPReader
	rtcpIn chan []byte

	mu   sync.Mutex
	wire map[uint16]int
}

func newResendHarness(t *testing.T) *resendHarness {
	t.Helper()
	m := &webrtc.MediaEngine{}
	if err := m.RegisterDefaultCodecs(); err != nil {
		t.Fatal(err)
	}
	registry, err := newInterceptorRegistry(m)
	if err != nil {
		t.Fatal(err)
	}
	chain, err := registry.Build("resend-harness")
	if err != nil {
		t.Fatal(err)
	}

	h := &resendHarness{t: t, rtcpIn: make(chan []byte, 64), wire: map[uint16]int{}}
	info := &interceptor.StreamInfo{
		SSRC:         harnessSSRC,
		ClockRate:    90000,
		MimeType:     webrtc.MimeTypeVP8,
		PayloadType:  96,
		RTCPFeedback: []interceptor.RTCPFeedback{{Type: "nack"}},
	}
	h.writer = chain.BindLocalStream(info, interceptor.RTPWriterFunc(
		func(hdr *rtp.Header, payload []byte, _ interceptor.Attributes) (int, error) {
			h.mu.Lock()
			h.wire[hdr.SequenceNumber]++
			h.mu.Unlock()
			return len(payload), nil
		}))
	h.reader = chain.BindRTCPReader(interceptor.RTCPReaderFunc(
		func(b []byte, a interceptor.Attributes) (int, interceptor.Attributes, error) {
			pkt, ok := <-h.rtcpIn
			if !ok {
				return 0, nil, io.EOF
			}
			return copy(b, pkt), a, nil
		}))
	t.Cleanup(func() {
		close(h.rtcpIn)
		chain.UnbindLocalStream(info)
		_ = chain.Close()
	})
	return h
}

// send writes programme packets with these sequence numbers.
func (h *resendHarness) send(seqs ...uint16) {
	h.t.Helper()
	for _, seq := range seqs {
		hdr := &rtp.Header{Version: 2, PayloadType: 96, SSRC: harnessSSRC, SequenceNumber: seq, Timestamp: uint32(seq) * 3000}
		if _, err := h.writer.Write(hdr, []byte{0x90, 0x80, byte(seq)}, nil); err != nil {
			h.t.Fatalf("send %d: %v", seq, err)
		}
	}
}

// ask delivers one resend request from the viewer for these sequence numbers.
func (h *resendHarness) ask(seqs ...uint16) {
	h.t.Helper()
	raw, err := (&rtcp.TransportLayerNack{
		SenderSSRC: 1,
		MediaSSRC:  harnessSSRC,
		Nacks:      rtcp.NackPairsFromSequenceNumbers(seqs),
	}).Marshal()
	if err != nil {
		h.t.Fatal(err)
	}
	h.rtcpIn <- raw
	if _, _, err := h.reader.Read(make([]byte, 1500), nil); err != nil {
		h.t.Fatalf("read rtcp: %v", err)
	}
}

// resends reports how many times each packet went out again after its first
// transmission. Resends leave on their own goroutines, so it waits for the
// counts to stop moving first.
func (h *resendHarness) resends() map[uint16]int {
	h.t.Helper()
	total := func() int {
		h.mu.Lock()
		defer h.mu.Unlock()
		n := 0
		for _, c := range h.wire {
			n += c
		}
		return n
	}
	last, stableSince := total(), time.Now()
	for deadline := time.Now().Add(3 * time.Second); time.Now().Before(deadline); {
		time.Sleep(10 * time.Millisecond)
		if now := total(); now != last {
			last, stableSince = now, time.Now()
		} else if time.Since(stableSince) > 100*time.Millisecond {
			break
		}
	}
	h.mu.Lock()
	defer h.mu.Unlock()
	out := make(map[uint16]int, len(h.wire))
	for seq, c := range h.wire {
		out[seq] = c - 1
	}
	return out
}

func seqRange(from, n uint16) []uint16 {
	out := make([]uint16, n)
	for i := range out {
		out[i] = from + uint16(i)
	}
	return out
}

// The stage load test of 2026-11-26: bots on a lossy link asked for the same packets
// every 100 ms, the switch answered every request in full, and 138 Mbps of programme
// became about 1 Gbps on the wire. A packet may be sent again, but not once per request.
func TestAPacketAskedForTwentyTimesIsNotSentTwentyTimes(t *testing.T) {
	h := newResendHarness(t)
	h.send(seqRange(100, 50)...)

	for i := 0; i < 20; i++ {
		h.ask(seqRange(100, 50)...)
	}

	for seq, n := range h.resends() {
		if n < 1 {
			t.Errorf("packet %d was never sent again: the first request must still be answered", seq)
		}
		if n > 3 {
			t.Errorf("packet %d was sent again %d times for 20 requests, want at most 3", seq, n)
		}
	}
}

// Ordinary repair must survive the limit: a viewer that lost five packets and asks once
// gets exactly those five, once.
func TestASingleRequestForLostPacketsIsAnsweredOnce(t *testing.T) {
	h := newResendHarness(t)
	h.send(seqRange(100, 50)...)

	h.ask(103, 110, 111, 127, 149)

	got := h.resends()
	for seq := uint16(100); seq < 150; seq++ {
		want := 0
		switch seq {
		case 103, 110, 111, 127, 149:
			want = 1
		}
		if got[seq] != want {
			t.Errorf("packet %d was sent again %d times, want %d", seq, got[seq], want)
		}
	}
}

// ── The limit's rules, on a clock the test controls ──────────────────────────

type fakeClock struct{ t time.Time }

func (c *fakeClock) now() time.Time          { return c.t }
func (c *fakeClock) advance(d time.Duration) { c.t = c.t.Add(d) }

func newTestLimiter() (*resendLimiter, *fakeClock) {
	clock := &fakeClock{t: time.Unix(1_700_000_000, 0)}
	return newResendLimiter(0.25, clock.now), clock
}

// LiveKit's rule (pkg/sfu/sequencer.go, maxAck = 3): however long a viewer keeps
// asking, one packet goes out again three times and no more.
func TestAFourthResendOfTheSamePacketIsRefused(t *testing.T) {
	l, clock := newTestLimiter()
	l.noteSent(7)

	var got []bool
	for i := 0; i < 5; i++ {
		got = append(got, l.allow(7))
		clock.advance(150 * time.Millisecond)
	}

	want := []bool{true, true, true, false, false}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("request %d: allowed=%v, want %v (all: %v)", i+1, got[i], want[i], got)
		}
	}
}

// A second request for a packet that was sent again 20 ms ago is the same loss
// report arriving twice, not a second loss.
func TestARepeatRequestInsideTheSpacingIsRefused(t *testing.T) {
	l, clock := newTestLimiter()
	l.noteSent(7)

	first := l.allow(7)
	clock.advance(20 * time.Millisecond)
	tooSoon := l.allow(7)
	clock.advance(100 * time.Millisecond)
	later := l.allow(7)

	if !first || tooSoon || !later {
		t.Fatalf("first=%v after 20ms=%v after 120ms=%v, want true false true", first, tooSoon, later)
	}
}

// The case the per-packet rules cannot catch: a viewer (or a link) that loses
// everything asks for every packet exactly once. Resends must stay a fraction of
// the programme. At 250 packets a second and a 25% share that is 125 resends in
// two seconds; before the limit it was all 500.
func TestResendsStayWithinAQuarterOfTheProgramme(t *testing.T) {
	l, clock := newTestLimiter()
	seq := uint16(0)
	playOneSecond := func() (allowed int) {
		for i := 0; i < 250; i++ {
			l.noteSent(seq)
			if l.allow(seq) {
				allowed++
			}
			seq++
			clock.advance(4 * time.Millisecond)
		}
		return allowed
	}
	playOneSecond() // the limiter learns the programme rate
	playOneSecond() // and spends whatever start-up allowance it had

	got := playOneSecond() + playOneSecond()

	if got < 120 || got > 130 {
		t.Fatalf("%d of 500 packets were sent again in two seconds, want about 125 (a quarter)", got)
	}
}

// Requests for packets that were never sent cost nothing: they must not use up
// the allowance a real loss needs.
func TestRequestsForPacketsNeverSentAreRefusedAndCostNothing(t *testing.T) {
	l, _ := newTestLimiter()
	for _, seq := range seqRange(100, 10) {
		l.noteSent(seq)
	}

	for i := 0; i < 1000; i++ {
		if l.allow(uint16(5000 + i)) {
			t.Fatalf("packet %d was never sent but a resend was allowed", 5000+i)
		}
	}

	for _, seq := range seqRange(100, 10) {
		if !l.allow(seq) {
			t.Fatalf("packet %d: resend refused after a flood of bogus requests", seq)
		}
	}
}

// pion keeps the last 1024 packets to resend from. A request for something older
// cannot be served, so it must not be let through (or counted against the viewer).
func TestAPacketOlderThanTheResendBufferIsRefused(t *testing.T) {
	l, _ := newTestLimiter()
	for _, seq := range seqRange(0, 2000) {
		l.noteSent(seq)
	}

	if l.allow(0) {
		t.Fatal("packet 0 is 2000 packets old, beyond the 1024-packet buffer, but a resend was allowed")
	}
	if !l.allow(1999) {
		t.Fatal("packet 1999 was just sent but its resend was refused")
	}
}

// Sequence numbers wrap at 65535. Packets after the wrap are still programme and
// still repairable.
func TestRepairStillWorksAcrossSequenceWrap(t *testing.T) {
	l, _ := newTestLimiter()
	for _, seq := range []uint16{65533, 65534, 65535, 0, 1, 2} {
		l.noteSent(seq)
	}

	for _, seq := range []uint16{65534, 0, 2} {
		if !l.allow(seq) {
			t.Fatalf("packet %d: resend refused around the sequence wrap", seq)
		}
	}
}

// Without RTX a resend leaves with its original sequence number, through the same
// writer as the programme. If the limiter took it for a new packet it would forget
// how often that packet had already gone out, and the three-resend rule would never
// bite.
func TestAResendGoingOutIsNotMistakenForANewPacket(t *testing.T) {
	l, clock := newTestLimiter()
	l.noteSent(7)

	allowed := 0
	for i := 0; i < 6; i++ {
		if l.allow(7) {
			allowed++
			l.noteSent(7) // the resend itself passing the writer
		}
		clock.advance(150 * time.Millisecond)
	}

	if allowed != 3 {
		t.Fatalf("packet 7 was sent again %d times over six requests, want 3", allowed)
	}
}

// A storm must be visible. Programme egress is counted elsewhere and resends are not,
// so before this counter the only sign of one was the network interface.
func TestRefusedAndAllowedResendRequestsAreCounted(t *testing.T) {
	allowedBefore := testutil.ToFloat64(resendRequestsTotal.WithLabelValues("allowed"))
	limitedBefore := testutil.ToFloat64(resendRequestsTotal.WithLabelValues("limited"))
	h := newResendHarness(t)
	h.send(seqRange(100, 10)...)

	h.ask(seqRange(100, 10)...) // 10 answered
	h.ask(seqRange(100, 10)...) // 10 refused: asked again within the spacing
	h.ask(9000, 9001)           // 2 refused: never sent
	h.resends()

	allowed := testutil.ToFloat64(resendRequestsTotal.WithLabelValues("allowed")) - allowedBefore
	limited := testutil.ToFloat64(resendRequestsTotal.WithLabelValues("limited")) - limitedBefore
	if allowed != 10 || limited != 12 {
		t.Fatalf("counted allowed=%v limited=%v, want 10 and 12", allowed, limited)
	}
}

// MM_SWITCH_RESEND_BUDGET_PERCENT sets the share of the programme that may be sent
// again. A value that is not a whole number from 0 to 100 must leave the default in
// force: a typo in a safety limit must neither stop the switch nor switch the limit off.
func TestResendShareFromEnvironment(t *testing.T) {
	for _, tc := range []struct {
		env  string
		want float64
	}{
		{"", 0.25},
		{"50", 0.50},
		{"100", 1.00},
		{"0", 0.00},
		{"abc", 0.25},
		{"-5", 0.25},
		{"250", 0.25},
		{"12.5", 0.25},
	} {
		t.Setenv("MM_SWITCH_RESEND_BUDGET_PERCENT", tc.env)
		if got := loadResendShare(); got != tc.want {
			t.Errorf("MM_SWITCH_RESEND_BUDGET_PERCENT=%q: share %v, want %v", tc.env, got, tc.want)
		}
	}
}
