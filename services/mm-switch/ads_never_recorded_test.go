// services/mm-switch/ads_never_recorded_test.go
package main

import (
	"bytes"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/pion/rtp"
)

const (
	programmeMark byte = 0x5E
	adMark        byte = 0xAD
)

// markedVP8 is a one-packet VP8 frame whose payload ends in four marker bytes, so a frame's
// origin (programme or ad) survives into what the recorder writes.
func markedVP8(key bool, seq uint16, ts uint32, mark byte) *rtp.Packet {
	p := makeVP8Pkt(key, seq, ts)
	p.Payload = append(p.Payload, mark, mark, mark, mark)
	return p
}

// T-REC-1 (internal spec 2026-10-11 §5.12): switching a viewer into an ad break must not change
// what the recorder captures. It taps the publisher's source, never a viewer's mix; this pins it.
func TestAnAdOnAViewerNeverReachesTheRecording(t *testing.T) {
	t.Setenv(recorderIsolationEnv, recorderModeInline)
	prog := newTestWebRTCSource("stream-rec")
	rec, capture := recorderWithCapture(t, prog, "rec-ads")
	defer rec.Finalise()
	ad := &fakeSource{typ: "file"}
	v := capturingViewer(t, "viewer-ads", 0)
	// The recorder is registered with the switch the viewer belongs to, under the source the viewer
	// is billed for. If Viewer.deliver (or anything it calls) ever looked the recorder up there and
	// tapped the viewer's packets into it, the ad would be recorded and this test would fail.
	ms := NewMediaSwitch()
	ms.AddSource("stream-rec", prog)
	ms.RegisterRecorder("stream-rec", rec)
	v.sw = ms
	v.billingSource = "stream-rec"

	attach(v, "stream-rec", prog)
	prog.fanout("video", markedVP8(true, 1, 0, programmeMark))
	attach(v, "ad-viewer-ads-1", ad) // the break starts for this viewer
	ad.emit("video", markedVP8(true, 1, 0, adMark))
	ad.emit("video", markedVP8(false, 2, 3000, adMark))
	prog.fanout("video", markedVP8(false, 2, 3000, programmeMark)) // the programme carries on
	prog.fanout("video", markedVP8(false, 3, 6000, programmeMark))

	rec.Finalise() // flushes the last frame the sample builder still holds; the deferred call is then a no-op
	blocks := capture.snapshot()
	if len(blocks) != 3 {
		t.Fatalf("the recorder wrote %d blocks, want the 3 programme frames", len(blocks))
	}
	for i, b := range blocks {
		if bytes.Contains(b.data, []byte{adMark, adMark, adMark, adMark}) {
			t.Fatalf("block %d carries ad bytes", i)
		}
		if !bytes.Contains(b.data, []byte{programmeMark, programmeMark, programmeMark, programmeMark}) {
			t.Fatalf("block %d is not programme", i)
		}
	}
	// The viewer really got the ad, so the test exercised a switch.
	sawAd := false
drain:
	for {
		select {
		case it := <-v.queue:
			if it.kind == "video" && bytes.Contains(it.pkt.Payload, []byte{adMark, adMark}) {
				sawAd = true
			}
		default:
			break drain
		}
	}
	if !sawAd {
		t.Fatal("the viewer never received the ad: no switch was exercised")
	}
}

// T-REC-2: recording starts only on a direct-publish source; a file source is refused.
func TestRecordingRefusesAFileSource(t *testing.T) {
	ms := NewMediaSwitch()
	setSwitch(ms)
	t.Cleanup(func() { setSwitch(NewMediaSwitch()) })
	ms.AddSource("ad-file", &fakeSource{typ: "file"})
	req := httptest.NewRequest("POST", "/api/sources/ad-file/record", strings.NewReader(`{"recording_id":"rec-x"}`))
	req.SetPathValue("id", "ad-file")
	w := httptest.NewRecorder()
	handleStartOrResumeRecording(w, req)
	if w.Code != 400 || !strings.Contains(w.Body.String(), "webrtc") {
		t.Fatalf("status %d body %q, want 400 naming webrtc sources", w.Code, w.Body.String())
	}
}
