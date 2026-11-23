package main

import (
	"bytes"
	"context"
	"crypto/rand"
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
// authMiddleware: the key is resolved to a creator + stream through
// whipKeyResolver, then the broadcast joins the exact same
// WebRTCSource path as an app publisher (POST /api/publish/offer).
//
// The raw key is never logged, never used as a metric label, and never
// stored. It leaves this process exactly once per publish: as the body
// of the internal authenticated resolve call to mm-core (stream-key
// plan §3.4, the pinned contract).

var (
	ErrWHIPKeyUnknown   = errors.New("unknown stream key")
	ErrWHIPKeyRevoked   = errors.New("stream key revoked")
	ErrWHIPKeySuspended = errors.New("creator suspended")
)

// whipResolvedKey mirrors the pinned resolve response (stream-key plan
// §3.4): the creator, the ensured live broadcast, and its Matrix room.
type whipResolvedKey struct {
	CreatorID string `json:"creator_user_id"`
	StreamID  string `json:"stream_id"`
	RoomID    string `json:"room_id"`
}

func (k *whipResolvedKey) sourceID() string {
	if k.StreamID != "" {
		return "stream-" + k.StreamID
	}
	return ""
}

type whipKeyResolver interface {
	ResolveKey(ctx context.Context, key string) (*whipResolvedKey, error)
}

// httpWHIPKeyResolver resolves a stream key against mm-core's pinned
// internal ingest endpoint (stream-key plan §3.4). Any failure to reach
// or authenticate to it fails closed: WHIP answers 502.
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

func (r *httpWHIPKeyResolver) ResolveKey(ctx context.Context, key string) (*whipResolvedKey, error) {
	if r.baseURL == "" {
		return nil, fmt.Errorf("MM_CORE_INTERNAL_URL not configured")
	}
	// The plaintext key travels ONLY on this internal authenticated call.
	body, _ := json.Marshal(map[string]string{"key": key})
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, r.baseURL+"/_mm/internal/v1/ingest/resolve", bytes.NewReader(body))
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
	case http.StatusNotFound, http.StatusBadRequest:
		// Unknown, rotated-away and revoked keys are indistinguishable
		// server-side by design; all are an unknown key at this edge.
		// 400 is a malformed key — also just an unknown key to the client.
		return nil, ErrWHIPKeyUnknown
	case http.StatusUnauthorized:
		// mm-api maps Forbidden to 401; the error code, not the number,
		// distinguishes a suspended creator from a bad service token.
		var e struct {
			Error string `json:"error"`
		}
		_ = json.NewDecoder(io.LimitReader(resp.Body, 1<<20)).Decode(&e)
		if e.Error == "MM_FORBIDDEN" {
			return nil, ErrWHIPKeySuspended
		}
		return nil, fmt.Errorf("resolver auth failed (%s)", e.Error)
	default:
		return nil, fmt.Errorf("resolver returned %d", resp.StatusCode)
	}
}

func whipEnabled() bool {
	return !strings.EqualFold(strings.TrimSpace(os.Getenv("MM_WHIP_ENABLED")), "false")
}

// whipEndNotifier tells mm-core a WHIP broadcast ended
// (POST /_mm/internal/v1/ingest/end). Best-effort by contract: the
// caller never blocks teardown on it.
type whipEndNotifier interface {
	NotifyStreamEnd(ctx context.Context, streamID string) error
}

// NotifyStreamEnd implements whipEndNotifier on the same internal
// base URL + service token as the resolve call.
func (r *httpWHIPKeyResolver) NotifyStreamEnd(ctx context.Context, streamID string) error {
	if r.baseURL == "" {
		return fmt.Errorf("MM_CORE_INTERNAL_URL not configured")
	}
	body, _ := json.Marshal(map[string]string{"stream_id": streamID})
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, r.baseURL+"/_mm/internal/v1/ingest/end", bytes.NewReader(body))
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "application/json")
	if r.token != "" {
		req.Header.Set("Authorization", "Bearer "+r.token)
	}
	resp, err := r.client.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return fmt.Errorf("ingest end returned %d", resp.StatusCode)
	}
	return nil
}

type whipSession struct {
	id       string
	sourceID string
	streamID string
	src      *WebRTCSource
}

