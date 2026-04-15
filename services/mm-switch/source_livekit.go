package main

import (
	"context"
	"log"
	"sync"

	lksdk "github.com/livekit/server-sdk-go/v2"
	"github.com/pion/rtp"
	"github.com/pion/webrtc/v4"
)

// LiveKitSource subscribes to a LiveKit room and receives the streamer's
// tracks. Fans out RTP packets to all subscribed viewers.
type LiveKitSource struct {
	id     string
	room   *lksdk.Room
	active bool

	mu          sync.RWMutex
	subscribers map[string]PacketHandler
	stopCh      chan struct{}
}

// NewLiveKitSource connects to a LiveKit room and subscribes to all tracks.
func NewLiveKitSource(id, lkURL, apiKey, apiSecret, roomName, identity string) (*LiveKitSource, error) {
	src := &LiveKitSource{
		id:          id,
		subscribers: make(map[string]PacketHandler),
		stopCh:      make(chan struct{}),
	}

	// Connect to LiveKit room as a subscriber
	roomCB := &lksdk.RoomCallback{
		ParticipantCallback: lksdk.ParticipantCallback{
			OnTrackSubscribed: func(track *webrtc.TrackRemote, pub *lksdk.RemoteTrackPublication, rp *lksdk.RemoteParticipant) {
				kind := "video"
				if track.Kind() == webrtc.RTPCodecTypeAudio {
					kind = "audio"
				}
				log.Printf("[lk-source:%s] track subscribed: %s from %s (%s)", id, kind, rp.Identity(), track.Codec().MimeType)

				src.mu.Lock()
				src.active = true
				src.mu.Unlock()

				// Read RTP and fan out
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
							return
						}
						pkt := &rtp.Packet{}
						if err := pkt.Unmarshal(buf[:n]); err != nil {
							continue
						}
						src.mu.RLock()
						for _, h := range src.subscribers {
							h(kind, pkt)
						}
						src.mu.RUnlock()
					}
				}()
			},
		},
	}

	room, err := lksdk.ConnectToRoom(lkURL, lksdk.ConnectInfo{
		APIKey:              apiKey,
		APISecret:           apiSecret,
		RoomName:            roomName,
		ParticipantIdentity: identity,
	}, roomCB)
	if err != nil {
		return nil, err
	}

	src.room = room
	log.Printf("[lk-source:%s] connected to LiveKit room %s", id, roomName)

	return src, nil
}

func (s *LiveKitSource) Type() string { return "livekit" }

func (s *LiveKitSource) IsActive() bool {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.active
}

func (s *LiveKitSource) Subscribe(id string, handler PacketHandler) func() {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.subscribers[id] = handler
	return func() {
		s.mu.Lock()
		defer s.mu.Unlock()
		delete(s.subscribers, id)
	}
}

func (s *LiveKitSource) Stop() {
	select {
	case <-s.stopCh:
	default:
		close(s.stopCh)
	}
	if s.room != nil {
		s.room.Disconnect()
	}
}

var _ Source = (*LiveKitSource)(nil)
var _ context.Context = context.Background() // keep context import
