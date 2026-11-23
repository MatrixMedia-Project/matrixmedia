package main

import (
	"context"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/pion/webrtc/v4"
)

type fakeResolver struct {
	result   *whipResolvedKey
	err      error
	lastHash string
}

func (f *fakeResolver) ResolveKey(_ context.Context, hash string) (*whipResolvedKey, error) {
	f.lastHash = hash
	return f.result, f.err
}

func whipMux() *http.ServeMux {
	mux := http.NewServeMux()
	mux.HandleFunc("POST /whip", whipCreate)
	mux.HandleFunc("POST /whip/{key}", whipCreate)
	mux.HandleFunc("DELETE /whip/{session}", whipDelete)
	mux.HandleFunc("PATCH /whip/{session}", whipPatch)
	return mux
}

func setupWHIP(t *testing.T, f *fakeResolver) {
	t.Helper()
	setSwitch(NewMediaSwitch())
	whipReg = newWHIPRegistry(f)
	t.Setenv("MM_WHIP_ENABLED", "true")
}

func validOfferSDP(t *testing.T) string {
	t.Helper()
	pc, err := webrtc.NewPeerConnection(webrtc.Configuration{})
	if err != nil {
		t.Fatal(err)
	}
	defer pc.Close()
	if _, err := pc.AddTransceiverFromKind(webrtc.RTPCodecTypeVideo, webrtc.RTPTransceiverInit{Direction: webrtc.RTPTransceiverDirectionSendonly}); err != nil {
		t.Fatal(err)
	}
	if _, err := pc.AddTransceiverFromKind(webrtc.RTPCodecTypeAudio, webrtc.RTPTransceiverInit{Direction: webrtc.RTPTransceiverDirectionSendonly}); err != nil {
		t.Fatal(err)
	}
	offer, err := pc.CreateOffer(nil)
	if err != nil {
		t.Fatal(err)
	}
	gather := webrtc.GatheringCompletePromise(pc)
	if err := pc.SetLocalDescription(offer); err != nil {
		t.Fatal(err)
	}
	<-gather
	return pc.LocalDescription().SDP
}

func whipPost(t *testing.T, mux *http.ServeMux, url, key, sdp string) *httptest.ResponseRecorder {
	t.Helper()
	req := httptest.NewRequest("POST", url, strings.NewReader(sdp))
	req.Header.Set("Content-Type", "application/sdp")
	if key != "" {
		req.Header.Set("Authorization", "Bearer "+key)
	}
	rec := httptest.NewRecorder()
	mux.ServeHTTP(rec, req)
	return rec
}

const goodResolve = "stream-abc123"

func okResolver() *fakeResolver {
	return &fakeResolver{result: &whipResolvedKey{CreatorID: "@alice:hs", StreamID: "abc123", RoomID: "!room:hs"}}
}

func TestWHIPOfferAccepted(t *testing.T) {
	f := okResolver()
	setupWHIP(t, f)
	rec := whipPost(t, whipMux(), "/whip", "secret-key", validOfferSDP(t))
	if rec.Code != http.StatusCreated {
		t.Fatalf("want 201, got %d: %s", rec.Code, rec.Body.String())
	}
	loc := rec.Header().Get("Location")
	if !strings.HasPrefix(loc, "/whip/") || len(loc) < 20 {
		t.Fatalf("bad Location %q", loc)
	}
	if ct := rec.Header().Get("Content-Type"); !strings.HasPrefix(ct, "application/sdp") {
		t.Fatalf("bad Content-Type %q", ct)
	}
	ans, _ := io.ReadAll(rec.Body)
	if !strings.Contains(string(ans), "v=0") || !strings.Contains(string(ans), "m=video") {
		t.Fatalf("answer is not SDP: %q", ans)
	}
	if got := mediaSwitch.GetSource(goodResolve); got == nil {
		t.Fatalf("source %s not registered", goodResolve)
	}
	// Resolver must have seen only the SHA-256 hash, never the raw key.
	if f.lastHash != hashStreamKey("secret-key") || strings.Contains(f.lastHash, "secret") {
		t.Fatalf("resolver saw %q", f.lastHash)
	}
}

