package main

import (
	"log"
	"sync"
	"sync/atomic"
	"time"

	"github.com/pion/rtcp"
	"github.com/pion/rtp"
	"github.com/pion/webrtc/v4"
)

// WebRTCSource receives media from a direct WebRTC publisher.
// Forwards original RTP packets — no depacketization. Browser-published VP8
// RTP already has all required fields (PictureID, etc.) for browser decoding.
type WebRTCSource struct {
	id     string
	pc     *webrtc.PeerConnection
	active bool
	// gone is sticky: once the publisher's PeerConnection has Failed or Closed, active can
	// never become true again. pion runs OnTrack and OnConnectionStateChange in separate
	// goroutines with no ordering, so a hang-up right after the first RTP packet can let
	// OnTrack run AFTER the state handler; without this a dead source would be marked
	// active again and nothing would ever clear it. A fresh offer always builds a new PC
	// and a new source (no ICE restart / renegotiation), so stickiness costs nothing.
	// Guarded by mu, like active.
	gone bool

	mu          sync.RWMutex
	subscribers map[string]PacketHandler
	stopCh      chan struct{}

	// For PLI requests
	videoTrack    *webrtc.TrackRemote
	videoReceiver *webrtc.RTPReceiver

	// Keyframe-request throttle: when the last PLI went out. Its own lock, not mu:
	// RequestKeyframe is reached from paths that already hold mu for reading (see the
	// recorder), and a write lock there could deadlock.
	pliMu   sync.Mutex
	lastPLI time.Time
	// For the PLI log line, which summarises rather than logging every request.
	plisSent, plisThrottled atomic.Int64

	// keyframes counts the video keyframes fanned out, so a new subscriber's PLI burst
	// can tell when one has reached it.
	keyframes atomic.Uint64
}

// keyframeRequestInterval is the minimum gap between two PLIs to one publisher, whoever
// asks: viewers passing on their phone's PLI/FIR (each limited to one a second), the
// recorder, and Subscribe's burst all share it. Every PLI costs the publisher a keyframe
// many times the size of a normal frame, and N stuck viewers add up to N requests a
// second. libwebrtc ignores requests only within 300 ms of the last one it honoured, so
// without this a few stuck phones hold the host at ~3 keyframes a second, starving
// everyone's bitrate. 500 ms caps that at 2 a second, and a lost keyframe is still
// re-requested within half a second.
const keyframeRequestInterval = 500 * time.Millisecond

// subscribeKeyframeAttempts is how many PLIs a new subscriber's burst sends at most, one
// per keyframeRequestInterval, until a keyframe reaches it.
const subscribeKeyframeAttempts = 5

// publisherGone reports whether a PeerConnection state means the publisher is not
// coming back. Disconnected is deliberately NOT one of them: ICE can recover from it, and
// it is the state a briefly flaky network passes through on its way to Failed or back to
// Connected.
func publisherGone(state webrtc.PeerConnectionState) bool {
	return state == webrtc.PeerConnectionStateFailed ||
		state == webrtc.PeerConnectionStateClosed
}

func NewWebRTCSource(id string, pc *webrtc.PeerConnection) *WebRTCSource {
	src := &WebRTCSource{
		id:          id,
		pc:          pc,
		subscribers: make(map[string]PacketHandler),
		stopCh:      make(chan struct{}),
	}

	pc.OnTrack(func(track *webrtc.TrackRemote, receiver *webrtc.RTPReceiver) {
		kind := "video"
		if track.Kind() == webrtc.RTPCodecTypeAudio {
			kind = "audio"
		}
		log.Printf("[webrtc-source:%s] track: %s (%s)", id, kind, track.Codec().MimeType)

		if !src.trackArrived(kind, track, receiver) {
			// The publisher is already gone: leave the source inactive and don't start a
			// reader for a track whose PeerConnection is dead.
			log.Printf("[webrtc-source:%s] ignoring %s track: publisher already gone", id, kind)
			return
		}

		go src.readTrack(track, kind)
	})

	// A publisher that crashes or hangs up must stop looking live. Without this, active
	// stayed true forever once the first track arrived, so "the switch has an active
	// source" could not tell a live host from a dead one. Failed is the ICE timeout (a
	// silent host); Closed is a hang-up or our own Stop(). NOT Disconnected — see
	// publisherGone. pion keeps only the last handler per PeerConnection and nothing else
	// registers one on a publisher's PC (the viewer's is a different PC).
	pc.OnConnectionStateChange(src.connectionStateChanged)

	return src
}

