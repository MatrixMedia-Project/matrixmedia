package main

import (
	"log"
	"sync"
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

	mu          sync.RWMutex
	subscribers map[string]PacketHandler
	stopCh      chan struct{}

	// For PLI requests
	videoTrack    *webrtc.TrackRemote
	videoReceiver *webrtc.RTPReceiver
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

		src.mu.Lock()
		src.active = true
		if kind == "video" {
			src.videoTrack = track
			src.videoReceiver = receiver
		}
		src.mu.Unlock()

		go src.readTrack(track, kind)
	})

	return src
}

func (s *WebRTCSource) readTrack(track *webrtc.TrackRemote, kind string) {
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

		s.fanout(kind, pkt)
	}
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

// RequestKeyframe sends PLI RTCP to the publisher to trigger a keyframe.
func (s *WebRTCSource) RequestKeyframe() {
	s.mu.RLock()
	track := s.videoTrack
	s.mu.RUnlock()
	if track == nil {
		return
	}
	pli := &rtcp.PictureLossIndication{MediaSSRC: uint32(track.SSRC())}
	if err := s.pc.WriteRTCP([]rtcp.Packet{pli}); err != nil {
		log.Printf("[webrtc-source:%s] PLI write error: %v", s.id, err)
	} else {
		log.Printf("[webrtc-source:%s] PLI sent", s.id)
	}
}

func (s *WebRTCSource) Type() string  { return "webrtc" }
func (s *WebRTCSource) IsActive() bool { s.mu.RLock(); defer s.mu.RUnlock(); return s.active }
func (s *WebRTCSource) Subscribe(id string, handler PacketHandler) func() {
	s.mu.Lock()
	s.subscribers[id] = handler
	s.mu.Unlock()
	// Send PLI repeatedly: handle UDP loss + give encoder multiple chances.
	// Stops as soon as any keyframe arrives (subscriber stops needing them).
	go func() {
		for i := 0; i < 5; i++ {
			s.RequestKeyframe()
			time.Sleep(150 * time.Millisecond)
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
