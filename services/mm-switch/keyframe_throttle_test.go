package main

import (
	"fmt"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/pion/rtcp"
	"github.com/pion/rtp"
	"github.com/pion/webrtc/v4"
	"github.com/prometheus/client_golang/prometheus/testutil"
)

// Every keyframe request to a publisher (viewers passing on their phone's PLI/FIR, the
// recorder, Subscribe's burst) goes through one throttle per source. These tests run a
// real publisher PeerConnection (see newPublisherRigSending) and count the PLIs that
// reach it on the wire.

// fakeEncoder stands in for the broadcaster's encoder: it counts the PLIs that arrive
// and, if it answers them, makes its next frame a keyframe. Every other frame is a delta.
type fakeEncoder struct {
	answers bool
	keyNext atomic.Bool

	mu   sync.Mutex
	plis []time.Time
}

func (e *fakeEncoder) payload() []byte {
	if e.keyNext.Swap(false) {
		return []byte{0x10, 0x00, 0x00, 0x00} // S bit, then a VP8 frame tag with P clear
	}
	return []byte{0x10, 0x01, 0x00, 0x00}
}

func (e *fakeEncoder) readRTCP(sender *webrtc.RTPSender) {
	for {
		pkts, _, err := sender.ReadRTCP()
		if err != nil {
			return
		}
		for _, p := range pkts {
			if _, ok := p.(*rtcp.PictureLossIndication); !ok {
				continue
			}
			e.mu.Lock()
			e.plis = append(e.plis, time.Now())
			e.mu.Unlock()
			if e.answers {
				e.keyNext.Store(true)
			}
		}
	}
}

func (e *fakeEncoder) pliTimes() []time.Time {
	e.mu.Lock()
	defer e.mu.Unlock()
	return append([]time.Time(nil), e.plis...)
}

func (e *fakeEncoder) pliCount() int { return len(e.pliTimes()) }

// newKeyframeRig connects a publisher whose encoder answers PLIs with a keyframe (or,
// with answers false, never sends one) to a fresh, active WebRTCSource.
func newKeyframeRig(t *testing.T, answers bool) (*publisherRig, *fakeEncoder) {
	t.Helper()
	enc := &fakeEncoder{answers: answers}
	// A long ICE failed timeout: nothing here cuts the network, so the publisher must not
	// be declared gone by a slow race-detector run.
	rig := newPublisherRigSending(t, 5*time.Second, enc.payload)
	go enc.readRTCP(rig.pubPC.GetSenders()[0])
	return rig, enc
}

// keyframeCollector is a subscriber that counts the keyframes fanned out to it.
type keyframeCollector struct{ keyframes atomic.Int64 }

func (c *keyframeCollector) handle(kind string, pkt *rtp.Packet) {
	if kind == "video" && IsVP8Keyframe(pkt.Payload) {
		c.keyframes.Add(1)
	}
}

func keyframeRequests(result string) float64 {
	return testutil.ToFloat64(sourceKeyframeRequestsTotal.WithLabelValues(result))
}

// N stuck viewers asking at once cost the publisher one PLI per window, not N.
func TestConcurrentKeyframeRequestsSendOnePLIPerWindow(t *testing.T) {
	rig, enc := newKeyframeRig(t, false)
	sent, throttled, failed := keyframeRequests("sent"), keyframeRequests("throttled"), keyframeRequests("failed")

	const callers = 20
	start := make(chan struct{})
	var wg sync.WaitGroup
	for i := 0; i < callers; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			<-start
			rig.src.RequestKeyframe()
		}()
	}
	began := time.Now()
	close(start)
	wg.Wait()
	if took := time.Since(began); took >= keyframeRequestInterval {
		t.Fatalf("%d requests took %s, more than one throttle window: the test proves nothing", callers, took)
	}

	waitUntil(t, rigWait, "the PLI to reach the publisher", func() bool { return enc.pliCount() >= 1 })
	time.Sleep(keyframeRequestInterval / 2) // room for any stray second PLI to land
	if n := enc.pliCount(); n != 1 {
		t.Fatalf("publisher got %d PLIs for %d concurrent requests, want 1", n, callers)
	}
	if d := keyframeRequests("sent") - sent; d != 1 {
		t.Fatalf(`keyframe requests{result="sent"} grew by %v, want 1`, d)
	}
	if d := keyframeRequests("throttled") - throttled; d != callers-1 {
		t.Fatalf(`keyframe requests{result="throttled"} grew by %v, want %d`, d, callers-1)
	}

	// The window reopens: the next request goes out.
	time.Sleep(keyframeRequestInterval)
	rig.src.RequestKeyframe()
	waitUntil(t, rigWait, "the second window's PLI", func() bool { return enc.pliCount() == 2 })

	// A source whose publisher is gone asks nothing of it, not even a throttled request
	// (so no write error to log for every stuck viewer).
	sent, throttled = keyframeRequests("sent"), keyframeRequests("throttled")
	rig.src.connectionStateChanged(webrtc.PeerConnectionStateClosed)
	rig.src.RequestKeyframe()
	if keyframeRequests("sent") != sent || keyframeRequests("throttled") != throttled || keyframeRequests("failed") != failed {
		t.Fatal("a gone source still went through the keyframe throttle")
	}
}

