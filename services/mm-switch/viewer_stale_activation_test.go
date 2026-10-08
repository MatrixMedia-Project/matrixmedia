package main

import (
	"sync"
	"testing"
	"time"

	"github.com/pion/webrtc/v4"
)

// gatedSource counts its live subscriptions. With a gate, Subscribe registers the handler
// and then blocks until the test opens the gate: a source slow to return from Subscribe,
// as FileSource is while it replays its cached keyframe inline. That holds an activation
// in the window where it has released v.mu but not yet stored its unsubscribe.
type gatedSource struct {
	name    string
	entered chan struct{} // closed when Subscribe is called
	gate    chan struct{} // nil: Subscribe returns at once

	mu   sync.Mutex
	subs int
}

func newGatedSource(name string, gated bool) *gatedSource {
	s := &gatedSource{name: name, entered: make(chan struct{})}
	if gated {
		s.gate = make(chan struct{})
	}
	return s
}

func (s *gatedSource) String() string   { return s.name }
func (s *gatedSource) Type() string     { return "webrtc" }
func (s *gatedSource) IsActive() bool   { return true }
func (s *gatedSource) RequestKeyframe() {}
func (s *gatedSource) Stop()            {}
func (s *gatedSource) Subscribe(id string, h PacketHandler) func() {
	s.mu.Lock()
	s.subs++
	s.mu.Unlock()
	if s.gate != nil {
		close(s.entered)
		<-s.gate
	}
	var once sync.Once
	return func() {
		once.Do(func() {
			s.mu.Lock()
			s.subs--
			s.mu.Unlock()
		})
	}
}

func (s *gatedSource) live() int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.subs
}

func activeSourceOf(v *Viewer) Source {
	v.mu.RLock()
	defer v.mu.RUnlock()
	return v.activeSource
}

// stallActivation activates src the way OnConnectionStateChange does and returns once that
// activation is stuck inside src.Subscribe with v.mu released. resume lets Subscribe return
// and waits for the activation to finish.
func stallActivation(t *testing.T, v *Viewer, id string, src *gatedSource) (resume func()) {
	t.Helper()
	done := make(chan struct{})
	go func() {
		defer close(done)
		attach(v, id, src)
	}()
	select {
	case <-src.entered:
	case <-done:
		t.Fatalf("activating %s returned without subscribing", id)
	case <-time.After(2 * time.Second):
		t.Fatalf("activating %s never reached Subscribe", id)
	}
	return func() {
		t.Helper()
		close(src.gate)
		select {
		case <-done:
		case <-time.After(2 * time.Second):
			t.Fatalf("activating %s never returned after Subscribe did", id)
		}
	}
}

// A viewer switching sources twice in quick succession must end up subscribed to the second
// one only. The first activation subscribes with v.mu released; if the second switch and
// its activation finish in that window, the first must not store its unsubscribe over the
// second's. That overwrite lost the only handle on the second subscription: the viewer got
// packets from both sources for good, and activeSource no longer matched currentSource.
func TestSwitchingDuringASlowSubscribeKeepsOnlyTheNewSource(t *testing.T) {
	v := capturingViewer(t, "late-activation", 0)
	v.connected = true

	b := newGatedSource("b", true)
	resume := stallActivation(t, v, "b", b)

	c := newGatedSource("c", false)
	v.SwitchTo("c", c)
	waitUntil(t, 2*time.Second, "c's activation", func() bool { return activeSourceOf(v) == c })

	resume()

	if n := b.live(); n != 0 {
		t.Errorf("the viewer switched away from b while b's Subscribe was running, "+
			"yet b still has %d subscription(s) to it", n)
	}
	if got := activeSourceOf(v); got != c {
		t.Errorf("activeSource is %v, want c (currentSource is %q)", got, v.CurrentSourceID())
	}
	v.DetachSource()
	if n := c.live(); n != 0 {
		t.Errorf("c's subscription leaked: %d still live after DetachSource, because b's late "+
			"activation overwrote the unsubscribe that would have removed it", n)
	}
}

// The same window, but the viewer moves on some other way. The late Subscribe must be
// undone rather than stored: after Close nothing would ever remove it, and after
// DetachSource or a switch the viewer would keep receiving a source it has left.
func TestALateSubscribeIsUndoneWhenTheViewerHasMovedOn(t *testing.T) {
	cases := []struct {
		name   string
		moveOn func(t *testing.T, v *Viewer)
	}{
		{"SwitchTo before the new source activates", func(t *testing.T, v *Viewer) {
			// Not connected, so SwitchTo's goroutine is still waiting to activate c when b's
			// Subscribe returns. Let it finish before the test ends.
			c := newGatedSource("c", false)
			v.SwitchTo("c", c)
			t.Cleanup(func() {
				v.mu.Lock()
				v.connected = true
				v.mu.Unlock()
				waitUntil(t, 2*time.Second, "c's activation", func() bool { return activeSourceOf(v) == c })
			})
		}},
		{"DetachSource", func(t *testing.T, v *Viewer) {
			v.DetachSource()
		}},
		{"Close", func(t *testing.T, v *Viewer) {
			pc, err := webrtc.NewPeerConnection(webrtc.Configuration{})
			if err != nil {
				t.Fatal(err)
			}
			v.pc = pc
			v.writerDone = make(chan struct{})
			go v.writeLoop()
			v.Close()
		}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			v := capturingViewer(t, "late-activation", 0)
			b := newGatedSource("b", true)
			resume := stallActivation(t, v, "b", b)

			tc.moveOn(t, v)
			resume()

			if n := b.live(); n != 0 {
				t.Errorf("b's Subscribe returned after %s, and b still has %d subscription(s)", tc.name, n)
			}
			if got := activeSourceOf(v); got != nil {
				t.Errorf("activeSource is %v after %s, want nil", got, tc.name)
			}
		})
	}
}
