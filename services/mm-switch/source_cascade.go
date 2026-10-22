package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"sync"
	"sync/atomic"
	"time"

	"github.com/pion/rtcp"
	"github.com/pion/rtp"
	"github.com/pion/webrtc/v4"
)

// CascadeSource subscribes to an upstream (origin) mm-switch as an
// authenticated viewer and re-fans the received RTP to its own
// subscribers. This is how a rented relay node extends the origin's
// fan-out tree: from the origin's point of view, a CascadeSource is
// just another viewer; from this node's point of view, it's a Source
// like any other.
//
// Forwards original RTP packets — no depacketization, no conversion,
// same contract as every other Source (see source.go).
type CascadeSource struct {
	id            string
	originBaseURL string
	// relayNodeID is THIS relay's own fleet node id (M1): it is folded into the viewer id
	// this source presents to the origin ("cascade-{id}-{relayNodeID}") and into the body
	// of every offer, so the origin can enforce that the signed cascade token actually
	// names this relay, not merely this broadcast — see validateCascadeToken.
	relayNodeID string

	mu          sync.RWMutex
	token       string
	pc          *webrtc.PeerConnection
	active      bool
	stopped     bool
	subscribers map[string]PacketHandler
	stopCh      chan struct{}
	reconnectCh chan struct{}

	videoTrack *webrtc.TrackRemote

	// Keyframe-request throttle shared by every relay-side viewer's PLI and Subscribe's
	// join burst — mirrors WebRTCSource's pliMu/lastPLI (source_webrtc.go). Without it, N
	// relay-side viewers joining or stalling at once each fire their own PLI upstream to
	// the origin, 1:1 — the very storm the origin-side throttle exists to stop, just one
	// hop further out.
	pliMu   sync.Mutex
	lastPLI time.Time
	// keyframes counts video keyframes fanned out, so Subscribe's PLI burst can tell when
	// one has reached a newly joined relay-side viewer (mirrors WebRTCSource.keyframes).
	keyframes atomic.Uint64
}

// cascadeKeyframeRequestInterval mirrors source_webrtc.go's keyframeRequestInterval: the
// minimum gap between two PLIs this cascade source sends upstream to the origin, whoever
// asks (a relay-side viewer's stall, or Subscribe's join burst).
const cascadeKeyframeRequestInterval = 500 * time.Millisecond

// cascadeSubscribeKeyframeAttempts mirrors source_webrtc.go's subscribeKeyframeAttempts.
const cascadeSubscribeKeyframeAttempts = 5

// cascadeMaxReconnectAttempts bounds how many times CascadeSource retries the
// offer/answer exchange after an ICE disconnect or an expired-token response
// before giving up and staying inactive.
const cascadeMaxReconnectAttempts = 5

// cascadeReconnectBaseDelay is the first retry's backoff; it doubles each
// attempt (1s, 2s, 4s, 8s, 16s). A var so tests can shorten it.
var cascadeReconnectBaseDelay = time.Second

// cascadeHTTPTimeout bounds the viewer-offer POST to the origin.
const cascadeHTTPTimeout = 10 * time.Second

// NewCascadeSource dials the origin switch's viewer-offer endpoint and, once
// the handshake completes, starts forwarding received RTP to subscribers.
//
// relayNodeID identifies THIS relay (M1): folded into the viewer id presented to the
// origin so concurrently-seeded relays for the same broadcast each hold their own slot
// instead of evicting one another.
func NewCascadeSource(id, originBaseURL, bearerToken, relayNodeID string) (*CascadeSource, error) {
	src := &CascadeSource{
		id:            id,
		originBaseURL: originBaseURL,
		relayNodeID:   relayNodeID,
		token:         bearerToken,
		subscribers:   make(map[string]PacketHandler),
		stopCh:        make(chan struct{}),
		reconnectCh:   make(chan struct{}, 1),
	}

	if err := src.connect(); err != nil {
		return nil, err
	}

	go src.reconnectLoop()

	return src, nil
}

// UpdateToken swaps in a fresh bearer token, used on the next reconnect
// attempt. mm-core re-seeds the origin's viewer token before it expires;
// Task 2's token-refresh flow calls this so a subsequent reconnect (ICE
// disconnect, 401/403) uses the renewed credential instead of a stale one.
func (s *CascadeSource) UpdateToken(token string) {
	s.mu.Lock()
	s.token = token
	s.mu.Unlock()
}

