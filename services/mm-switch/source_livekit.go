package main

import (
	"log"
	"sync"

	lksdk "github.com/livekit/server-sdk-go/v2"
	"github.com/pion/rtp"
	"github.com/pion/webrtc/v4"
)

// LiveKitSource subscribes to a LiveKit room and forwards original RTP packets.
type LiveKitSource struct {
	id     string
	room   *lksdk.Room
	active bool

	mu          sync.RWMutex
	subscribers map[string]PacketHandler
	stopCh      chan struct{}

	// For PLI: hold the participant's video pub so we can call SetTrackSubscriptionPermissions
	// LiveKit SDK has its own internal PLI mechanism via the receiver interceptor.
}

func NewLiveKitSource(id, lkURL, apiKey, apiSecret, roomName, identity string) (*LiveKitSource, error) {
	src := &LiveKitSource{
		id:          id,
		subscribers: make(map[string]PacketHandler),
		stopCh:      make(chan struct{}),
	}

	roomCB := &lksdk.RoomCallback{
		ParticipantCallback: lksdk.ParticipantCallback{
			OnTrackSubscribed: func(track *webrtc.TrackRemote, pub *lksdk.RemoteTrackPublication, rp *lksdk.RemoteParticipant) {
				kind := "video"
				if track.Kind() == webrtc.RTPCodecTypeAudio {
					kind = "audio"
				}
				log.Printf("[lk-source:%s] track: %s from %s (%s)", id, kind, rp.Identity(), track.Codec().MimeType)
				src.mu.Lock()
				src.active = true
				src.mu.Unlock()
				go src.readTrack(track, kind)
			},
		},
	}

	room, err := lksdk.ConnectToRoom(lkURL, lksdk.ConnectInfo{
		APIKey:              apiKey,
		APISecret:           apiSecret,
		RoomName:            roomName,
		ParticipantIdentity: identity,
	}, roomCB, lksdk.WithAutoSubscribe(true))
	if err != nil {
		return nil, err
	}

	src.room = room
	log.Printf("[lk-source:%s] connected to %s (RTP passthrough)", id, roomName)
	return src, nil
}

func (s *LiveKitSource) readTrack(track *webrtc.TrackRemote, kind string) {
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
			log.Printf("[lk-source:%s] %s track ended: %v", s.id, kind, err)
			return
		}

		pkt := &rtp.Packet{}
		if err := pkt.Unmarshal(buf[:n]); err != nil {
			continue
		}

		pktCount++
		if pktCount == 1 {
			log.Printf("[lk-source:%s] first %s pkt (seq=%d, ts=%d, payload=%d)",
				s.id, kind, pkt.SequenceNumber, pkt.Timestamp, len(pkt.Payload))
		}
		if pktCount%1000 == 0 {
			log.Printf("[lk-source:%s] %d %s pkts", s.id, pktCount, kind)
		}

		// Forward original packet to subscribers (they MUST clone before modifying)
		s.mu.RLock()
		for _, h := range s.subscribers {
			h(kind, pkt)
		}
		s.mu.RUnlock()
	}
}

// RequestKeyframe — LiveKit SDK has its own PLI mechanism, this is a no-op for now.
// TODO: investigate if we can trigger PLI through the SDK.
func (s *LiveKitSource) RequestKeyframe() {}

func (s *LiveKitSource) Type() string  { return "livekit" }
func (s *LiveKitSource) IsActive() bool { s.mu.RLock(); defer s.mu.RUnlock(); return s.active }
func (s *LiveKitSource) Subscribe(id string, handler PacketHandler) func() {
	s.mu.Lock(); defer s.mu.Unlock()
	s.subscribers[id] = handler
	return func() { s.mu.Lock(); defer s.mu.Unlock(); delete(s.subscribers, id) }
}
func (s *LiveKitSource) Stop() {
	select { case <-s.stopCh: default: close(s.stopCh) }
	if s.room != nil { s.room.Disconnect() }
}

var _ Source = (*LiveKitSource)(nil)
