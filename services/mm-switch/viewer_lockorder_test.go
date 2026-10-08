package main

import (
	"sync"
	"testing"
	"time"

	"github.com/pion/rtp"
)

// lockingSource locks the way WebRTCSource does: the fan-out calls every handler while
// holding the source's READ lock, and unsubscribe takes its WRITE lock. With raceFanout
// set, unsubscribe first starts a fan-out and waits until it holds the read lock — a
// packet arriving at the exact moment a viewer leaves, which on a live source is any
// moment at all.
type lockingSource struct {
	mu         sync.RWMutex
	subs       map[string]PacketHandler
	raceFanout bool
}

func newLockingSource() *lockingSource {
	return &lockingSource{subs: map[string]PacketHandler{}}
}

func (s *lockingSource) Type() string     { return "webrtc" }
func (s *lockingSource) IsActive() bool   { return true }
func (s *lockingSource) RequestKeyframe() {}
func (s *lockingSource) Stop()            {}
func (s *lockingSource) Subscribe(id string, h PacketHandler) func() {
	s.mu.Lock()
	s.subs[id] = h
	s.mu.Unlock()
	return func() {
		if s.raceFanout {
			holding := make(chan struct{})
			go s.fanout("audio", &rtp.Packet{}, holding)
			<-holding
		}
		s.mu.Lock()
		delete(s.subs, id)
		s.mu.Unlock()
	}
}

func (s *lockingSource) fanout(kind string, pkt *rtp.Packet, holding chan<- struct{}) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	close(holding)
	dispatchAll("locking", s.subs, kind, pkt)
}

// Leaving a source must never hold v.mu while unsubscribing. Unsubscribe waits for the
// source's in-flight fan-out, and that fan-out is inside this viewer's handler waiting for
// v.mu: an ABBA deadlock that wedges the source and every viewer on it. Every ad pre-roll
// ends in a SwitchTo onto live, so this is not a rare path.
func TestLeavingASourceDuringFanoutDoesNotDeadlock(t *testing.T) {
	cases := []struct {
		name  string
		leave func(v *Viewer)
	}{
		{"SwitchTo", func(v *Viewer) {
			v.SwitchTo("b", newLockingSource())
		}},
		{"activateSource", func(v *Viewer) {
			// A pending source activated while the viewer is still subscribed to the old one,
			// as OnConnectionStateChange does.
			attach(v, "b", newLockingSource())
		}},
		{"DetachSource", func(v *Viewer) {
			v.DetachSource()
		}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			v := capturingViewer(t, "lock-order", 0)
			v.connected = true
			old := newLockingSource()
			attach(v, "a", old)
			old.raceFanout = true

			done := make(chan struct{})
			go func() {
				defer close(done)
				tc.leave(v)
			}()
			select {
			case <-done:
			case <-time.After(2 * time.Second):
				t.Fatalf("%s deadlocked: it held v.mu while unsubscribing from a source "+
					"whose fan-out was blocked on v.mu", tc.name)
			}

			// Unsubscribe returned, so the racing fan-out finished — and it got through the
			// handler, which proves the race really ran into v.mu rather than missing it.
			v.mu.RLock()
			got := v.audioPkts
			v.mu.RUnlock()
			if got != 1 {
				t.Fatalf("the racing packet should have reached the viewer once, got %d", got)
			}
		})
	}
}
