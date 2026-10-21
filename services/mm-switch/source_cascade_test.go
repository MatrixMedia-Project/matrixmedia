package main

import (
	"bytes"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"github.com/pion/rtp"
	"github.com/pion/webrtc/v4"
)

// originStub spins up an httptest server that answers POST /api/viewers/offer
// like mm-switch's own handleViewerOffer: it builds its own PeerConnection,
// completes the offer/answer exchange, and replies with a valid SDP answer.
// This lets CascadeSource complete a real (loopback) WebRTC handshake in-process.
func originStub(t *testing.T) *httptest.Server {
	t.Helper()
	mux := http.NewServeMux()
	mux.HandleFunc("POST /api/viewers/offer", func(w http.ResponseWriter, r *http.Request) {
		if got := r.Header.Get("Authorization"); got != "Bearer test-token" {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}
		var req struct {
			ID    string                    `json:"id"`
			Offer webrtc.SessionDescription `json:"offer"`
		}
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			http.Error(w, err.Error(), 400)
			return
		}

		pc, err := createPeerConnection()
		if err != nil {
			http.Error(w, err.Error(), 500)
			return
		}
		t.Cleanup(func() { _ = pc.Close() })

		if err := pc.SetRemoteDescription(req.Offer); err != nil {
			http.Error(w, err.Error(), 500)
			return
		}
		answer, err := pc.CreateAnswer(nil)
		if err != nil {
			http.Error(w, err.Error(), 500)
			return
		}
		gatherComplete := webrtc.GatheringCompletePromise(pc)
		if err := pc.SetLocalDescription(answer); err != nil {
			http.Error(w, err.Error(), 500)
			return
		}
		<-gatherComplete

		w.WriteHeader(http.StatusOK)
		_ = json.NewEncoder(w).Encode(map[string]any{
			"id":     req.ID,
			"answer": pc.LocalDescription(),
		})
	})
	return httptest.NewServer(mux)
}

func TestCascadeSourceSubscribesToOrigin(t *testing.T) {
	srv := originStub(t)
	defer srv.Close()

	src, err := NewCascadeSource("cascade-1", srv.URL, "test-token")
	if err != nil {
		t.Fatalf("NewCascadeSource: %v", err)
	}
	defer src.Stop()

	if src.Type() != "cascade" {
		t.Fatalf("Type() = %q, want %q", src.Type(), "cascade")
	}

	// The offer/answer exchange has completed; the underlying PeerConnection
	// must have a non-nil remote description (the origin's answer).
	if src.pc.RemoteDescription() == nil {
		t.Fatal("cascade source has no remote description — offer/answer exchange did not complete")
	}
}

func TestCascadeSourceForwardsRTPToSubscribers(t *testing.T) {
	srv := originStub(t)
	defer srv.Close()

	src, err := NewCascadeSource("cascade-2", srv.URL, "test-token")
	if err != nil {
		t.Fatalf("NewCascadeSource: %v", err)
	}
	defer src.Stop()

	received := make(chan *rtp.Packet, 1)
	unsub := src.Subscribe("sub-1", func(kind string, pkt *rtp.Packet) {
		if kind == "video" {
			received <- pkt
		}
	})
	defer unsub()

	pkt := &rtp.Packet{
		Header:  rtp.Header{SequenceNumber: 42, Timestamp: 12345},
		Payload: []byte{0xde, 0xad, 0xbe, 0xef},
	}

	// Inject directly into the fan-out path, mirroring source_webrtc.go's
	// fanout semantics: subscribers receive the ORIGINAL packet (same
	// sequence number / payload), not a copy with different identity.
	src.fanout("video", pkt)

	select {
	case got := <-received:
		if got.SequenceNumber != pkt.SequenceNumber || !bytes.Equal(got.Payload, pkt.Payload) {
			t.Fatalf("subscriber got mismatched packet: %+v", got)
		}
	case <-time.After(time.Second):
		t.Fatal("subscriber never received the forwarded packet")
	}
}

// TestCascadeReconnectSurvivesLateOldPCClose pins the stale-handler guards:
// after a reconnect replaces the source's PeerConnection, the OLD PC closing
// late fires its OnConnectionStateChange (Closed) — which must NOT trigger
// another reconnect that would tear down the healthy new connection — and a
// late OnTrack from the old PC must not mark the source active or repoint
// videoTrack at a dead track.
func TestCascadeReconnectSurvivesLateOldPCClose(t *testing.T) {
	// Shrink the backoff so a (buggy) reconnect triggered by the old PC's
	// Closed state would replace the new PC well inside the observation window.
	origDelay := cascadeReconnectBaseDelay
	cascadeReconnectBaseDelay = 50 * time.Millisecond
	defer func() { cascadeReconnectBaseDelay = origDelay }()

	srv := originStub(t)
	defer srv.Close()

	src, err := NewCascadeSource("cascade-churn", srv.URL, "test-token")
	if err != nil {
		t.Fatalf("NewCascadeSource: %v", err)
	}
	defer src.Stop()

	src.mu.RLock()
	oldPC := src.pc
	src.mu.RUnlock()

	// Simulate a reconnect: a second handshake replaces src.pc and closes the
	// old PC (connect() does both), exactly the churn the guards exist for.
	if err := src.connect(); err != nil {
		t.Fatalf("reconnect: %v", err)
	}

	src.mu.RLock()
	newPC := src.pc
	src.mu.RUnlock()
	if newPC == oldPC {
		t.Fatal("connect() did not replace the PeerConnection")
	}

	// The old PC is now closed; its Closed-state handler fires asynchronously.
	// Without the staleness guard it queues a reconnect, which (with the
	// shortened backoff) would replace the healthy new PC within this window.
	time.Sleep(time.Second)

	src.mu.RLock()
	stillNew := src.pc == newPC
	src.mu.RUnlock()
	if !stillNew {
		t.Fatal("old PC's late Closed state triggered a reconnect that replaced the healthy new connection")
	}
}

func TestCascadeHandlerRejectsDuplicateID(t *testing.T) {
	setSwitch(NewMediaSwitch())

	srv := originStub(t)
	defer srv.Close()

	body := map[string]string{
		"id":         "cascade-dup",
		"origin_url": srv.URL,
		"token":      "test-token",
	}

	code1 := postOffer(t, handleAddCascadeSource, "/api/sources/cascade", body)
	if code1 != http.StatusCreated {
		t.Fatalf("first request: status = %d, want %d", code1, http.StatusCreated)
	}

	code2 := postOffer(t, handleAddCascadeSource, "/api/sources/cascade", body)
	if code2 != http.StatusConflict {
		t.Fatalf("duplicate request: status = %d, want %d", code2, http.StatusConflict)
	}
}
