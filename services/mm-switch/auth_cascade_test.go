// R2: cascade capability token.
//
// A rented relay node cascades a broadcast from the origin by posting a viewer offer with
// id = "cascade-{broadcast_id}" and a Bearer token minted by mm-core's mint_cascade_token
// (crates/mm-core/src/switch_client.rs), not an ordinary per-viewer token. This file proves
// the Go side accepts exactly that token for exactly that broadcast, rejects everything
// else (expired, wrong broadcast, malformed), and that a relay reconnecting with a fresh
// token replaces its old subscription instead of duplicating it.

package main

import (
	"bytes"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"github.com/pion/webrtc/v4"
)

// ─── Shared fixture — byte-for-byte identical to Rust's cascade_token_tests ──────────────
//
// crates/mm-core/src/switch_client.rs: mint_cascade_token_roundtrip asserts the SAME
// token string for the SAME inputs. If the claim format or HMAC construction drifts
// between the two languages, one of the two tests fails — cross-language compatibility
// proven by a shared fixture, not hope.
const (
	fixtureSecret              = "test-secret"
	fixtureBroadcastID         = "broadcast-abc"
	fixtureRelayNodeID         = "relay-1"
	fixtureNowUnix       int64 = 1700000000
	fixtureTTLSecs       int64 = 300
	fixtureExpUnix       int64 = fixtureNowUnix + fixtureTTLSecs
	fixtureExpectedToken       = "cascade:broadcast-abc:relay-1:1700000300.3949d0a0c51268ae143e46f8d4d4c88233736f72abf6a191c9bf73053350c475"
)

// mintCascadeTokenForTest mirrors mint_cascade_token so tests can build tokens for
// broadcasts/relays/expiries the fixture doesn't cover (e.g. "already expired").
func mintCascadeTokenForTest(secret, broadcastID, relayNodeID string, exp int64) string {
	claim := fmt.Sprintf("cascade:%s:%s:%d", broadcastID, relayNodeID, exp)
	mac := hmac.New(sha256.New, []byte(secret))
	mac.Write([]byte(claim))
	sig := hex.EncodeToString(mac.Sum(nil))
	return claim + "." + sig
}

func TestCascadeTokenValid(t *testing.T) {
	// The exact string mint_cascade_token produces for the fixture inputs — byte-for-byte
	// identical to Rust's mint_cascade_token_roundtrip. The fixture's exp (2023) is long
	// past by the time this test runs, so the SHAPE/SIGNATURE check here is independent of
	// expiry; TestCascadeTokenExpiredRejected below covers the expiry rule itself.
	if got := mintCascadeTokenForTest(fixtureSecret, fixtureBroadcastID, fixtureRelayNodeID, fixtureExpUnix); got != fixtureExpectedToken {
		t.Fatalf("test fixture drifted from the Rust mint: got %q, want %q", got, fixtureExpectedToken)
	}

	// Signature + broadcast match a FRESH (not-yet-expired) token built the same way —
	// proves the fixture's signing scheme is what validateCascadeToken actually accepts.
	future := time.Now().Add(time.Hour).Unix()
	fresh := mintCascadeTokenForTest(fixtureSecret, fixtureBroadcastID, fixtureRelayNodeID, future)
	if rej, err := validateCascadeToken(fixtureSecret, fresh, fixtureBroadcastID, fixtureRelayNodeID); err != nil {
		t.Fatalf("a freshly minted cascade token in the shared format must validate: %v (reason %s)", err, rej)
	}
}

func TestCascadeTokenExpiredRejected(t *testing.T) {
	past := time.Now().Add(-time.Hour).Unix()
	token := mintCascadeTokenForTest(fixtureSecret, fixtureBroadcastID, fixtureRelayNodeID, past)
	rej, err := validateCascadeToken(fixtureSecret, token, fixtureBroadcastID, fixtureRelayNodeID)
	if err == nil {
		t.Fatal("an expired cascade token was accepted")
	}
	if rej != rejReasonExpired {
		t.Fatalf("expired token's rejection reason = %q, want %q", rej, rejReasonExpired)
	}
}

func TestCascadeTokenWrongBroadcastRejected(t *testing.T) {
	future := time.Now().Add(time.Hour).Unix()
	token := mintCascadeTokenForTest(fixtureSecret, "broadcast-a", fixtureRelayNodeID, future)
	rej, err := validateCascadeToken(fixtureSecret, token, "broadcast-b", fixtureRelayNodeID)
	if err == nil {
		t.Fatal("a cascade token minted for one broadcast validated against another")
	}
	if rej != rejReasonSubjectMismatch {
		t.Fatalf("wrong-broadcast rejection reason = %q, want %q", rej, rejReasonSubjectMismatch)
	}
}

func TestCascadeTokenWrongSecretRejected(t *testing.T) {
	future := time.Now().Add(time.Hour).Unix()
	token := mintCascadeTokenForTest("wrong-secret", fixtureBroadcastID, fixtureRelayNodeID, future)
	rej, err := validateCascadeToken(fixtureSecret, token, fixtureBroadcastID, fixtureRelayNodeID)
	if err == nil {
		t.Fatal("a cascade token signed with the wrong secret validated")
	}
	if rej != rejReasonInvalidSig {
		t.Fatalf("wrong-secret rejection reason = %q, want %q", rej, rejReasonInvalidSig)
	}
}

