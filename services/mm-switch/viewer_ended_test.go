// SPDX-License-Identifier: Apache-2.0
//
// When a broadcast ends, its viewers must hear about it at once.
//
// Removing a programme source used to only detach its viewers: their PeerConnections
// stayed up, the picture went black, and the apps learned the stream was over from their
// 15 s poll. Closing the PeerConnection is not a fast signal either: a libwebrtc client
// keeps connectionState "connected" after the remote DTLS close (per spec) and only
// reaches "disconnected" ~7 s and "failed" ~17 s later (measured 2026-10-08). The apps
// therefore open an "mm-control" data channel, and the switch tells them over it.

package main

import (
	"encoding/json"
	"testing"
	"time"

	"github.com/pion/webrtc/v4"
	"github.com/prometheus/client_golang/prometheus/testutil"
)

// viewerApp is the phone's side of a viewer connection, wired to a real switch Viewer.
type viewerApp struct {
	v        *Viewer
	pc       *webrtc.PeerConnection
	messages chan string
	ctlClose chan struct{}
}

// connectViewerApp connects an app to the switch as viewer `id`. withControl decides
// whether the app opens the mm-control channel (new apps do, old builds don't).
func connectViewerApp(t *testing.T, ms *MediaSwitch, id string, withControl bool) *viewerApp {
	t.Helper()
	appPC := mustPC(t, rigSettings(5*time.Second))
	srvPC := mustPC(t, rigSettings(5*time.Second))
	v, err := NewViewer(id, srvPC, ms)
	if err != nil {
		t.Fatalf("NewViewer: %v", err)
	}
	ms.AddViewer(id, v)

	app := &viewerApp{v: v, pc: appPC, messages: make(chan string, 8), ctlClose: make(chan struct{})}
	for _, kind := range []webrtc.RTPCodecType{webrtc.RTPCodecTypeVideo, webrtc.RTPCodecTypeAudio} {
		if _, err := appPC.AddTransceiverFromKind(kind,
			webrtc.RTPTransceiverInit{Direction: webrtc.RTPTransceiverDirectionRecvonly}); err != nil {
			t.Fatalf("transceiver: %v", err)
		}
	}
	ready := make(chan struct{})
	if withControl {
		dc, err := appPC.CreateDataChannel(controlChannelLabel, nil)
		if err != nil {
			t.Fatalf("data channel: %v", err)
		}
		dc.OnOpen(func() { close(ready) })
		dc.OnMessage(func(m webrtc.DataChannelMessage) { app.messages <- string(m.Data) })
		dc.OnClose(func() { close(app.ctlClose) })
	} else {
		appPC.OnConnectionStateChange(func(s webrtc.PeerConnectionState) {
			if s == webrtc.PeerConnectionStateConnected {
				select {
				case <-ready:
				default:
					close(ready)
				}
			}
		})
	}

	offer, err := appPC.CreateOffer(nil)
	if err != nil {
		t.Fatalf("offer: %v", err)
	}
	gathered := webrtc.GatheringCompletePromise(appPC)
	_ = appPC.SetLocalDescription(offer)
	<-gathered
	if err := srvPC.SetRemoteDescription(*appPC.LocalDescription()); err != nil {
		t.Fatalf("switch SetRemoteDescription: %v", err)
	}
	answer, err := srvPC.CreateAnswer(nil)
	if err != nil {
		t.Fatalf("answer: %v", err)
	}
	gathered = webrtc.GatheringCompletePromise(srvPC)
	_ = srvPC.SetLocalDescription(answer)
	<-gathered
	if err := appPC.SetRemoteDescription(*srvPC.LocalDescription()); err != nil {
		t.Fatalf("app SetRemoteDescription: %v", err)
	}

	select {
	case <-ready:
	case <-time.After(10 * time.Second):
		t.Fatal("viewer app never connected")
	}
	if withControl {
		// The switch registers the channel from its OnDataChannel callback, which can
		// trail the app's OnOpen by a moment.
		waitUntil(t, 5*time.Second, "switch sees the control channel", func() bool {
			v.mu.RLock()
			defer v.mu.RUnlock()
			return v.control != nil
		})
	}
	t.Cleanup(func() { _ = appPC.Close(); v.Close() })
	return app
}

// watch puts the viewer on `sourceID` the way mm-core does and waits until it is subscribed.
func watch(t *testing.T, ms *MediaSwitch, app *viewerApp, sourceID string) {
	t.Helper()
	if err := ms.SwitchViewer(app.v.id, sourceID); err != nil {
		t.Fatalf("SwitchViewer(%s): %v", sourceID, err)
	}
	waitUntil(t, 5*time.Second, "viewer subscribed to "+sourceID, func() bool {
		app.v.mu.RLock()
		defer app.v.mu.RUnlock()
		return app.v.pendingSource == nil && app.v.unsubscribe != nil
	})
}

func viewerRegistered(ms *MediaSwitch, id string) bool {
	ms.mu.RLock()
	defer ms.mu.RUnlock()
	_, ok := ms.viewers[id]
	return ok
}

