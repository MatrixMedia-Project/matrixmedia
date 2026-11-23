package main

import (
	"bytes"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"strings"
	"sync"
	"time"

	"github.com/pion/webrtc/v4"
)

// WHIP ingest (RFC 9725): OBS and other WHIP encoders publish with the
// creator's stream key as a bearer credential. The key is NOT an HMAC
// token (see auth.go), so these routes deliberately do not use
// authMiddleware: the key is hashed and resolved to a creator + stream
// through whipKeyResolver, then the broadcast joins the exact same
// WebRTCSource path as an app publisher (POST /api/publish/offer).
//
// The raw key is never logged, never used as a metric label, and never
// leaves this process: only its SHA-256 hex digest reaches the resolver.

var (
	ErrWHIPKeyUnknown   = errors.New("unknown stream key")
	ErrWHIPKeyRevoked   = errors.New("stream key revoked")
	ErrWHIPKeySuspended = errors.New("creator suspended")
)

type whipResolvedKey struct {
	CreatorID string `json:"creator_id"`
	StreamID  string `json:"stream_id"`
	SourceID  string `json:"source_id"`
	RoomID    string `json:"room_id"`
}

func (k *whipResolvedKey) sourceID() string {
	if k.SourceID != "" {
		return k.SourceID
	}
	if k.StreamID != "" {
		return "stream-" + k.StreamID
	}
	return ""
}

type whipKeyResolver interface {
	ResolveKey(ctx context.Context, keyHash string) (*whipResolvedKey, error)
}

// httpWHIPKeyResolver resolves a key hash against mm-core's internal
// ingest endpoint (stream-key feature). Until that route ships, every
// call fails and WHIP fails closed with 502.
type httpWHIPKeyResolver struct {
	baseURL string
	token   string
	client  *http.Client
}

func newHTTPWHIPKeyResolver() *httpWHIPKeyResolver {
	return &httpWHIPKeyResolver{
		baseURL: strings.TrimRight(os.Getenv("MM_CORE_INTERNAL_URL"), "/"),
		token:   os.Getenv("MM_CORE_INTERNAL_TOKEN"),
		client:  &http.Client{Timeout: 5 * time.Second},
	}
}

func (r *httpWHIPKeyResolver) ResolveKey(ctx context.Context, keyHash string) (*whipResolvedKey, error) {
	if r.baseURL == "" {
		return nil, fmt.Errorf("MM_CORE_INTERNAL_URL not configured")
	}
	body, _ := json.Marshal(map[string]string{"key_hash": keyHash})
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, r.baseURL+"/_mm/internal/ingest/resolve-key", bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	if r.token != "" {
		req.Header.Set("Authorization", "Bearer "+r.token)
	}
	resp, err := r.client.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	switch resp.StatusCode {
	case http.StatusOK:
		var out whipResolvedKey
		if err := json.NewDecoder(io.LimitReader(resp.Body, 1<<20)).Decode(&out); err != nil {
			return nil, err
		}
		return &out, nil
	case http.StatusNotFound:
		return nil, ErrWHIPKeyUnknown
	case http.StatusForbidden:
		var e struct {
			Reason string `json:"reason"`
		}
		_ = json.NewDecoder(io.LimitReader(resp.Body, 1<<20)).Decode(&e)
		if e.Reason == "suspended" {
			return nil, ErrWHIPKeySuspended
		}
		return nil, ErrWHIPKeyRevoked
	default:
		return nil, fmt.Errorf("resolver returned %d", resp.StatusCode)
	}
}

func whipEnabled() bool {
	return !strings.EqualFold(strings.TrimSpace(os.Getenv("MM_WHIP_ENABLED")), "false")
}

func hashStreamKey(key string) string {
	sum := sha256.Sum256([]byte(key))
	return hex.EncodeToString(sum[:])
}

type whipSession struct {
	id       string
	sourceID string
	src      *WebRTCSource
}

type whipRegistry struct {
	mu       sync.Mutex
	sessions map[string]*whipSession
	resolver whipKeyResolver
}

func newWHIPRegistry(resolver whipKeyResolver) *whipRegistry {
	return &whipRegistry{sessions: make(map[string]*whipSession), resolver: resolver}
}

var whipReg = newWHIPRegistry(newHTTPWHIPKeyResolver())

func newSessionID() string {
	b := make([]byte, 16)
	_, _ = rand.Read(b)
	return hex.EncodeToString(b)
}

// whipKey extracts the stream key: Bearer header first (OBS's token
// field), then the /whip/{key} path form.
func whipKey(r *http.Request) string {
	if h := r.Header.Get("Authorization"); strings.HasPrefix(h, "Bearer ") {
		if k := strings.TrimSpace(strings.TrimPrefix(h, "Bearer ")); k != "" {
			return k
		}
	}
	return r.PathValue("key")
}

