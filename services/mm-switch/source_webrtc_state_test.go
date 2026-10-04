package main

import (
	"net"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/pion/ice/v4"
	"github.com/pion/rtp"
	"github.com/pion/webrtc/v4"
)

// A publisher's source must stop reporting ACTIVE once its PeerConnection is Failed or
// Closed. WebRTCSource.active used to be set on the first track and never cleared, so
// "the switch has an active source for this stream" could not tell a live host from a
// crashed one — and mm-core's stream auto-end sweep relies on exactly that signal.
//
// These tests run a real publisher PeerConnection against the source's PeerConnection
// in-process (loopback ICE, short ICE timeouts) and watch IsActive().

const (
	rigDisconnectedTimeout = 500 * time.Millisecond
	rigFailedTimeout       = 1 * time.Second
	rigKeepAlive           = 200 * time.Millisecond

	// Generous but bounded: the happy path settles in well under a second.
	rigWait = 10 * time.Second
)

// blackholeConn wraps the publisher's UDP socket. Flipping dropping to true silently
// discards every packet in both directions while the publisher's PeerConnection stays
// alive and sends no close_notify — a faithful model of a host whose box or network died.
type blackholeConn struct {
	net.PacketConn
	dropping atomic.Bool
}

func (c *blackholeConn) ReadFrom(p []byte) (int, net.Addr, error) {
	for {
		n, addr, err := c.PacketConn.ReadFrom(p)
		if err != nil || !c.dropping.Load() {
			return n, addr, err
		}
	}
}

func (c *blackholeConn) WriteTo(p []byte, addr net.Addr) (int, error) {
	if c.dropping.Load() {
		return len(p), nil
	}
	return c.PacketConn.WriteTo(p, addr)
}

type publisherRig struct {
	src   *WebRTCSource
	srcPC *webrtc.PeerConnection // the switch's side; what handlePublishOffer hands to NewWebRTCSource
	pubPC *webrtc.PeerConnection // the broadcaster
	wire  *blackholeConn         // the broadcaster's network
}

func rigSettings(failed time.Duration) webrtc.SettingEngine {
	var se webrtc.SettingEngine
	se.SetICETimeouts(rigDisconnectedTimeout, failed, rigKeepAlive)
	se.SetIncludeLoopbackCandidate(true)
	se.SetNetworkTypes([]webrtc.NetworkType{webrtc.NetworkTypeUDP4})
	return se
}

func mustPC(t *testing.T, se webrtc.SettingEngine) *webrtc.PeerConnection {
	t.Helper()
	m := &webrtc.MediaEngine{}
	if err := m.RegisterDefaultCodecs(); err != nil {
		t.Fatalf("codecs: %v", err)
	}
	pc, err := webrtc.NewAPI(webrtc.WithMediaEngine(m), webrtc.WithSettingEngine(se)).NewPeerConnection(webrtc.Configuration{})
	if err != nil {
		t.Fatalf("new peer connection: %v", err)
	}
	return pc
}

