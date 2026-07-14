package main

import (
	"net/http"
	"sync/atomic"
	"testing"
)

// victimSource stands in for a live broadcast already registered in the switch.
type victimSource struct {
	stopped atomic.Bool
}

func (s *victimSource) Type() string                                { return "victim" }
func (s *victimSource) IsActive() bool                              { return !s.stopped.Load() }
func (s *victimSource) Subscribe(string, PacketHandler) func()      { return func() {} }
func (s *victimSource) RequestKeyframe()                            {}
func (s *victimSource) Stop()                                       { s.stopped.Store(true) }

var _ Source = (*victimSource)(nil)

// THE CRITICAL ONE.
//
// Source ids are caller-supplied and AddSource overwrites without checking. The
// failed-offer cleanup added for the PeerConnection leak originally removed BY ID — so a
// single POST carrying junk SDP and a live broadcast's id would displace that broadcast's
// map entry, then stop its source and DetachSource every viewer watching it. One request,
// somebody else's stream goes black and cannot be rejoined.
//
// A failed offer must not touch the switch's state at all.
func TestFailedPublishOfferCannotEvictAnotherPublishersLiveSource(t *testing.T) {
	mediaSwitch = NewMediaSwitch()

	const victimID = "live-broadcast"
	victim := &victimSource{}
	mediaSwitch.AddSource(victimID, victim)

	// The attack: a well-formed request carrying the victim's id and unusable SDP.
	code := postOffer(t, handlePublishOffer, "/api/sources/publish/offer", map[string]any{
		"id":    victimID,
		"offer": badOffer(),
	})
	if code == http.StatusOK {
		t.Fatal("malformed SDP unexpectedly negotiated")
	}

	if victim.stopped.Load() {
		t.Fatal("a failed offer STOPPED another publisher's live source — " +
			"one junk-SDP POST would black out a live broadcast")
	}

	sources := mediaSwitch.ListSources()
	if len(sources) != 1 {
		t.Fatalf("the victim's registration must survive; switch holds %d source(s)", len(sources))
	}

	// And it must still be the victim's object, not a dead one left by the failed offer.
	mediaSwitch.mu.RLock()
	got := mediaSwitch.sources[victimID]
	mediaSwitch.mu.RUnlock()
	if got != Source(victim) {
		t.Fatalf("the victim's id was hijacked by the failed offer: %T", got)
	}
}

// Same hazard on the viewer side: a failed viewer offer must not close a live viewer that
// happens to hold the same id.
func TestFailedViewerOfferCannotCloseAnotherLiveViewer(t *testing.T) {
	mediaSwitch = NewMediaSwitch()

	const victimID = "live-viewer"
	victim := &Viewer{id: victimID, videoTrack: newTestVideoTrack(t)}
	mediaSwitch.AddViewer(victimID, victim)

	code := postOffer(t, handleViewerOffer, "/api/viewers/offer", map[string]any{
		"id":    victimID,
		"offer": badOffer(),
	})
	if code == http.StatusOK {
		t.Fatal("malformed SDP unexpectedly negotiated")
	}

	victim.mu.RLock()
	closed := victim.closed
	victim.mu.RUnlock()
	if closed {
		t.Fatal("a failed offer CLOSED another live viewer")
	}
	if n := len(mediaSwitch.ListViewers()); n != 1 {
		t.Fatalf("the victim's registration must survive; switch holds %d viewer(s)", n)
	}
}