type whipRegistry struct {
	mu       sync.Mutex
	sessions map[string]*whipSession
	resolver whipKeyResolver
	notifier whipEndNotifier
}

func newWHIPRegistry(resolver whipKeyResolver) *whipRegistry {
	reg := &whipRegistry{sessions: make(map[string]*whipSession), resolver: resolver}
	if n, ok := resolver.(whipEndNotifier); ok {
		reg.notifier = n
	}
	return reg
}

// claimSession removes the session from the registry exactly once,
// reporting whether this caller won. Only the winner notifies mm-core,
// so a DELETE racing the PC-gone callback notifies at most once.
func (reg *whipRegistry) claimSession(id string) *whipSession {
	reg.mu.Lock()
	sess := reg.sessions[id]
	if sess != nil {
		delete(reg.sessions, id)
	}
	reg.mu.Unlock()
	if sess != nil {
		whipSessionsGauge.Dec()
	}
	return sess
}

// notifyStreamEnd posts the ingest-end notification in its own
// goroutine (5 s timeout, failure only logged): teardown never blocks
// on mm-core. It skips the notification when the switch now holds a
// DIFFERENT source for this session's id — a same-key republish already
// replaced it and the broadcast is still live.
func (reg *whipRegistry) notifyStreamEnd(sess *whipSession) {
	if reg.notifier == nil || sess.streamID == "" {
		return
	}
	if cur := switchRef().GetSource(sess.sourceID); cur != nil {
		if ws, ok := cur.(*WebRTCSource); !ok || ws != sess.src {
			log.Printf("[whip] session %s displaced by a new publisher for %s; broadcast still live, not ending it", sess.id, sess.sourceID)
			return
		}
	}
	notifier, streamID := reg.notifier, sess.streamID
	go func() {
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		if err := notifier.NotifyStreamEnd(ctx, streamID); err != nil {
			log.Printf("[whip] ingest end notify for stream %s failed: %v", streamID, err)
		}
	}()
}

// publisherGone handles a WHIP publisher PC reaching Failed/Closed
// without a DELETE (OBS crash/kill/network drop): claim the session
// and notify mm-core, unless a republish already displaced it.
func (reg *whipRegistry) publisherGone(sess *whipSession) {
	if reg.claimSession(sess.id) == nil {
		return // DELETE (or an earlier gone) already handled it
	}
	reg.notifyStreamEnd(sess)
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
	reg := whipReg
	resolved, err := reg.resolver.ResolveKey(r.Context(), key)
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

	sess := &whipSession{id: newSessionID(), sourceID: sourceID, streamID: resolved.StreamID, src: src}
	// A publisher that dies without DELETE (crash/kill/network drop)
	// still ends the broadcast in mm-core. Set before registration so
	// no gone transition is missed; the registry claim makes it
	// exactly-once against a racing DELETE.
	src.onPublisherGone = func() { reg.publisherGone(sess) }
	sw := switchRef()
	if !sw.AddSourceIfAbsent(sourceID, src) {
		whipRequestsTotal.WithLabelValues("conflict").Inc()
		http.Error(w, `{"error":"a publisher is already live for this stream key"}`, http.StatusConflict)
		return
	}
	reg.mu.Lock()
	reg.sessions[sess.id] = sess
	reg.mu.Unlock()
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
	reg := whipReg
	reg.mu.Lock()
	sess := reg.sessions[id]
	reg.mu.Unlock()
	if sess == nil {
		http.Error(w, `{"error":"unknown whip session"}`, http.StatusNotFound)
		return nil
	}
	return sess
}

func whipDelete(w http.ResponseWriter, r *http.Request) {
	reg := whipReg
	sess := whipSessionFromRequest(w, r)
	if sess == nil {
		return
	}
	if reg.claimSession(sess.id) == nil {
		// The PC-gone path claimed it first; the session is over either
		// way, and it owns the end notification.
		w.WriteHeader(http.StatusOK)
		return
	}
	switchRef().RemoveSourceIf(sess.sourceID, sess.src)
	// Tell mm-core the broadcast ended (best-effort, async): WHIP has
	// no host-API end call, so this is what ends the stream row.
	reg.notifyStreamEnd(sess)
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
