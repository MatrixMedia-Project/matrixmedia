package main

import (
	"bytes"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"sync"
	"testing"
	"time"

	"github.com/pion/rtp"
)

// ─── B1 + B3 production-shaped loopback: one relay, one broadcast, real RTP ─────────────
//
// This is the "same three-file seam, one fix round" test the review's suggested fix order
// calls for: it proves, end to end and in the PRODUCTION id shape (switch_source_id =
// "stream-{broadcast}", not a raw test id), that
//
//   - B1: a token minted for switch_source_id(broadcast)+relay_node_id validates against
//     the offer the relay actually sends (cascade-{source}-{relay}, stripped the same way
//     mm-switch's production code strips it) — no subject_mismatch 401.
//   - B3: the cascade viewer gets ATTACHED to the broadcast's real programme source after
//     authenticating, not left subscribed to nothing.
//   - Actual media flows: a real Source emits RTP, a real Viewer (the origin's) forwards
//     it over a real WebRTC connection, the relay's CascadeSource's OnTrack fires, and a
//     subscriber on the relay side receives the ORIGINAL packet.
//
// B2 (the relay's OWN /api/sources/cascade endpoint requiring a server bearer) is a
// different seam — mm-core (Rust) authenticating to the relay (Go) — and is proven by
// TestAddCascadeSourceRequiresServerAuth below instead of folded into this one, because
// this test's "relay" is a bare CascadeSource object (constructed directly, as mm-core's
// seeded process would construct it conceptually), not routed back through this same
// process's single global MediaSwitch a second time (which is already playing the
// origin's role here — see that test's comment for why they can't share one call).
func TestCascadeLoopbackRTPFlowsFromOriginToRelaySubscriber(t *testing.T) {
	setSwitch(NewMediaSwitch())
	const (
		originSecret = "origin-secret"
		broadcastID  = "3f2a1c9e-0000-4000-8000-00000000000a" // UUID-shaped, like a real stream id
		relayNodeID  = "relay-prod-1"
	)
	sourceID := switch_source_id_for_test(broadcastID) // "stream-{broadcastID}"

	// The origin's real programme source, emitting real RTP on a loop once it has a
	// subscriber — standing in for a live WebRTCSource's publisher feed.
	prog := newTestProgrammeSource()
	mediaSwitch.AddSource(sourceID, prog)
	defer prog.Stop()

	// The origin's REAL production route: the same wrapViewerOfferAuth + handleViewerOffer
	// wiring main() installs, not a simplified stub. This is what makes the test exercise
	// B1's claim/offer agreement and B3's attach, not a stand-in for them.
	mux := http.NewServeMux()
	mux.Handle("POST /api/viewers/offer", wrapViewerOfferAuth(originSecret, []string{roleServer, roleViewer}, handleViewerOffer))
	originSrv := httptest.NewServer(mux)
	defer originSrv.Close()

	// B1: mint for the PRODUCTION claim shape — switch_source_id(broadcast), not the raw
	// broadcast id — exactly what pool_sync.rs's seed_cascade_source now does.
	future := time.Now().Add(time.Hour).Unix()
	token := mintCascadeTokenForTest(originSecret, sourceID, relayNodeID, future)

	relay, err := NewCascadeSource(sourceID, originSrv.URL, token, relayNodeID)
	if err != nil {
		t.Fatalf("relay could not cascade from the origin: %v", err)
	}
	defer relay.Stop()

	received := make(chan *rtp.Packet, 1)
	unsub := relay.Subscribe("relay-subscriber", func(kind string, pkt *rtp.Packet) {
		if kind == "video" {
			select {
			case received <- pkt:
			default:
			}
		}
	})
	defer unsub()

	select {
	case pkt := <-received:
		if len(pkt.Payload) == 0 {
			t.Fatal("received packet had no payload")
		}
	case <-time.After(10 * time.Second):
		t.Fatal("B1+B3: relay's cascade source never received real RTP from the origin's " +
			"programme source — either the handshake was rejected (B1 id/claim mismatch) " +
			"or the viewer was never attached to the source (B3)")
	}

	if n := len(mediaSwitch.ListViewers()); n != 1 {
		t.Fatalf("expected exactly 1 cascade viewer registered on the origin, got %d", n)
	}
	gotID := mediaSwitch.ListViewers()[0].ID
	wantID := "cascade-" + sourceID + "-" + relayNodeID
	if gotID != wantID {
		t.Fatalf("cascade viewer id = %q, want %q (M1 per-relay id scheme)", gotID, wantID)
	}
	if mediaSwitch.ListViewers()[0].CurrentSource != sourceID {
		t.Fatalf("B3: cascade viewer's current_source = %q, want %q — it was never attached to the programme source", mediaSwitch.ListViewers()[0].CurrentSource, sourceID)
	}
}

