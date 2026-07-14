package main

import (
	"testing"
	"time"

	"github.com/pion/rtp"
	"github.com/pion/webrtc/v4"
)

// The whole point of async delivery: a viewer whose transport is wedged must not stall
// the source's fan-out goroutine, because that goroutine serves EVERY other viewer and
// the recorder too. Here the viewer is stalled in the most total way possible — nothing
// ever drains its queue — and `deliver` must still return promptly, shedding packets.
func TestAsyncDeliverNeverBlocksOnAStalledViewer(t *testing.T) {
	v := &Viewer{
		id:         "stalled",
		async:      true,
		queue:      make(chan viewerItem, viewerQueueSize),
		stop:       make(chan struct{}),
		videoTrack: newTestVideoTrack(t),
	}
	// Deliberately NO writeLoop: this viewer never consumes anything.

	const overflow = 200
	total := viewerQueueSize + overflow

	done := make(chan struct{})
	go func() {
		defer close(done)
		for i := 0; i < total; i++ {
			v.deliver("video", &rtp.Packet{})
		}
	}()

	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("deliver() blocked on a stalled viewer — the fan-out goroutine would be " +
			"stuck, starving every other viewer and the recorder")
	}

	if got := v.Dropped(); got != int64(overflow) {
		t.Fatalf("expected exactly %d dropped packets once the queue filled, got %d",
			overflow, got)
	}
	if len(v.queue) != viewerQueueSize {
		t.Fatalf("queue should be full at %d, holds %d", viewerQueueSize, len(v.queue))
	}
}

// The writer goroutine must actually drain the queue, in order, so that a healthy viewer
// sees every packet. Without a real PeerConnection the tracks are unbound and WriteRTP is
// a no-op, so observe the drain through the queue depth instead.
func TestAsyncWriteLoopDrainsTheQueue(t *testing.T) {
	v := &Viewer{
		id:         "healthy",
		async:      true,
		queue:      make(chan viewerItem, viewerQueueSize),
		stop:       make(chan struct{}),
		writerDone: make(chan struct{}),
		videoTrack: newTestVideoTrack(t),
	}
	go v.writeLoop()

	for i := 0; i < 50; i++ {
		v.deliver("video", &rtp.Packet{})
	}

	deadline := time.After(2 * time.Second)
	for len(v.queue) > 0 {
		select {
		case <-deadline:
			t.Fatalf("writeLoop did not drain the queue; %d packets still pending", len(v.queue))
		default:
			time.Sleep(5 * time.Millisecond)
		}
	}
	if got := v.Dropped(); got != 0 {
		t.Fatalf("a drained queue must drop nothing, dropped %d", got)
	}

	close(v.stop)
	select {
	case <-v.writerDone:
	case <-time.After(2 * time.Second):
		t.Fatal("writeLoop ignored the stop signal — goroutine leak per viewer")
	}
}

// Async delivery changes the live media path, so it must stay opt-in: an operator who
// sets nothing keeps the synchronous behaviour production has been running.
func TestAsyncViewersAreOffByDefault(t *testing.T) {
	t.Setenv(asyncViewersEnv, "")
	if asyncViewersEnabled() {
		t.Fatal("async viewer delivery must default to OFF")
	}
	t.Setenv(asyncViewersEnv, "true")
	if !asyncViewersEnabled() {
		t.Fatalf("%s=true must enable async delivery", asyncViewersEnv)
	}
	t.Setenv(asyncViewersEnv, "1")
	if !asyncViewersEnabled() {
		t.Fatalf("%s=1 must enable async delivery", asyncViewersEnv)
	}
}

// Sync mode must not touch the queue at all — the flag-off path has to be exactly the
// code that ran before.
func TestSyncDeliverDoesNotEnqueue(t *testing.T) {
	track := newTestVideoTrack(t)
	v := &Viewer{id: "sync", async: false, videoTrack: track}

	// Unbound track: WriteRTP returns an error rather than blocking. We only care that
	// deliver took the inline path and never went near a (nil) queue.
	v.deliver("video", &rtp.Packet{})

	if v.Dropped() != 0 {
		t.Fatal("sync mode must never count a drop")
	}
}

func newTestVideoTrack(t *testing.T) *webrtc.TrackLocalStaticRTP {
	t.Helper()
	tr, err := webrtc.NewTrackLocalStaticRTP(
		webrtc.RTPCodecCapability{MimeType: webrtc.MimeTypeVP8}, "video", "mm-test",
	)
	if err != nil {
		t.Fatalf("track: %v", err)
	}
	return tr
}