// newPublisherRig connects a publisher to a fresh WebRTCSource and returns once the
// source has gone ACTIVE (its first RTP packet arrived). failed is the ICE failed
// timeout on both ends; it is the width of the Disconnected window.
func newPublisherRig(t *testing.T, failed time.Duration) *publisherRig {
	t.Helper()

	// Publisher: ICE over a socket we can cut.
	udp, err := net.ListenPacket("udp4", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen udp: %v", err)
	}
	wire := &blackholeConn{PacketConn: udp}
	mux := ice.NewUDPMuxDefault(ice.UDPMuxParams{UDPConn: wire})
	pubSE := rigSettings(failed)
	pubSE.SetICEUDPMux(mux)
	pubPC := mustPC(t, pubSE)

	srcPC := mustPC(t, rigSettings(failed))
	src := NewWebRTCSource("stream-test", srcPC)

	track, err := webrtc.NewTrackLocalStaticRTP(
		webrtc.RTPCodecCapability{MimeType: webrtc.MimeTypeVP8}, "video", "publisher")
	if err != nil {
		t.Fatalf("track: %v", err)
	}
	if _, err := pubPC.AddTrack(track); err != nil {
		t.Fatalf("add track: %v", err)
	}

	// Offer/answer, in-process, with complete candidate gathering (no trickle).
	offer, err := pubPC.CreateOffer(nil)
	if err != nil {
		t.Fatalf("offer: %v", err)
	}
	pubGathered := webrtc.GatheringCompletePromise(pubPC)
	if err := pubPC.SetLocalDescription(offer); err != nil {
		t.Fatalf("pub set local: %v", err)
	}
	<-pubGathered
	if err := srcPC.SetRemoteDescription(*pubPC.LocalDescription()); err != nil {
		t.Fatalf("src set remote: %v", err)
	}
	answer, err := srcPC.CreateAnswer(nil)
	if err != nil {
		t.Fatalf("answer: %v", err)
	}
	srcGathered := webrtc.GatheringCompletePromise(srcPC)
	if err := srcPC.SetLocalDescription(answer); err != nil {
		t.Fatalf("src set local: %v", err)
	}
	<-srcGathered
	if err := pubPC.SetRemoteDescription(*srcPC.LocalDescription()); err != nil {
		t.Fatalf("pub set remote: %v", err)
	}

	// The broadcaster sends RTP until the test ends; pion fires OnTrack on the first packet.
	stopMedia := make(chan struct{})
	var media sync.WaitGroup
	media.Add(1)
	go func() {
		defer media.Done()
		tick := time.NewTicker(20 * time.Millisecond)
		defer tick.Stop()
		var seq uint16
		for {
			select {
			case <-stopMedia:
				return
			case <-tick.C:
				seq++
				_ = track.WriteRTP(&rtp.Packet{
					Header:  rtp.Header{Version: 2, SequenceNumber: seq, Timestamp: uint32(seq) * 3000},
					Payload: []byte{0x10, 0x00, 0x00, 0x00},
				})
			}
		}
	}()

	t.Cleanup(func() {
		close(stopMedia)
		media.Wait()
		_ = pubPC.Close()
		_ = srcPC.Close()
		_ = mux.Close()
	})

	rig := &publisherRig{src: src, srcPC: srcPC, pubPC: pubPC, wire: wire}
	waitUntil(t, rigWait, "source to go active", func() bool { return src.IsActive() })
	return rig
}

