package main

import (
	"bytes"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"

	"github.com/pion/webrtc/v4"
)

// A malformed offer must leave nothing behind: no PeerConnection, and no half-registered
// source/viewer in the switch. Before the fix each failing request leaked a PC (ICE agent,
// UDP sockets, goroutines) AND left a dead entry in the switch's map, so a client sending
// junk SDP in a loop could exhaust the process.
//
// The switch maps are the observable proxy: if cleanup ran, they are empty.

func badOffer() webrtc.SessionDescription {
	// Well-formed JSON, structurally invalid SDP — parses into the request struct, then
	// fails inside SetRemoteDescription, which is exactly the path that used to leak.
	return webrtc.SessionDescription{Type: webrtc.SDPTypeOffer, SDP: "this is not sdp"}
}

func postOffer(t *testing.T, handler http.HandlerFunc, path string, body any) int {
	t.Helper()
	raw, err := json.Marshal(body)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	req := httptest.NewRequest("POST", path, bytes.NewReader(raw))
	rec := httptest.NewRecorder()
	handler(rec, req)
	return rec.Code
}

func TestFailedPublishOfferLeavesNoSource(t *testing.T) {
	mediaSwitch = NewMediaSwitch()

	const attempts = 5
	for i := 0; i < attempts; i++ {
		code := postOffer(t, handlePublishOffer, "/api/sources/publish/offer", map[string]any{
			"id":    "leaky-publisher",
			"offer": badOffer(),
		})
		if code == http.StatusOK {
			t.Fatalf("attempt %d: malformed SDP unexpectedly succeeded", i)
		}
	}

	if n := len(mediaSwitch.ListSources()); n != 0 {
		t.Fatalf("after %d failed offers the switch still holds %d source(s); "+
			"the failed-offer cleanup did not run", attempts, n)
	}
}

func TestFailedViewerOfferLeavesNoViewer(t *testing.T) {
	mediaSwitch = NewMediaSwitch()

	const attempts = 5
	for i := 0; i < attempts; i++ {
		code := postOffer(t, handleViewerOffer, "/api/viewers/offer", map[string]any{
			"id":    "leaky-viewer",
			"offer": badOffer(),
		})
		if code == http.StatusOK {
			t.Fatalf("attempt %d: malformed SDP unexpectedly succeeded", i)
		}
	}

	if n := len(mediaSwitch.ListViewers()); n != 0 {
		t.Fatalf("after %d failed offers the switch still holds %d viewer(s); "+
			"the failed-offer cleanup did not run", attempts, n)
	}
}