// trackArrived records a newly arrived remote track and marks the source active. It
// returns false, changing nothing, if the publisher is already gone.
func (s *WebRTCSource) trackArrived(kind string, track *webrtc.TrackRemote, receiver *webrtc.RTPReceiver) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.gone {
		return false
	}
	s.active = true
	if kind == "video" {
		s.videoTrack = track
		s.videoReceiver = receiver
	}
	return true
}

// connectionStateChanged is the publisher PeerConnection's state handler: Failed or
// Closed make the source permanently inactive (logged once, on the first such state).
func (s *WebRTCSource) connectionStateChanged(state webrtc.PeerConnectionState) {
	if !publisherGone(state) {
		return
	}
	s.mu.Lock()
	first := !s.gone
	s.gone = true
	s.active = false
	s.mu.Unlock()
	if first {
		log.Printf("[webrtc-source:%s] connection %s: publisher gone, source inactive", s.id, state)
	}
}

func (s *WebRTCSource) readTrack(track *webrtc.TrackRemote, kind string) {
	buf := make([]byte, 1500)
	pktCount := int64(0)
	dupCount := int64(0)
	// One window per track, owned by this goroutine: no lock needed.
	var win seqWindow

	for {
		select {
		case <-s.stopCh:
			return
		default:
		}

		n, _, err := track.Read(buf)
		if err != nil {
			log.Printf("[webrtc-source:%s] %s track ended: %v", s.id, kind, err)
			return
		}

		pkt := &rtp.Packet{}
		if err := pkt.Unmarshal(buf[:n]); err != nil {
			continue
		}

		pktCount++
		if pktCount == 1 {
			log.Printf("[webrtc-source:%s] first %s pkt (seq=%d, ts=%d, payload=%d)",
				s.id, kind, pkt.SequenceNumber, pkt.Timestamp, len(pkt.Payload))
		}
		if pktCount%1000 == 0 {
			log.Printf("[webrtc-source:%s] %d %s pkts", s.id, pktCount, kind)
		}

		if !s.ingest(&win, kind, pkt) {
			dupCount++
			if dupCount == 1 || dupCount%1000 == 0 {
				log.Printf("[webrtc-source:%s] %d duplicate %s pkts dropped", s.id, dupCount, kind)
			}
		}
	}
}

// ingest fans a packet out unless this track has already delivered its sequence number
// (see seqWindow: pion hands RTX re-sends back with the original numbers). Returns false
// for a dropped duplicate. Subscribers may therefore still see a LATE packet, a repair
// for one that really went missing, out of order: the recorder's sample builder and each
// viewer's jitter buffer put it back in place.
func (s *WebRTCSource) ingest(win *seqWindow, kind string, pkt *rtp.Packet) bool {
	if !win.accept(pkt.SequenceNumber) {
		sourceDuplicatePacketsTotal.WithLabelValues(kind).Inc()
		return false
	}
	// Counted BEFORE the fan-out: a burst that sees the count move then knows the
	// keyframe was fanned out after its subscriber was registered (see Subscribe).
	if kind == "video" && IsVP8Keyframe(pkt.Payload) {
		s.keyframes.Add(1)
	}
	s.fanout(kind, pkt)
	return true
}

// fanout forwards one original packet to all subscribers (they MUST
// clone before modifying). Panicking subscribers are quarantined
// instead of killing the process (ADR-04 Phase 1).
func (s *WebRTCSource) fanout(kind string, pkt *rtp.Packet) {
	s.mu.RLock()
	panicked := dispatchAll(s.id, s.subscribers, kind, pkt)
	s.mu.RUnlock()
	quarantineSubscribers(s.id, &s.mu, s.subscribers, panicked)
}