// ─── B2: the relay's /api/sources/cascade endpoint demands a server bearer ──────────────
//
// Before the fix, this route accepted an unauthenticated body (the JSON "token" field is
// the CASCADE token the relay will later present to the ORIGIN — a completely different
// credential from whatever authenticates mm-core to THIS relay). TestAddCascadeSourceRequiresServerAuth
// pins that the route itself is gated on the server-role bearer, same as every other
// /api/sources/* route.
func TestAddCascadeSourceRequiresServerAuth(t *testing.T) {
	setSwitch(NewMediaSwitch())
	const secret = "relay-server-secret"
	handler := wrapAuth(secret, []string{roleServer}, handleAddCascadeSource)

	// A real (loopback) origin for NewCascadeSource to dial — the auth gate under test is
	// on THIS request (to this relay's own /api/sources/cascade), not on the origin.
	origin := originStub(t)
	defer origin.Close()

	bodyFor := func(id string) []byte {
		b, _ := json.Marshal(map[string]string{
			"id":            id,
			"origin_url":    origin.URL,
			"token":         "test-token",
			"relay_node_id": "relay-1",
		})
		return b
	}

	// No Authorization header at all: must be rejected.
	req := httptest.NewRequest("POST", "/api/sources/cascade", bytes.NewReader(bodyFor("stream-b2-test-noauth")))
	rec := httptest.NewRecorder()
	handler.ServeHTTP(rec, req)
	if rec.Code != http.StatusUnauthorized {
		t.Fatalf("missing server bearer: status = %d, want %d", rec.Code, http.StatusUnauthorized)
	}

	// A valid server-role bearer succeeds.
	req2 := httptest.NewRequest("POST", "/api/sources/cascade", bytes.NewReader(bodyFor("stream-b2-test-auth")))
	req2.Header.Set("Authorization", "Bearer "+generateToken(secret, roleServer, "mm-core", 60))
	rec2 := httptest.NewRecorder()
	handler.ServeHTTP(rec2, req2)
	if rec2.Code != http.StatusCreated {
		t.Fatalf("valid server bearer: status = %d, want %d (body: %s)", rec2.Code, http.StatusCreated, rec2.Body.String())
	}
}

// switch_source_id_for_test mirrors mm-core's switch_source_id (crates/mm-core/src/switch_client.rs)
// without importing Rust — the Go side only ever receives this value over the wire, so the
// format is pinned here the same way egress_test.go already pins it.
func switch_source_id_for_test(streamID string) string {
	return "stream-" + streamID
}

// testProgrammeSource is a minimal Source that, once ANY subscriber joins, emits one RTP
// video packet roughly every 50ms until stopped — standing in for a live publisher feed
// for exactly long enough that a real WebRTC handshake (which completes asynchronously,
// after Subscribe is called) has time to come up and receive at least one of them.
type testProgrammeSource struct {
	mu     sync.Mutex
	subs   map[string]PacketHandler
	stopCh chan struct{}
	seq    uint16
}

func newTestProgrammeSource() *testProgrammeSource {
	s := &testProgrammeSource{subs: make(map[string]PacketHandler), stopCh: make(chan struct{})}
	go s.loop()
	return s
}

func (s *testProgrammeSource) loop() {
	ticker := time.NewTicker(50 * time.Millisecond)
	defer ticker.Stop()
	for {
		select {
		case <-s.stopCh:
			return
		case <-ticker.C:
			s.mu.Lock()
			s.seq++
			// VP8 RTP payload descriptor (RFC 7741): 0x10 = start-of-partition, no
			// extension; next byte's low bit 0 = keyframe. Viewer.go buffers video
			// until it sees a real keyframe before forwarding, same as a live
			// publisher's first frame — a packet that doesn't parse as one would
			// never reach the relay, making this test indistinguishable from a
			// real B1/B3 failure.
			pkt := &rtp.Packet{
				// Version 2 (real RTP, not the zero-value a bare struct literal gives
				// you) — WriteRTP marshals this onto the wire as-is, and a version-0
				// packet either never leaves the sender or is dropped on arrival,
				// which looks EXACTLY like "the viewer was never attached" (B3) from
				// outside. SSRC/PayloadType are whatever TrackLocalStaticRTP
				// negotiates on send; only Version/SequenceNumber/Timestamp/Marker
				// need to be real here.
				Header: rtp.Header{
					Version:        2,
					SequenceNumber: s.seq,
					Timestamp:      uint32(s.seq) * 90,
					Marker:         true,
				},
				Payload: []byte{0x10, 0x00, 0xde, 0xad, 0xbe, 0xef},
			}
			for _, h := range s.subs {
				h("video", pkt)
			}
			s.mu.Unlock()
		}
	}
}

func (s *testProgrammeSource) Type() string   { return "test-programme" }
func (s *testProgrammeSource) IsActive() bool { return true }
func (s *testProgrammeSource) Subscribe(id string, handler PacketHandler) func() {
	s.mu.Lock()
	s.subs[id] = handler
	s.mu.Unlock()
	return func() {
		s.mu.Lock()
		delete(s.subs, id)
		s.mu.Unlock()
	}
}
func (s *testProgrammeSource) RequestKeyframe() {}
func (s *testProgrammeSource) Stop() {
	select {
	case <-s.stopCh:
	default:
		close(s.stopCh)
	}
}

var _ Source = (*testProgrammeSource)(nil)