func TestCascadeTokenMalformedRejected(t *testing.T) {
	for _, bad := range []string{
		"",
		"not-a-token-at-all",
		"cascade:" + fixtureBroadcastID + ":" + fixtureRelayNodeID, // no signature
		"cascade:" + fixtureBroadcastID + ".deadbeef",              // too few claim fields
	} {
		rej, err := validateCascadeToken(fixtureSecret, bad, fixtureBroadcastID, fixtureRelayNodeID)
		if err == nil {
			t.Fatalf("malformed token %q was accepted", bad)
		}
		if rej == "" {
			t.Fatalf("malformed token %q rejected without a metric reason", bad)
		}
	}
}

// ─── End to end through wrapViewerOfferAuth + handleViewerOffer ─────────────────────────

// localOffer builds a fully-gathered local SDP offer from a throwaway PeerConnection, the
// same way a real cascade relay's pc.CreateOffer()/SetLocalDescription() would.
func localOffer(t *testing.T) webrtc.SessionDescription {
	t.Helper()
	pc := mustPC(t, rigSettings(5*time.Second))
	t.Cleanup(func() { _ = pc.Close() })
	if _, err := pc.AddTransceiverFromKind(webrtc.RTPCodecTypeVideo,
		webrtc.RTPTransceiverInit{Direction: webrtc.RTPTransceiverDirectionRecvonly}); err != nil {
		t.Fatalf("add video transceiver: %v", err)
	}
	if _, err := pc.AddTransceiverFromKind(webrtc.RTPCodecTypeAudio,
		webrtc.RTPTransceiverInit{Direction: webrtc.RTPTransceiverDirectionRecvonly}); err != nil {
		t.Fatalf("add audio transceiver: %v", err)
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
	return *pc.LocalDescription()
}

// postCascadeOffer sends a viewer offer for "cascade-{broadcastID}-{relayNodeID}" through
// the real middleware + handler, bearing `token`. M1: the relay_node_id the offer declares
// rides in its own body field, mirroring CascadeSource.postOffer.
func postCascadeOffer(t *testing.T, secret, broadcastID, relayNodeID, token string) int {
	t.Helper()
	handler := wrapViewerOfferAuth(secret, []string{roleServer, roleViewer}, handleViewerOffer)
	raw, err := json.Marshal(map[string]any{
		"id":            "cascade-" + broadcastID + "-" + relayNodeID,
		"relay_node_id": relayNodeID,
		"offer":         localOffer(t),
	})
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	req := httptest.NewRequest("POST", "/api/viewers/offer", bytes.NewReader(raw))
	req.Header.Set("Authorization", "Bearer "+token)
	rec := httptest.NewRecorder()
	handler.ServeHTTP(rec, req)
	return rec.Code
}

func TestCascadeOfferAcceptedForClaimedBroadcast(t *testing.T) {
	setSwitch(NewMediaSwitch())
	const secret = "cascade-e2e-secret"
	future := time.Now().Add(time.Hour).Unix()
	token := mintCascadeTokenForTest(secret, fixtureBroadcastID, fixtureRelayNodeID, future)

	code := postCascadeOffer(t, secret, fixtureBroadcastID, fixtureRelayNodeID, token)
	if code != http.StatusOK {
		t.Fatalf("a valid cascade offer for its own broadcast was rejected: status %d", code)
	}
	if n := len(mediaSwitch.ListViewers()); n != 1 {
		t.Fatalf("expected exactly 1 registered viewer, got %d", n)
	}
}

func TestCascadeOfferRejectedForWrongBroadcast(t *testing.T) {
	setSwitch(NewMediaSwitch())
	const secret = "cascade-e2e-secret"
	future := time.Now().Add(time.Hour).Unix()
	// Token minted for "broadcast-a", offer claims to be cascading "broadcast-b".
	token := mintCascadeTokenForTest(secret, "broadcast-a", fixtureRelayNodeID, future)

	code := postCascadeOffer(t, secret, "broadcast-b", fixtureRelayNodeID, token)
	if code == http.StatusOK {
		t.Fatal("a cascade token for one broadcast opened a DIFFERENT broadcast's viewer offer")
	}
	if n := len(mediaSwitch.ListViewers()); n != 0 {
		t.Fatalf("a rejected cascade offer must register nothing; switch holds %d viewer(s)", n)
	}
}

// TestCascadeOfferRejectedForWrongRelay pins M1: a token minted for relay-1 must not open
// relay-2's slot for the SAME broadcast — each relay's cascade viewer is its own, not a
// single shared slot the first valid token happens to occupy.
func TestCascadeOfferRejectedForWrongRelay(t *testing.T) {
	setSwitch(NewMediaSwitch())
	const secret = "cascade-e2e-secret"
	future := time.Now().Add(time.Hour).Unix()
	token := mintCascadeTokenForTest(secret, fixtureBroadcastID, "relay-1", future)

	code := postCascadeOffer(t, secret, fixtureBroadcastID, "relay-2", token)
	if code == http.StatusOK {
		t.Fatal("a cascade token for one relay opened a DIFFERENT relay's viewer offer")
	}
	if n := len(mediaSwitch.ListViewers()); n != 0 {
		t.Fatalf("a rejected cascade offer must register nothing; switch holds %d viewer(s)", n)
	}
}

// TestCascadeOffersFromTwoRelaysBothRegister pins M1's actual fix: two relays cascading the
// SAME broadcast each hold their own viewer slot instead of the second evicting the first
// (the flap the old shared "cascade-{broadcast_id}" id caused every sync tick).
func TestCascadeOffersFromTwoRelaysBothRegister(t *testing.T) {
	setSwitch(NewMediaSwitch())
	const secret = "cascade-e2e-secret"
	future := time.Now().Add(time.Hour).Unix()

	tokenA := mintCascadeTokenForTest(secret, fixtureBroadcastID, "relay-a", future)
	tokenB := mintCascadeTokenForTest(secret, fixtureBroadcastID, "relay-b", future)

	if code := postCascadeOffer(t, secret, fixtureBroadcastID, "relay-a", tokenA); code != http.StatusOK {
		t.Fatalf("relay-a's cascade offer rejected: status %d", code)
	}
	if code := postCascadeOffer(t, secret, fixtureBroadcastID, "relay-b", tokenB); code != http.StatusOK {
		t.Fatalf("relay-b's cascade offer rejected: status %d", code)
	}

	if n := len(mediaSwitch.ListViewers()); n != 2 {
		t.Fatalf("two relays cascading the same broadcast must both hold their own viewer slot; got %d", n)
	}
}

func TestCascadeOfferRejectedWhenExpired(t *testing.T) {
	setSwitch(NewMediaSwitch())
	const secret = "cascade-e2e-secret"
	past := time.Now().Add(-time.Hour).Unix()
	token := mintCascadeTokenForTest(secret, fixtureBroadcastID, fixtureRelayNodeID, past)

	code := postCascadeOffer(t, secret, fixtureBroadcastID, fixtureRelayNodeID, token)
	if code == http.StatusOK {
		t.Fatal("an expired cascade token opened a viewer offer")
	}
	if n := len(mediaSwitch.ListViewers()); n != 0 {
		t.Fatalf("a rejected cascade offer must register nothing; switch holds %d viewer(s)", n)
	}
}

func TestCascadeOfferRejectedWhenMalformed(t *testing.T) {
	setSwitch(NewMediaSwitch())
	const secret = "cascade-e2e-secret"

	code := postCascadeOffer(t, secret, fixtureBroadcastID, fixtureRelayNodeID, "garbage-not-a-token")
	if code == http.StatusOK {
		t.Fatal("a malformed bearer token opened a viewer offer")
	}
	if n := len(mediaSwitch.ListViewers()); n != 0 {
		t.Fatalf("a rejected cascade offer must register nothing; switch holds %d viewer(s)", n)
	}
}

// Review Focus 2: a relay that reconnects (fresh PeerConnection, fresh cascade token, same
// broadcast) must REPLACE its existing subscription under "cascade-{broadcast_id}", not
// pile up a second one next to it — MediaSwitch.AddViewer displaces by id, but only if
// auth lets the second offer reach it at all.
func TestRenewalOfferAcceptedForExistingRelaySubscriber(t *testing.T) {
	setSwitch(NewMediaSwitch())
	const secret = "cascade-e2e-secret"
	future := time.Now().Add(time.Hour).Unix()

	firstToken := mintCascadeTokenForTest(secret, fixtureBroadcastID, fixtureRelayNodeID, future)
	if code := postCascadeOffer(t, secret, fixtureBroadcastID, fixtureRelayNodeID, firstToken); code != http.StatusOK {
		t.Fatalf("first cascade offer rejected: status %d", code)
	}
	if n := len(mediaSwitch.ListViewers()); n != 1 {
		t.Fatalf("after the first offer expected 1 viewer, got %d", n)
	}

	// A fresh offer/token for the SAME broadcast — e.g. the relay reconnected after an ICE
	// drop and mm-core re-seeded its token.
	secondToken := mintCascadeTokenForTest(secret, fixtureBroadcastID, fixtureRelayNodeID, future+60)
	if code := postCascadeOffer(t, secret, fixtureBroadcastID, fixtureRelayNodeID, secondToken); code != http.StatusOK {
		t.Fatalf("renewal cascade offer rejected: status %d", code)
	}

	viewers := mediaSwitch.ListViewers()
	if n := len(viewers); n != 1 {
		t.Fatalf("a renewal must replace, not duplicate, the subscription; switch holds %d viewer(s)", n)
	}
	if viewers[0].ID != "cascade-"+fixtureBroadcastID+"-"+fixtureRelayNodeID {
		t.Fatalf("unexpected viewer id after renewal: %q", viewers[0].ID)
	}
}