// connect performs one offer/answer exchange against the origin and wires up
// the PeerConnection's track + state handlers. On success the source is left
// active once the first track arrives.
func (s *CascadeSource) connect() error {
	// M4: refuse to even start a new handshake once Stop() has fired. Without this check
	// up front, a Stop() landing between the "already stopped" decision higher up (e.g.
	// reconnectLoop's pre-delay check) and connect() being called could still race in —
	// this call is the authoritative gate right before any resources are allocated.
	s.mu.RLock()
	alreadyStopped := s.stopped
	s.mu.RUnlock()
	if alreadyStopped {
		return fmt.Errorf("cascade source: stopped")
	}

	pc, err := createPeerConnection()
	if err != nil {
		return fmt.Errorf("cascade source: create peer connection: %w", err)
	}

	if _, err := pc.AddTransceiverFromKind(webrtc.RTPCodecTypeVideo, webrtc.RTPTransceiverInit{
		Direction: webrtc.RTPTransceiverDirectionRecvonly,
	}); err != nil {
		_ = pc.Close()
		return fmt.Errorf("cascade source: add video transceiver: %w", err)
	}
	if _, err := pc.AddTransceiverFromKind(webrtc.RTPCodecTypeAudio, webrtc.RTPTransceiverInit{
		Direction: webrtc.RTPTransceiverDirectionRecvonly,
	}); err != nil {
		_ = pc.Close()
		return fmt.Errorf("cascade source: add audio transceiver: %w", err)
	}

	// Both handlers below capture `pc` and check it is STILL the source's
	// current PeerConnection before acting. A reconnect replaces s.pc and
	// closes the old PC; pion then fires the OLD PC's handlers (Closed state,
	// possibly a late OnTrack) on their own goroutines. Without this guard the
	// old PC's state handler would call triggerReconnect and tear down the
	// healthy NEW connection, and a late OnTrack could mark the source active
	// on a dead track.
	pc.OnTrack(func(track *webrtc.TrackRemote, receiver *webrtc.RTPReceiver) {
		kind := "video"
		if track.Kind() == webrtc.RTPCodecTypeAudio {
			kind = "audio"
		}

		s.mu.Lock()
		if s.pc != pc || s.stopped {
			// M4: either a reconnect already replaced this PC, or Stop() landed between
			// this handshake completing and this track arriving. Either way this PC is not
			// the live connection and must never flip active=true.
			s.mu.Unlock()
			log.Printf("[cascade-source:%s] ignoring %s track from a replaced or stopped connection", s.id, kind)
			return
		}
		s.active = true
		if kind == "video" {
			s.videoTrack = track
		}
		s.mu.Unlock()

		log.Printf("[cascade-source:%s] track: %s (%s)", s.id, kind, track.Codec().MimeType)
		go s.readTrack(track, kind)
	})

	pc.OnConnectionStateChange(func(state webrtc.PeerConnectionState) {
		if !publisherGone(state) {
			return
		}
		s.mu.Lock()
		if s.pc != pc {
			// A replaced PC winding down — not the live connection's problem.
			s.mu.Unlock()
			return
		}
		s.active = false
		stopped := s.stopped
		s.mu.Unlock()
		if stopped {
			return
		}
		log.Printf("[cascade-source:%s] connection %s: triggering reconnect", s.id, state)
		s.triggerReconnect()
	})

	offer, err := pc.CreateOffer(nil)
	if err != nil {
		_ = pc.Close()
		return fmt.Errorf("cascade source: create offer: %w", err)
	}
	gatherComplete := webrtc.GatheringCompletePromise(pc)
	if err := pc.SetLocalDescription(offer); err != nil {
		_ = pc.Close()
		return fmt.Errorf("cascade source: set local description: %w", err)
	}
	<-gatherComplete

	answer, err := s.postOffer(pc.LocalDescription())
	if err != nil {
		_ = pc.Close()
		return err
	}

	if err := pc.SetRemoteDescription(*answer); err != nil {
		_ = pc.Close()
		return fmt.Errorf("cascade source: set remote description: %w", err)
	}

	// M4: the authoritative check, under s.mu, right before this handshake's PC becomes
	// the live one. A Stop() that landed anywhere during this function's long async work
	// (ICE gathering, the HTTP round trip) must not have this PC stored afterward — that
	// would both resurrect a "stopped" source and leak the PC Stop() never got a chance to
	// close (it closed whatever was in s.pc at the time, which was not this one yet).
	s.mu.Lock()
	if s.stopped {
		s.mu.Unlock()
		_ = pc.Close()
		return fmt.Errorf("cascade source: stopped during connect")
	}
	old := s.pc
	s.pc = pc
	s.mu.Unlock()
	if old != nil {
		old.Close()
	}

	return nil
}

// cascadeAuthError marks a 401/403 from the origin so reconnect logic can
// treat it the same as any other retryable failure, while still being able
// to pick up a freshly updated token (via UpdateToken) before the next try.
type cascadeAuthError struct{ status int }