// RequestKeyframe sends PLI RTCP to the publisher to trigger a keyframe, unless one went
// out within keyframeRequestInterval: that PLI's keyframe is already on its way to every
// subscriber, the caller included.
func (s *WebRTCSource) RequestKeyframe() {
	s.mu.RLock()
	track := s.videoTrack
	gone := s.gone
	s.mu.RUnlock()
	if track == nil || gone {
		return
	}
	if !s.claimKeyframeRequest() {
		s.plisThrottled.Add(1)
		sourceKeyframeRequestsTotal.WithLabelValues("throttled").Inc()
		return
	}
	pli := &rtcp.PictureLossIndication{MediaSSRC: uint32(track.SSRC())}
	if err := s.pc.WriteRTCP([]rtcp.Packet{pli}); err != nil {
		sourceKeyframeRequestsTotal.WithLabelValues("failed").Inc()
		log.Printf("[webrtc-source:%s] PLI write error: %v", s.id, err)
		return
	}
	sourceKeyframeRequestsTotal.WithLabelValues("sent").Inc()
	// Stuck viewers can hold this at two a second for a whole stream: log the first PLI
	// and then a running count, not every one.
	if n := s.plisSent.Add(1); n == 1 || n%100 == 0 {
		log.Printf("[webrtc-source:%s] %d PLIs sent, %d throttled", s.id, n, s.plisThrottled.Load())
	}
}

// claimKeyframeRequest reports whether a PLI may go out now and, if so, starts a new
// throttle window. Of any number of concurrent callers within one window, one wins.
func (s *WebRTCSource) claimKeyframeRequest() bool {
	s.pliMu.Lock()
	defer s.pliMu.Unlock()
	now := time.Now()
	if !s.lastPLI.IsZero() && now.Sub(s.lastPLI) < keyframeRequestInterval {
		return false
	}
	s.lastPLI = now
	return true
}

func (s *WebRTCSource) Type() string  { return "webrtc" }
func (s *WebRTCSource) IsActive() bool { s.mu.RLock(); defer s.mu.RUnlock(); return s.active }
func (s *WebRTCSource) Subscribe(id string, handler PacketHandler) func() {
	s.mu.Lock()
	s.subscribers[id] = handler
	s.mu.Unlock()
	// Ask for a keyframe until one reaches the new subscriber, repeatedly because the PLI
	// or the keyframe can be lost on UDP.
	//
	// The burst goes THROUGH the per-source throttle instead of bypassing it. A source
	// switch re-subscribes every viewer at once, and a bypass would turn one switch into
	// five PLIs per viewer, the very storm the throttle exists to stop. Instead the
	// attempts are spaced a full keyframeRequestInterval apart, so only the first can be
	// swallowed by a PLI sent before this subscriber was listening. A later attempt that
	// is throttled lost out to a PLI sent after the subscription, whose keyframe this
	// subscriber does receive.
	//
	// The burst stops at the first keyframe fanned out after the subscription, so a join
	// normally costs the publisher one keyframe, not one per attempt. The snapshot is
	// taken after the handler is registered, and ingest counts a keyframe before fanning
	// it out, so a keyframe that moves the count was delivered to this subscriber.
	seen := s.keyframes.Load()
	go func() {
		for i := 0; i < subscribeKeyframeAttempts; i++ {
			s.RequestKeyframe()
			select {
			case <-s.stopCh:
				return
			case <-time.After(keyframeRequestInterval):
			}
			if s.keyframes.Load() != seen {
				return
			}
			// Check if subscriber still exists (may have unsubscribed)
			s.mu.RLock()
			_, ok := s.subscribers[id]
			s.mu.RUnlock()
			if !ok {
				return
			}
		}
	}()
	return func() {
		s.mu.Lock(); defer s.mu.Unlock()
		delete(s.subscribers, id)
	}
}
func (s *WebRTCSource) Stop() {
	select { case <-s.stopCh: default: close(s.stopCh) }
	s.pc.Close()
}

var _ Source = (*WebRTCSource)(nil)
