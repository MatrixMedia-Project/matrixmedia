package main

import (
	"bytes"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"
)

// ─── FR-347f: a viewer token only opens its own stream's source ──────────────
//
// mm-core mints a viewer token at /join with sub = "viewer-{stream_id}-{user}" and hands
// the viewer "stream-{stream_id}" as the source to watch. Before the pin, the offer
// handler switched the viewer to whatever `source_id` the body named — so a token from a
// FREE stream plus a PAID stream's id gave the paid video without entitlement.

const (
	freeStream = "0a1b2c3d-1111-4222-8333-444455556666"
	paidStream = "9f8e7d6c-aaaa-4bbb-8ccc-ddddeeeeffff"
)

func viewerOf(stream string) switchIdentity {
	return switchIdentity{Role: roleViewer, Subject: "viewer-" + stream + "--alice-example.org", Authenticated: true}
}

// THE ONE THAT MATTERS.
func TestViewerTokenCannotOpenAnotherStreamsSource(t *testing.T) {
	if err := pinViewerSource(viewerOf(freeStream), "stream-"+paidStream); err == nil {
		t.Fatal("a viewer token from one stream opened ANOTHER stream's source — " +
			"a free stream's token unlocks every paid stream")
	}
}

func TestViewerTokenMayOpenItsOwnStreamsSource(t *testing.T) {
	// The live path: every shipped client sends switch_source_id from its own /join
	// response. If this fails, nobody can watch anything.
	if err := pinViewerSource(viewerOf(freeStream), "stream-"+freeStream); err != nil {
		t.Fatalf("a viewer must be able to open its own stream's source: %v", err)
	}
}

func TestViewerOfferWithoutASourceIsNotPinned(t *testing.T) {
	// `source_id` is optional: without it the viewer is registered and the server
	// attaches it later (POST /api/switch, server role).
	if err := pinViewerSource(viewerOf(freeStream), ""); err != nil {
		t.Fatalf("an offer with no source must pass: %v", err)
	}
}

func TestViewerTokenCannotOpenANonStreamSource(t *testing.T) {
	// Ad sources and file sources are attached by the control plane, never by the
	// viewer's own offer.
	for _, src := range []string{"ad-1234abcd", "camera", "stream-"} {
		if err := pinViewerSource(viewerOf(freeStream), src); err == nil {
			t.Fatalf("a viewer token opened non-stream source %q", src)
		}
	}
}

func TestServerRoleIsNotSourcePinned(t *testing.T) {
	server := switchIdentity{Role: roleServer, Subject: "mm-core", Authenticated: true}
	if err := pinViewerSource(server, "stream-"+paidStream); err != nil {
		t.Fatalf("the control plane must not be source-pinned: %v", err)
	}
}

func TestUnauthenticatedOfferKeepsLegacyBehaviour(t *testing.T) {
	// No secret configured: nothing to pin against (same rule as bindSubject).
	if err := pinViewerSource(switchIdentity{}, "stream-"+paidStream); err != nil {
		t.Fatalf("an unsecured origin must behave exactly as before: %v", err)
	}
}

// End to end through the real middleware + handler: the refusal is a 403 that happens
// before any SDP work or registration, and the legitimate request is not refused.
func TestViewerOfferForAnotherStreamIsRefusedBeforeAnythingHappens(t *testing.T) {
	setSwitch(NewMediaSwitch())
	paid := &victimSource{}
	mediaSwitch.AddSource("stream-"+paidStream, paid)

	const secret = "source-pin-test"
	handler := wrapAuth(secret, []string{roleServer, roleViewer}, handleViewerOffer)
	viewerID := "viewer-" + freeStream + "--alice-example.org"
	token := generateToken(secret, roleViewer, viewerID, 60)

	post := func(source string) int {
		raw, _ := json.Marshal(map[string]any{"id": viewerID, "source_id": source, "offer": badOffer()})
		req := httptest.NewRequest("POST", "/api/viewers/offer", bytes.NewReader(raw))
		req.Header.Set("Authorization", "Bearer "+token)
		rec := httptest.NewRecorder()
		handler.ServeHTTP(rec, req)
		return rec.Code
	}

	if code := post("stream-" + paidStream); code != http.StatusForbidden {
		t.Fatalf("an offer for another stream's source must be refused with 403, got %d", code)
	}
	if n := len(mediaSwitch.ListViewers()); n != 0 {
		t.Fatalf("a refused offer must register nothing; switch holds %d viewer(s)", n)
	}
	// The own-stream offer gets past the pin (it then fails on the junk SDP, not on auth).
	if code := post("stream-" + freeStream); code == http.StatusForbidden {
		t.Fatal("an offer for the viewer's OWN stream was refused")
	}
}