func (e *cascadeAuthError) Error() string {
	return fmt.Sprintf("cascade source: origin rejected token (status %d)", e.status)
}

// postOffer sends the local offer to the origin's viewer-offer endpoint and
// returns the SDP answer.
func (s *CascadeSource) postOffer(offer *webrtc.SessionDescription) (*webrtc.SessionDescription, error) {
	s.mu.RLock()
	token := s.token
	s.mu.RUnlock()

	// B1+M1: the viewer id this relay presents to the origin is per-relay —
	// "cascade-{sourceID}-{relayNodeID}" — and relay_node_id rides alongside in its own
	// field so the origin never has to parse it back out of the (ambiguous, hyphenated)
	// id string. mint_cascade_token (crates/mm-core/src/switch_client.rs) signs the EXACT
	// same two values into the claim; validateCascadeToken checks both.
	body, err := json.Marshal(map[string]any{
		"id":            "cascade-" + s.id + "-" + s.relayNodeID,
		"relay_node_id": s.relayNodeID,
		"offer":         offer,
	})
	if err != nil {
		return nil, fmt.Errorf("cascade source: marshal offer: %w", err)
	}

	req, err := http.NewRequest(http.MethodPost, s.originBaseURL+"/api/viewers/offer", bytes.NewReader(body))
	if err != nil {
		return nil, fmt.Errorf("cascade source: build request: %w", err)
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer "+token)

	client := &http.Client{Timeout: cascadeHTTPTimeout}
	resp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("cascade source: viewer offer request: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode == http.StatusUnauthorized || resp.StatusCode == http.StatusForbidden {
		return nil, &cascadeAuthError{status: resp.StatusCode}
	}
	if resp.StatusCode != http.StatusOK && resp.StatusCode != http.StatusCreated {
		return nil, fmt.Errorf("cascade source: origin returned status %d", resp.StatusCode)
	}

	var out struct {
		Answer webrtc.SessionDescription `json:"answer"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&out); err != nil {
		return nil, fmt.Errorf("cascade source: decode answer: %w", err)
	}

	return &out.Answer, nil
}

// triggerReconnect signals (non-blocking) the reconnect loop to wake up.
func (s *CascadeSource) triggerReconnect() {
	select {
	case s.reconnectCh <- struct{}{}:
	default:
	}
}

// reconnectLoop waits for triggerReconnect and retries the handshake with
// exponential backoff, up to cascadeMaxReconnectAttempts, re-posting a fresh
// offer each time (per the brief: no ICE restart / renegotiation — every
// attempt is a brand-new PeerConnection and offer).
func (s *CascadeSource) reconnectLoop() {
	for {
		select {
		case <-s.stopCh:
			return
		case <-s.reconnectCh:
		}

		s.mu.RLock()
		stopped := s.stopped
		s.mu.RUnlock()
		if stopped {
			return
		}

		delay := cascadeReconnectBaseDelay
		for attempt := 1; attempt <= cascadeMaxReconnectAttempts; attempt++ {
			select {
			case <-s.stopCh:
				return
			case <-time.After(delay):
			}

			if err := s.connect(); err != nil {
				log.Printf("[cascade-source:%s] reconnect attempt %d/%d failed: %v",
					s.id, attempt, cascadeMaxReconnectAttempts, err)
				delay *= 2
				continue
			}

			log.Printf("[cascade-source:%s] reconnected (attempt %d)", s.id, attempt)
			break
		}
		// Exhausted retries are INTENTIONALLY terminal: the source stays
		// registered but inactive until mm-core re-seeds it (or removes it).
	}
}

// readTrack reads RTP packets off a remote track and fans the ORIGINAL
// packet out to subscribers — same semantics as source_webrtc.go /
// source_livekit.go: no depacketization, no conversion.
func (s *CascadeSource) readTrack(track *webrtc.TrackRemote, kind string) {
	buf := make([]byte, 1500)
	pktCount := int64(0)
	dupCount := int64(0)
	// One window per track, owned by this goroutine: no lock needed. Mirrors
	// source_webrtc.go's readTrack (m1) — the origin's own RTX re-sends arrive with their
	// ORIGINAL sequence numbers, and without dedup those get fanned out to every relay-side
	// viewer a second time.
	var win seqWindow

	for {
		select {
		case <-s.stopCh:
			return
		default:
		}

		n, _, err := track.Read(buf)
		if err != nil {
			log.Printf("[cascade-source:%s] %s track ended: %v", s.id, kind, err)
			return
		}

		pkt := &rtp.Packet{}
		if err := pkt.Unmarshal(buf[:n]); err != nil {
			continue
		}

		pktCount++
		if pktCount == 1 {
			log.Printf("[cascade-source:%s] first %s pkt (seq=%d, ts=%d, payload=%d)",
				s.id, kind, pkt.SequenceNumber, pkt.Timestamp, len(pkt.Payload))
		}

		if !win.accept(pkt.SequenceNumber) {
			dupCount++
			if dupCount == 1 || dupCount%1000 == 0 {
				log.Printf("[cascade-source:%s] %d duplicate %s pkts dropped", s.id, dupCount, kind)
			}
			continue
		}
		if kind == "video" && IsVP8Keyframe(pkt.Payload) {
			s.keyframes.Add(1)
		}

		s.fanout(kind, pkt)
	}
}

// fanout forwards one original packet to all subscribers (they MUST clone
// before modifying). Panicking subscribers are quarantined instead of
// killing the process (ADR-04 Phase 1), same as every other source.
func (s *CascadeSource) fanout(kind string, pkt *rtp.Packet) {
	s.mu.RLock()
	panicked := dispatchAll(s.id, s.subscribers, kind, pkt)
	s.mu.RUnlock()
	quarantineSubscribers(s.id, &s.mu, s.subscribers, panicked)
}

func (s *CascadeSource) Type() string { return "cascade" }

func (s *CascadeSource) IsActive() bool {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.active
}

func (s *CascadeSource) Subscribe(id string, handler PacketHandler) func() {
	s.mu.Lock()
	s.subscribers[id] = handler
	s.mu.Unlock()

	// M5: mirror WebRTCSource.Subscribe's PLI-until-keyframe burst (source_webrtc.go). A
	// relay-side viewer joining between keyframes is otherwise black until the NEXT
	// keyframe the origin happens to send on its own cadence — up to a full GOP away.
	// The burst goes through the shared per-source throttle (RequestKeyframe), so a batch
	// of relay-side viewers joining at once still costs the origin at most one PLI per
	// throttle window, not one per viewer.
	seen := s.keyframes.Load()
	go func() {
		for i := 0; i < cascadeSubscribeKeyframeAttempts; i++ {
			s.RequestKeyframe()
			select {
			case <-s.stopCh:
				return
			case <-time.After(cascadeKeyframeRequestInterval):
			}
			if s.keyframes.Load() != seen {
				return
			}
			s.mu.RLock()
			_, ok := s.subscribers[id]
			s.mu.RUnlock()
			if !ok {
				return
			}
		}
	}()

	return func() {
		s.mu.Lock()
		defer s.mu.Unlock()
		delete(s.subscribers, id)
	}
}

// RequestKeyframe sends a PLI upstream to the origin, asking it to re-send a
// keyframe down through this cascade's track — same mechanism as
// WebRTCSource, one hop further out.
//
// M5: throttled exactly like WebRTCSource.RequestKeyframe (source_webrtc.go) — without
// this, N relay-side viewers each stalling or joining fan their PLIs 1:1 upstream to the
// origin, and the origin's OWN throttle only protects it from its own direct viewers, not
// from a whole relay's worth of them arriving as one client.
func (s *CascadeSource) RequestKeyframe() {
	s.mu.RLock()
	pc := s.pc
	track := s.videoTrack
	s.mu.RUnlock()
	if pc == nil || track == nil {
		return
	}
	if !s.claimKeyframeRequest() {
		return
	}
	pli := &rtcp.PictureLossIndication{MediaSSRC: uint32(track.SSRC())}
	if err := pc.WriteRTCP([]rtcp.Packet{pli}); err != nil {
		log.Printf("[cascade-source:%s] PLI write error: %v", s.id, err)
	}
}

// claimKeyframeRequest reports whether a PLI may go out now and, if so, starts a new
// throttle window — mirrors WebRTCSource.claimKeyframeRequest.
func (s *CascadeSource) claimKeyframeRequest() bool {
	s.pliMu.Lock()
	defer s.pliMu.Unlock()
	now := time.Now()
	if !s.lastPLI.IsZero() && now.Sub(s.lastPLI) < cascadeKeyframeRequestInterval {
		return false
	}
	s.lastPLI = now
	return true
}

func (s *CascadeSource) Stop() {
	s.mu.Lock()
	if s.stopped {
		s.mu.Unlock()
		return
	}
	s.stopped = true
	s.active = false // M4: Stop() never cleared this; a stopped source must not read as live.
	pc := s.pc
	s.mu.Unlock()

	select {
	case <-s.stopCh:
	default:
		close(s.stopCh)
	}
	if pc != nil {
		pc.Close()
	}
}

var _ Source = (*CascadeSource)(nil)