// A source switch re-subscribes every viewer at once. Their bursts share the throttle, so
// the publisher sees one PLI, and every burst stops at the keyframe it produced.
func TestSimultaneousSubscribersShareOnePLI(t *testing.T) {
	rig, enc := newKeyframeRig(t, true)

	const viewers = 10
	collectors := make([]*keyframeCollector, viewers)
	start := make(chan struct{})
	var wg sync.WaitGroup
	for i := range collectors {
		c := &keyframeCollector{}
		collectors[i] = c
		wg.Add(1)
		go func(id string) {
			defer wg.Done()
			<-start
			rig.src.Subscribe(id, c.handle)
		}(fmt.Sprintf("viewer-%d", i))
	}
	close(start)
	wg.Wait()

	waitUntil(t, rigWait, "every subscriber to receive a keyframe", func() bool {
		for _, c := range collectors {
			if c.keyframes.Load() == 0 {
				return false
			}
		}
		return true
	})
	// Every burst has passed its next attempt by now: each saw the keyframe and stopped.
	time.Sleep(2 * keyframeRequestInterval)
	if n := enc.pliCount(); n != 1 {
		t.Fatalf("publisher got %d PLIs for %d simultaneous subscribers, want 1", n, viewers)
	}
}

// A subscriber that joins just after a PLI went out (its keyframe already fanned out to
// the others) has its first attempt throttled. The next attempt, a full window later,
// gets through, so it still receives a keyframe.
func TestSubscriberJoiningInsideTheWindowStillGetsAKeyframe(t *testing.T) {
	rig, enc := newKeyframeRig(t, true)

	first := &keyframeCollector{}
	rig.src.Subscribe("first", first.handle)
	waitUntil(t, rigWait, "the first subscriber's keyframe", func() bool { return first.keyframes.Load() >= 1 })

	sent, throttled := keyframeRequests("sent"), keyframeRequests("throttled")
	late := &keyframeCollector{}
	rig.src.Subscribe("late", late.handle)
	waitUntil(t, rigWait, "the late subscriber's keyframe", func() bool { return late.keyframes.Load() >= 1 })

	if d := keyframeRequests("throttled") - throttled; d != 1 {
		t.Fatalf(`keyframe requests{result="throttled"} grew by %v, want 1: the late subscriber's first attempt should have fallen inside the window`, d)
	}
	if d := keyframeRequests("sent") - sent; d != 1 {
		t.Fatalf(`keyframe requests{result="sent"} grew by %v, want 1`, d)
	}
	time.Sleep(keyframeRequestInterval) // the late burst's next check: it stops
	if n := enc.pliCount(); n != 2 {
		t.Fatalf("publisher got %d PLIs, want 2 (one per subscriber)", n)
	}
}

// When no keyframe comes back (the PLI or the keyframe lost, or a publisher that ignores
// PLI), the burst keeps asking, every attempt a full throttle window after the last, so
// none is swallowed by the throttle, and then stops.
func TestSubscribeBurstRetriesOncePerWindowUntilItGivesUp(t *testing.T) {
	rig, enc := newKeyframeRig(t, false)
	rig.src.Subscribe("viewer", func(string, *rtp.Packet) {})

	waitUntil(t, rigWait, "the whole burst", func() bool { return enc.pliCount() >= subscribeKeyframeAttempts })
	time.Sleep(keyframeRequestInterval * 3 / 2) // past where a sixth attempt would be
	times := enc.pliTimes()
	if len(times) != subscribeKeyframeAttempts {
		t.Fatalf("burst sent %d PLIs, want %d", len(times), subscribeKeyframeAttempts)
	}
	for i := 1; i < len(times); i++ {
		// Measured where the publisher reads them, so allow some scheduling jitter.
		if gap := times[i].Sub(times[i-1]); gap < keyframeRequestInterval*3/4 {
			t.Fatalf("PLIs %d and %d arrived %s apart, want about %s", i, i+1, gap, keyframeRequestInterval)
		}
	}
}