func waitUntil(t *testing.T, limit time.Duration, what string, cond func() bool) {
	t.Helper()
	deadline := time.Now().Add(limit)
	for !cond() {
		if time.Now().After(deadline) {
			t.Fatalf("timed out after %s waiting for %s", limit, what)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

// (a) Closing the source's own PeerConnection — what Stop() and the failed-offer cleanup do.
func TestWebRTCSourceGoesInactiveWhenItsConnectionCloses(t *testing.T) {
	for _, tc := range []struct {
		name  string
		close func(r *publisherRig)
	}{
		{"pc.Close", func(r *publisherRig) { _ = r.srcPC.Close() }},
		{"Stop", func(r *publisherRig) { r.src.Stop() }},
	} {
		t.Run(tc.name, func(t *testing.T) {
			r := newPublisherRig(t, rigFailedTimeout)
			tc.close(r)
			waitUntil(t, rigWait, "source to go inactive after its connection closed",
				func() bool { return !r.src.IsActive() })
		})
	}
}

// (b) The publisher goes away. Two ways a host disappears: it hangs up (closes its
// PeerConnection), or it just dies and the network goes quiet.
func TestWebRTCSourceGoesInactiveWhenThePublisherHangsUp(t *testing.T) {
	r := newPublisherRig(t, rigFailedTimeout)
	if err := r.pubPC.Close(); err != nil {
		t.Fatalf("close publisher: %v", err)
	}
	waitUntil(t, rigWait, "source to go inactive after the publisher closed",
		func() bool { return !r.src.IsActive() })
	t.Logf("source PeerConnection ended in state %s", r.srcPC.ConnectionState())
}

func TestWebRTCSourceGoesInactiveWhenThePublisherVanishes(t *testing.T) {
	r := newPublisherRig(t, rigFailedTimeout)
	r.wire.dropping.Store(true) // no close_notify, no more packets: a crashed host

	waitUntil(t, rigWait, "source to go inactive after the publisher vanished",
		func() bool { return !r.src.IsActive() })
	if got := r.srcPC.ConnectionState(); got != webrtc.PeerConnectionStateFailed {
		t.Fatalf("a silent publisher must end in Failed (ICE timeout), got %s", got)
	}
}

// (c) Disconnected is transient: ICE can recover, so the source must stay active through
// it — and still be active once the connection is back.
func TestWebRTCSourceStaysActiveThroughDisconnected(t *testing.T) {
	// A long failed timeout keeps the Disconnected window open long enough to observe.
	r := newPublisherRig(t, 5*time.Second)
	r.wire.dropping.Store(true)

	waitUntil(t, rigWait, "source PeerConnection to report Disconnected", func() bool {
		return r.srcPC.ConnectionState() == webrtc.PeerConnectionStateDisconnected
	})
	if !r.src.IsActive() {
		t.Fatal("Disconnected cleared the source; it is transient and must not")
	}

	r.wire.dropping.Store(false) // the network heals
	waitUntil(t, rigWait, "source PeerConnection to recover to Connected", func() bool {
		return r.srcPC.ConnectionState() == webrtc.PeerConnectionStateConnected
	})
	if !r.src.IsActive() {
		t.Fatal("source went inactive across a Disconnected -> Connected recovery")
	}
}

// The state -> "publisher is gone" mapping, over every state, so the transient states
// cannot be silently promoted to terminal ones.
func TestPublisherGoneStates(t *testing.T) {
	for state, want := range map[webrtc.PeerConnectionState]bool{
		webrtc.PeerConnectionStateUnknown:      false,
		webrtc.PeerConnectionStateNew:          false,
		webrtc.PeerConnectionStateConnecting:   false,
		webrtc.PeerConnectionStateConnected:    false,
		webrtc.PeerConnectionStateDisconnected: false,
		webrtc.PeerConnectionStateFailed:       true,
		webrtc.PeerConnectionStateClosed:       true,
	} {
		if got := publisherGone(state); got != want {
			t.Errorf("publisherGone(%s) = %v, want %v", state, got, want)
		}
	}
}

// pion runs OnTrack and OnConnectionStateChange in separate goroutines with no ordering,
// so a publisher that hangs up right after its first RTP packet can have the Closed
// handler run BEFORE OnTrack. The late track must not resurrect the source: nothing
// would ever clear it again, and mm-core's sweep would keep a dead broadcast alive.
// Driven through the same two methods the PeerConnection callbacks call, in the
// problem order, so it is deterministic.
func TestGoneSourceCannotBeReactivatedByALateTrack(t *testing.T) {
	for _, state := range []webrtc.PeerConnectionState{
		webrtc.PeerConnectionStateClosed,
		webrtc.PeerConnectionStateFailed,
	} {
		t.Run(state.String(), func(t *testing.T) {
			s := &WebRTCSource{id: "stream-late-track"}

			s.connectionStateChanged(state) // handler wins the race...
			if s.trackArrived("video", nil, nil) {
				t.Fatal("trackArrived accepted a track for a gone publisher")
			}
			if s.IsActive() { // ...and OnTrack arrives late
				t.Fatalf("a late track re-activated a source whose connection is %s", state)
			}

			// Stays gone through further state noise (Failed is followed by Closed on Stop).
			s.connectionStateChanged(webrtc.PeerConnectionStateClosed)
			if s.trackArrived("audio", nil, nil) || s.IsActive() {
				t.Fatal("a gone source became active again")
			}
		})
	}
}

// The ordinary order, and the transient state: neither may make the source sticky-gone.
func TestSourceActivatesAndDeactivatesInTheOrdinaryOrder(t *testing.T) {
	s := &WebRTCSource{id: "stream-ordinary"}

	s.connectionStateChanged(webrtc.PeerConnectionStateDisconnected) // transient: not gone
	if !s.trackArrived("video", nil, nil) || !s.IsActive() {
		t.Fatal("Disconnected must not stop a track from activating the source")
	}

	s.connectionStateChanged(webrtc.PeerConnectionStateClosed)
	if s.IsActive() {
		t.Fatal("Closed must clear an active source")
	}
}
