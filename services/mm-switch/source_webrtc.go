package main

import (
	"log"
	"sync"

	"github.com/pion/rtp"
	"github.com/pion/webrtc/v4"
)

// WebRTCSource receives media from a WebRTC publisher (streamer).
// It fans out received RTP packets to all subscribed viewers.
type WebRTCSource struct {
	id     string
	pc     *webrtc.PeerConnection
	active bool

	mu          sync.RWMutex
	subscribers map[string]PacketHandler
	stopCh      chan struct{}
}

// NewWebRTCSource creates a source from a WebRTC peer connection.
// The publisher sends their camera/screen/audio tracks to this connection.
func NewWebRTCSource(id string, pc *webrtc.PeerConnection) *WebRTCSource {
	src := &WebRTCSource{
		id:          id,
		pc:          pc,
		subscribers: make(map[string]PacketHandler),
		stopCh:      make(chan struct{}),
	}

	// When the publisher sends tracks, start forwarding RTP packets.
	pc.OnTrack(func(track *webrtc.TrackRemote, receiver *webrtc.RTPReceiver) {
		kind := "video"
		if track.Kind() == webrtc.RTPCodecTypeAudio {
			kind = "audio"
		}
		log.Printf("[source:%s] track received: %s (%s)", id, kind, track.Codec().MimeType)

		src.mu.Lock()
		src.active = true
		src.mu.Unlock()

		// Read RTP packets and fan out to subscribers.
		go func() {
			buf := make([]byte, 1500)
			for {
				select {
				case <-src.stopCh:
					return
				default:
				}

				n, _, err := track.Read(buf)
				if err != nil {
					log.Printf("[source:%s] track read error: %v", id, err)
					return
				}

				pkt := &rtp.Packet{}
				if err := pkt.Unmarshal(buf[:n]); err != nil {
					continue
				}

				// Fan out to all subscribers
				src.mu.RLock()
				for _, handler := range src.subscribers {
					handler(kind, pkt)
				}
				src.mu.RUnlock()
			}
		}()
	})

	pc.OnConnectionStateChange(func(state webrtc.PeerConnectionState) {
		log.Printf("[source:%s] connection: %s", id, state)
		if state == webrtc.PeerConnectionStateFailed || state == webrtc.PeerConnectionStateClosed {
			src.mu.Lock()
			src.active = false
			src.mu.Unlock()
		}
	})

	return src
}

func (s *WebRTCSource) Type() string { return "webrtc" }

func (s *WebRTCSource) IsActive() bool {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.active
}

func (s *WebRTCSource) Subscribe(id string, handler PacketHandler) func() {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.subscribers[id] = handler
	return func() {
		s.mu.Lock()
		defer s.mu.Unlock()
		delete(s.subscribers, id)
	}
}

func (s *WebRTCSource) Stop() {
	close(s.stopCh)
	s.pc.Close()
}