func whipCreate(w http.ResponseWriter, r *http.Request) {
	if !whipEnabled() {
		whipRequestsTotal.WithLabelValues("disabled").Inc()
		http.Error(w, `{"error":"whip ingest disabled"}`, http.StatusServiceUnavailable)
		return
	}
	key := whipKey(r)
	if key == "" {
		whipRequestsTotal.WithLabelValues("unknown_key").Inc()
		http.Error(w, `{"error":"missing stream key"}`, http.StatusUnauthorized)
		return
	}
	resolved, err := whipReg.resolver.ResolveKey(r.Context(), hashStreamKey(key))
	if err != nil {
		switch {
		case errors.Is(err, ErrWHIPKeyUnknown):
			whipRequestsTotal.WithLabelValues("unknown_key").Inc()
			http.Error(w, `{"error":"unknown stream key"}`, http.StatusUnauthorized)
		case errors.Is(err, ErrWHIPKeyRevoked), errors.Is(err, ErrWHIPKeySuspended):
			whipRequestsTotal.WithLabelValues("revoked_key").Inc()
			http.Error(w, `{"error":"stream key not accepted"}`, http.StatusForbidden)
		default:
			whipRequestsTotal.WithLabelValues("resolver_error").Inc()
			log.Printf("[whip] resolver error: %v", err)
			http.Error(w, `{"error":"key resolution failed"}`, http.StatusBadGateway)
		}
		return
	}
	sourceID := resolved.sourceID()
	if sourceID == "" {
		whipRequestsTotal.WithLabelValues("resolver_error").Inc()
		http.Error(w, `{"error":"resolver returned no stream"}`, http.StatusBadGateway)
		return
	}
	if ct := r.Header.Get("Content-Type"); !strings.HasPrefix(ct, "application/sdp") {
		whipRequestsTotal.WithLabelValues("bad_sdp").Inc()
		http.Error(w, `{"error":"Content-Type must be application/sdp"}`, http.StatusUnsupportedMediaType)
		return
	}
	raw, err := io.ReadAll(io.LimitReader(r.Body, 64<<10))
	if err != nil || len(raw) == 0 || !strings.Contains(string(raw), "v=0") {
		whipRequestsTotal.WithLabelValues("bad_sdp").Inc()
		http.Error(w, `{"error":"invalid SDP offer"}`, http.StatusBadRequest)
		return
	}

	pc, err := createPeerConnection()
	if err != nil {
		http.Error(w, `{"error":"internal error"}`, http.StatusInternalServerError)
		return
	}
	src := NewWebRTCSource(sourceID, pc)
	negotiated := false
	defer func() {
		if !negotiated {
			_ = pc.Close()
		}
	}()

	offer := webrtc.SessionDescription{Type: webrtc.SDPTypeOffer, SDP: string(raw)}
	if err := pc.SetRemoteDescription(offer); err != nil {
		whipRequestsTotal.WithLabelValues("bad_sdp").Inc()
		http.Error(w, `{"error":"invalid SDP offer"}`, http.StatusBadRequest)
		return
	}
	answer, err := pc.CreateAnswer(nil)
	if err != nil {
		http.Error(w, `{"error":"internal error"}`, http.StatusInternalServerError)
		return
	}
	gatherComplete := webrtc.GatheringCompletePromise(pc)
	if err := pc.SetLocalDescription(answer); err != nil {
		http.Error(w, `{"error":"internal error"}`, http.StatusInternalServerError)
		return
	}
	<-gatherComplete

	sw := switchRef()
	if !sw.AddSourceIfAbsent(sourceID, src) {
		whipRequestsTotal.WithLabelValues("conflict").Inc()
		http.Error(w, `{"error":"a publisher is already live for this stream key"}`, http.StatusConflict)
		return
	}
	sess := &whipSession{id: newSessionID(), sourceID: sourceID, src: src}
	whipReg.mu.Lock()
	whipReg.sessions[sess.id] = sess
	whipReg.mu.Unlock()
	whipSessionsGauge.Inc()
	whipRequestsTotal.WithLabelValues("accepted").Inc()
	negotiated = true

	w.Header().Set("Content-Type", "application/sdp")
	w.Header().Set("Location", "/whip/"+sess.id)
	w.Header().Set("ETag", `"`+sess.id+`"`)
	w.WriteHeader(http.StatusCreated)
	_, _ = io.WriteString(w, pc.LocalDescription().SDP)
}

func whipSessionFromRequest(w http.ResponseWriter, r *http.Request) *whipSession {
	id := r.PathValue("session")
	whipReg.mu.Lock()
	sess := whipReg.sessions[id]
	whipReg.mu.Unlock()
	if sess == nil {
		http.Error(w, `{"error":"unknown whip session"}`, http.StatusNotFound)
		return nil
	}
	return sess
}

func whipDelete(w http.ResponseWriter, r *http.Request) {
	sess := whipSessionFromRequest(w, r)
	if sess == nil {
		return
	}
	whipReg.mu.Lock()
	delete(whipReg.sessions, sess.id)
	whipReg.mu.Unlock()
	whipSessionsGauge.Dec()
	switchRef().RemoveSourceIf(sess.sourceID, sess.src)
	w.WriteHeader(http.StatusOK)
}

// whipPatch acknowledges trickle-ICE fragments. The server completes ICE
// gathering before answering, so candidates are not required; fragments
// are accepted per RFC 9725 §4.2 but not applied (see plan R7).
func whipPatch(w http.ResponseWriter, r *http.Request) {
	if whipSessionFromRequest(w, r) == nil {
		return
	}
	_, _ = io.Copy(io.Discard, io.LimitReader(r.Body, 64<<10))
	w.WriteHeader(http.StatusNoContent)
}