func TestWHIPUnknownKeyRejected(t *testing.T) {
	setupWHIP(t, &fakeResolver{err: ErrWHIPKeyUnknown})
	rec := whipPost(t, whipMux(), "/whip", "nope", validOfferSDP(t))
	if rec.Code != http.StatusUnauthorized {
		t.Fatalf("want 401, got %d", rec.Code)
	}
	if n := len(mediaSwitch.ListSources()); n != 0 {
		t.Fatalf("%d sources registered on rejected key", n)
	}
}

func TestWHIPRevokedAndSuspendedRejected(t *testing.T) {
	for _, err := range []error{ErrWHIPKeyRevoked, ErrWHIPKeySuspended} {
		setupWHIP(t, &fakeResolver{err: err})
		rec := whipPost(t, whipMux(), "/whip", "old-key", validOfferSDP(t))
		if rec.Code != http.StatusForbidden {
			t.Fatalf("%v: want 403, got %d", err, rec.Code)
		}
	}
}

func TestWHIPResolverErrorFailsClosed(t *testing.T) {
	setupWHIP(t, &fakeResolver{err: errors.New("connection refused")})
	rec := whipPost(t, whipMux(), "/whip", "k", validOfferSDP(t))
	if rec.Code != http.StatusBadGateway {
		t.Fatalf("want 502, got %d", rec.Code)
	}
}

func TestWHIPDeleteEndsSource(t *testing.T) {
	setupWHIP(t, okResolver())
	mux := whipMux()
	rec := whipPost(t, mux, "/whip", "k1", validOfferSDP(t))
	if rec.Code != http.StatusCreated {
		t.Fatalf("publish: %d", rec.Code)
	}
	req := httptest.NewRequest("DELETE", rec.Header().Get("Location"), nil)
	drec := httptest.NewRecorder()
	mux.ServeHTTP(drec, req)
	if drec.Code != http.StatusOK {
		t.Fatalf("DELETE want 200, got %d", drec.Code)
	}
	if mediaSwitch.GetSource(goodResolve) != nil {
		t.Fatal("source still registered after DELETE")
	}
	drec2 := httptest.NewRecorder()
	mux.ServeHTTP(drec2, httptest.NewRequest("DELETE", rec.Header().Get("Location"), nil))
	if drec2.Code != http.StatusNotFound {
		t.Fatalf("second DELETE want 404, got %d", drec2.Code)
	}
}

func TestWHIPSecondPublisherConflict(t *testing.T) {
	setupWHIP(t, okResolver())
	mux := whipMux()
	r1 := whipPost(t, mux, "/whip", "k1", validOfferSDP(t))
	if r1.Code != http.StatusCreated {
		t.Fatalf("first: %d", r1.Code)
	}
	first := mediaSwitch.GetSource(goodResolve)
	r2 := whipPost(t, mux, "/whip", "k1", validOfferSDP(t))
	if r2.Code != http.StatusConflict {
		t.Fatalf("second: want 409, got %d", r2.Code)
	}
	if mediaSwitch.GetSource(goodResolve) != first {
		t.Fatal("second publish displaced the first source")
	}
}

