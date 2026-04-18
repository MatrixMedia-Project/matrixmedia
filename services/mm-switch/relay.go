package main

import (
	"log"
	"sync"

	lksdk "github.com/livekit/server-sdk-go/v2"

	"github.com/pion/rtp"
	"github.com/pion/webrtc/v4"
)

// Relay subscribes to a source and publishes its tracks into a LiveKit room.
// Viewers connect to that room via the standard LiveKit SDK.
// The relay can switch sources on the fly — viewers see a seamless transition.
type Relay struct {
	id         string
	room       *lksdk.Room
	videoTrack *lksdk.LocalTrack
	audioTrack *lksdk.LocalTrack

	mu            sync.RWMutex
	currentSource string
	unsubscribe   func()
	active        bool
}

// NewRelay creates a relay that publishes into a LiveKit room.
func NewRelay(id, lkURL, apiKey, apiSecret, roomName, identity string) (*Relay, error) {
	r := &Relay{id: id}

	// Create local tracks that we'll publish
	videoTrack, err := lksdk.NewLocalTrack(webrtc.RTPCodecCapability{
		MimeType: webrtc.MimeTypeVP8,
	})
	if err != nil {
		return nil, err
	}

	audioTrack, err := lksdk.NewLocalTrack(webrtc.RTPCodecCapability{
		MimeType: webrtc.MimeTypeOpus,
	})
	if err != nil {
		return nil, err
	}

	r.videoTrack = videoTrack
	r.audioTrack = audioTrack

	// Connect to LiveKit room and publish tracks
	room, err := lksdk.ConnectToRoom(lkURL, lksdk.ConnectInfo{
		APIKey:              apiKey,
		APISecret:           apiSecret,
		RoomName:            roomName,
		ParticipantIdentity: identity,
	}, &lksdk.RoomCallback{})
	if err != nil {
		return nil, err
	}

	r.room = room

	// Publish tracks
	if _, err := room.LocalParticipant.PublishTrack(videoTrack, &lksdk.TrackPublicationOptions{
		Name: "mm-relay-video",
	}); err != nil {
		log.Printf("[relay:%s] failed to publish video: %v", id, err)
	}

	if _, err := room.LocalParticipant.PublishTrack(audioTrack, &lksdk.TrackPublicationOptions{
		Name: "mm-relay-audio",
	}); err != nil {
		log.Printf("[relay:%s] failed to publish audio: %v", id, err)
	}

	r.active = true
	log.Printf("[relay:%s] publishing to LiveKit room %s as %s", id, roomName, identity)

	return r, nil
}

// SwitchSource changes what the relay is forwarding.
func (r *Relay) SwitchSource(sourceID string, src Source) {
	r.mu.Lock()
	defer r.mu.Unlock()

	// Unsub from previous
	if r.unsubscribe != nil {
		r.unsubscribe()
	}

	r.currentSource = sourceID

	// Relay is deprecated — direct Pion viewers used instead.
	// No-op subscription to satisfy the interface.
	unsub := src.Subscribe("relay-"+r.id, func(kind string, pkt *rtp.Packet) {
		// intentionally empty — relay not used for direct Pion viewers
	})
	r.unsubscribe = unsub

	log.Printf("[relay:%s] switched to source %s", r.id, sourceID)
}

// CurrentSourceID returns the current source.
func (r *Relay) CurrentSourceID() string {
	r.mu.RLock()
	defer r.mu.RUnlock()
	return r.currentSource
}

// Close disconnects the relay.
func (r *Relay) Close() {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.unsubscribe != nil {
		r.unsubscribe()
	}
	if r.room != nil {
		r.room.Disconnect()
	}
	r.active = false
}