func expectEnded(t *testing.T, app *viewerApp, sourceID string) {
	t.Helper()
	select {
	case raw := <-app.messages:
		var msg map[string]string
		if err := json.Unmarshal([]byte(raw), &msg); err != nil {
			t.Fatalf("control message %q is not JSON: %v", raw, err)
		}
		if msg["type"] != "ended" || msg["source"] != sourceID {
			t.Fatalf("control message = %q, want type=ended source=%s", raw, sourceID)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("app never heard that the broadcast ended")
	}
	select {
	case <-app.ctlClose:
	case <-time.After(3 * time.Second):
		t.Fatal("switch did not hang up after announcing the end")
	}
}

func closedEgressFor(source string) int64 {
	closedEgress.Lock()
	defer closedEgress.Unlock()
	return closedEgress.bytes[source]
}

func useShortEndedGrace(t *testing.T) {
	old := viewerEndedGrace
	viewerEndedGrace = 20 * time.Millisecond
	t.Cleanup(func() { viewerEndedGrace = old })
}

// The host ends: the viewer's app is told at once, then the switch hangs up and forgets it.
func TestEndingAProgrammeTellsItsViewersAtOnce(t *testing.T) {
	useShortEndedGrace(t)
	ms := NewMediaSwitch()
	ms.AddSource("stream-a", &fakeSource{typ: "webrtc"})
	app := connectViewerApp(t, ms, "viewer-a--alice", true)
	watch(t, ms, app, "stream-a")
	before := testutil.ToFloat64(viewerProgrammeEndedTotal.WithLabelValues("yes"))
	app.v.egressBytes.Add(1234) // what it had been sent before the end
	egressBefore := closedEgressFor("stream-a")

	ms.RemoveSource("stream-a")

	expectEnded(t, app, "stream-a")
	waitUntil(t, 3*time.Second, "viewer removed from the switch", func() bool {
		return !viewerRegistered(ms, "viewer-a--alice")
	})
	// Like any departing viewer, its bytes go into the meter rather than vanish with it.
	waitUntil(t, 3*time.Second, "ended viewer's egress folded into the meter", func() bool {
		return closedEgressFor("stream-a")-egressBefore == 1234
	})
	if d := testutil.ToFloat64(viewerProgrammeEndedTotal.WithLabelValues("yes")) - before; d != 1 {
		t.Fatalf("mm_switch_viewer_programme_ended_total{control=yes} grew by %v, want 1", d)
	}
}

// Mid-ad: the viewer is on its own ad source, but the programme it is watching ended. It
// must not be left on the ad, waiting to be switched back to a stream that is gone.
func TestEndingAProgrammeReachesViewersOnAnAdBreak(t *testing.T) {
	useShortEndedGrace(t)
	ms := NewMediaSwitch()
	ms.AddSource("stream-a", &fakeSource{typ: "webrtc"})
	ms.AddSource("ad--bob-1", &fakeSource{typ: "file"})
	app := connectViewerApp(t, ms, "viewer-a--bob", true)
	watch(t, ms, app, "stream-a")
	watch(t, ms, app, "ad--bob-1")

	ms.RemoveSource("stream-a")

	expectEnded(t, app, "stream-a")
}

// An app build without the control channel still gets hung up on (it notices late, as
// before), and the switch stops holding its connection.
func TestEndingAProgrammeHangsUpOnOldApps(t *testing.T) {
	useShortEndedGrace(t)
	ms := NewMediaSwitch()
	ms.AddSource("stream-a", &fakeSource{typ: "webrtc"})
	app := connectViewerApp(t, ms, "viewer-a--carol", false)
	watch(t, ms, app, "stream-a")
	before := testutil.ToFloat64(viewerProgrammeEndedTotal.WithLabelValues("no"))

	ms.RemoveSource("stream-a")

	waitUntil(t, 3*time.Second, "old app's viewer removed", func() bool {
		return !viewerRegistered(ms, "viewer-a--carol")
	})
	waitUntil(t, 3*time.Second, "old app's connection closed by the switch", func() bool {
		return app.v.pc.ConnectionState() == webrtc.PeerConnectionStateClosed
	})
	if d := testutil.ToFloat64(viewerProgrammeEndedTotal.WithLabelValues("no")) - before; d != 1 {
		t.Fatalf("mm_switch_viewer_programme_ended_total{control=no} grew by %v, want 1", d)
	}
}

// Removing an ad source is routine (every pre-roll ends that way) and must not end anything.
// Neither may ending one broadcast touch the viewers of another.
func TestOnlyTheEndedProgrammesViewersAreEnded(t *testing.T) {
	useShortEndedGrace(t)
	ms := NewMediaSwitch()
	ms.AddSource("stream-a", &fakeSource{typ: "webrtc"})
	ms.AddSource("stream-b", &fakeSource{typ: "webrtc"})
	ms.AddSource("ad--dave-1", &fakeSource{typ: "file"})
	onAd := connectViewerApp(t, ms, "viewer-a--dave", true)
	watch(t, ms, onAd, "stream-a")
	watch(t, ms, onAd, "ad--dave-1")
	onB := connectViewerApp(t, ms, "viewer-b--erin", true)
	watch(t, ms, onB, "stream-b")

	ms.RemoveSource("ad--dave-1")
	ms.RemoveSource("stream-a")
	expectEnded(t, onAd, "stream-a")

	select {
	case raw := <-onB.messages:
		t.Fatalf("a viewer of stream-b was told %q when stream-a ended", raw)
	case <-time.After(300 * time.Millisecond):
	}
	if !viewerRegistered(ms, "viewer-b--erin") {
		t.Fatal("ending stream-a removed a viewer of stream-b")
	}
}