func TestWHIPPatchTrickle(t *testing.T) {
	setupWHIP(t, okResolver())
	mux := whipMux()
	rec := whipPost(t, mux, "/whip", "k1", validOfferSDP(t))
	frag := "a=candidate:1 1 udp 1 127.0.0.1 9 typ host\r\n"
	req := httptest.NewRequest("PATCH", rec.Header().Get("Location"), strings.NewReader(frag))
	req.Header.Set("Content-Type", "application/trickle-ice-sdpfrag")
	prec := httptest.NewRecorder()
	mux.ServeHTTP(prec, req)
	if prec.Code != http.StatusNoContent {
		t.Fatalf("PATCH want 204, got %d", prec.Code)
	}
	bad := httptest.NewRecorder()
	mux.ServeHTTP(bad, httptest.NewRequest("PATCH", "/whip/deadbeefdeadbeefdeadbeefdeadbeef", strings.NewReader(frag)))
	if bad.Code != http.StatusNotFound {
		t.Fatalf("PATCH unknown session want 404, got %d", bad.Code)
	}
}

func TestWHIPBadInputsRegisterNothing(t *testing.T) {
	setupWHIP(t, okResolver())
	mux := whipMux()
	if rec := whipPost(t, mux, "/whip", "k", "this is not sdp"); rec.Code != http.StatusBadRequest {
		t.Fatalf("bad SDP: want 400, got %d", rec.Code)
	}
	req := httptest.NewRequest("POST", "/whip", strings.NewReader(validOfferSDP(t)))
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer k")
	rec := httptest.NewRecorder()
	mux.ServeHTTP(rec, req)
	if rec.Code != http.StatusUnsupportedMediaType {
		t.Fatalf("wrong CT: want 415, got %d", rec.Code)
	}
	if rec := whipPost(t, mux, "/whip", "", validOfferSDP(t)); rec.Code != http.StatusUnauthorized {
		t.Fatalf("no key: want 401, got %d", rec.Code)
	}
	if n := len(mediaSwitch.ListSources()); n != 0 {
		t.Fatalf("%d sources after bad inputs", n)
	}
}

func TestWHIPPathKeyAndDisabledGate(t *testing.T) {
	setupWHIP(t, okResolver())
	if rec := whipPost(t, whipMux(), "/whip/path-key", "", validOfferSDP(t)); rec.Code != http.StatusCreated {
		t.Fatalf("path key: want 201, got %d", rec.Code)
	}
	setSwitch(NewMediaSwitch())
	t.Setenv("MM_WHIP_ENABLED", "false")
	if rec := whipPost(t, whipMux(), "/whip", "k", validOfferSDP(t)); rec.Code != http.StatusServiceUnavailable {
		t.Fatalf("disabled: want 503, got %d", rec.Code)
	}
}

func TestWHIPHTTPResolverStatusMapping(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/_mm/internal/ingest/resolve-key" {
			w.WriteHeader(500)
			return
		}
		_, _ = io.Copy(io.Discard, r.Body)
		w.Header().Set("Content-Type", "application/json")
		_, _ = io.WriteString(w, `{"creator_id":"@a:hs","stream_id":"s1","room_id":"!r:hs"}`)
	}))
	defer srv.Close()
	res := &httpWHIPKeyResolver{baseURL: srv.URL, client: srv.Client()}
	got, err := res.ResolveKey(context.Background(), hashStreamKey("k"))
	if err != nil || got.StreamID != "s1" || got.sourceID() != "stream-s1" {
		t.Fatalf("got %+v err %v", got, err)
	}
	srv2 := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { w.WriteHeader(404) }))
	defer srv2.Close()
	res2 := &httpWHIPKeyResolver{baseURL: srv2.URL, client: srv2.Client()}
	if _, err := res2.ResolveKey(context.Background(), "x"); !errors.Is(err, ErrWHIPKeyUnknown) {
		t.Fatalf("404 mapping: %v", err)
	}
	srv3 := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(403)
		_, _ = io.WriteString(w, `{"reason":"revoked"}`)
	}))
	defer srv3.Close()
	res3 := &httpWHIPKeyResolver{baseURL: srv3.URL, client: srv3.Client()}
	if _, err := res3.ResolveKey(context.Background(), "x"); !errors.Is(err, ErrWHIPKeyRevoked) {
		t.Fatalf("403 mapping: %v", err)
	}
}
