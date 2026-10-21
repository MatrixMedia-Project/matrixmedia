package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"sync"
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

	mu          sync.RWMutex
	token       string
	pc          *webrtc.PeerConnection
	active      bool
	stopped     bool
	subscribers map[string]PacketHandler
	stopCh      chan struct{}
	reconnectCh chan struct{}

	videoTrack *webrtc.TrackRemote
}

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
func NewCascadeSource(id, originBaseURL, bearerToken string) (*CascadeSource, error) {
	src := &CascadeSource{
		id:            id,
		originBaseURL: originBaseURL,
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
		if s.pc != pc {
			s.mu.Unlock()
			log.Printf("[cascade-source:%s] ignoring %s track from a replaced connection", s.id, kind)
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

	s.mu.Lock()
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

	body, err := json.Marshal(map[string]any{
		"id":    "cascade-" + s.id,
		"offer": offer,
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
	return func() {
		s.mu.Lock()
		defer s.mu.Unlock()
		delete(s.subscribers, id)
	}
}

// RequestKeyframe sends a PLI upstream to the origin, asking it to re-send a
// keyframe down through this cascade's track — same mechanism as
// WebRTCSource, one hop further out.
func (s *CascadeSource) RequestKeyframe() {
	s.mu.RLock()
	pc := s.pc
	track := s.videoTrack
	s.mu.RUnlock()
	if pc == nil || track == nil {
		return
	}
	pli := &rtcp.PictureLossIndication{MediaSSRC: uint32(track.SSRC())}
	if err := pc.WriteRTCP([]rtcp.Packet{pli}); err != nil {
		log.Printf("[cascade-source:%s] PLI write error: %v", s.id, err)
	}
}

func (s *CascadeSource) Stop() {
	s.mu.Lock()
	if s.stopped {
		s.mu.Unlock()
		return
	}
	s.stopped = true
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
