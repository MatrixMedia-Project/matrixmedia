package main

import (
	"net"
	"os"
	"strings"
	"testing"

	"github.com/pion/webrtc/v4"
)

// TestMain makes every peer connection the tests create gather ICE candidates on
// loopback addresses only.
//
// Several tests here run both ends of a WebRTC connection inside this one process.
// Left alone, each end gathers a candidate on every address the host has, and ICE has
// to try every pair of them. On a host with many interfaces (VPN tunnels, a VM or
// container bridge) that took more than six seconds, against the eight seconds
// createPeerConnection gives ICE before it calls the connection failed. The tests
// then passed or failed with the machine's load. Two ends in one process can always
// reach each other on loopback, so that is all they are given.
//
// It is set once, before any test runs, and never changed back: the hook is read by
// goroutines that outlive the test that started them.
func TestMain(m *testing.M) {
	tuneICEGathering = func(se *webrtc.SettingEngine) {
		se.SetIncludeLoopbackCandidate(true)
		se.SetIPFilter(func(ip net.IP) bool { return ip.IsLoopback() })
	}
	os.Exit(m.Run())
}

// What the cascade end-to-end tests rely on: a peer connection made the way production
// makes one, inside this test binary, offers loopback candidates and nothing else. It
// fails on any host that has a network interface if TestMain's setting is lost or
// createPeerConnection stops applying it.
func TestPeerConnectionsInTestsGatherLoopbackCandidatesOnly(t *testing.T) {
	pc, err := createPeerConnection()
	if err != nil {
		t.Fatalf("createPeerConnection: %v", err)
	}
	defer pc.Close()

	if _, err := pc.AddTransceiverFromKind(webrtc.RTPCodecTypeVideo); err != nil {
		t.Fatalf("add transceiver: %v", err)
	}
	offer, err := pc.CreateOffer(nil)
	if err != nil {
		t.Fatalf("create offer: %v", err)
	}
	gathered := webrtc.GatheringCompletePromise(pc)
	if err := pc.SetLocalDescription(offer); err != nil {
		t.Fatalf("set local description: %v", err)
	}
	<-gathered

	candidates := 0
	for _, line := range strings.Split(pc.LocalDescription().SDP, "\n") {
		line = strings.TrimSpace(line)
		if !strings.HasPrefix(line, "a=candidate:") {
			continue
		}
		// a=candidate:<foundation> <component> <transport> <priority> <address> <port> typ …
		fields := strings.Fields(line)
		if len(fields) < 6 {
			t.Fatalf("unexpected candidate line %q", line)
		}
		candidates++
		ip := net.ParseIP(fields[4])
		if ip == nil || !ip.IsLoopback() {
			t.Errorf("gathered a candidate that is not on loopback: %q", line)
		}
	}
	if candidates == 0 {
		t.Fatal("no candidates gathered: loopback candidates must be included, or the two ends cannot connect")
	}
}
