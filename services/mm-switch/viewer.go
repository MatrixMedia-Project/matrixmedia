package main

import (
	"log"
	"sync"

	"github.com/pion/rtp"
	"github.com/pion/webrtc/v4"
)

// Viewer represents an output WebRTC connection to a consumer.
// Each viewer has its own peer connection and can be independently
// switched to any source.
type Viewer struct {
	id        string
	pc        *webrtc.PeerConnection
	videoTrack *webrtc.TrackLocalStaticRTP
	audioTrack *webrtc.TrackLocalStaticRTP

	mu            sync.RWMutex
	currentSource string
	unsubscribe   func()
	connected     bool
	closed        bool
}

// NewViewer creates a viewer with WebRTC tracks ready for media.
func NewViewer(id string, pc *webrtc.PeerConnection) (*Viewer, error) {
	// Create outgoing tracks that we'll write RTP packets to.
	videoTrack, err := webrtc.NewTrackLocalStaticRTP(
		webrtc.RTPCodecCapability{MimeType: webrtc.MimeTypeVP8},
		"video", "mm-switch-video",
	)
	if err != nil {
		return nil, err
	}

	audioTrack, err := webrtc.NewTrackLocalStaticRTP(
		webrtc.RTPCodecCapability{MimeType: webrtc.MimeTypeOpus},
		"audio", "mm-switch-audio",
	)
	if err != nil {
		return nil, err
	}

	// Add tracks to the peer connection.
	if _, err = pc.AddTrack(videoTrack); err != nil {
		return nil, err
	}
	if _, err = pc.AddTrack(audioTrack); err != nil {
		return nil, err
	}

	v := &Viewer{
		id:         id,
		pc:         pc,
		videoTrack: videoTrack,
		audioTrack: audioTrack,
	}

	pc.OnConnectionStateChange(func(state webrtc.PeerConnectionState) {
		v.mu.Lock()
		v.connected = state == webrtc.PeerConnectionStateConnected
		v.mu.Unlock()
		log.Printf("[viewer:%s] connection state: %s", id, state)
		if state == webrtc.PeerConnectionStateFailed || state == webrtc.PeerConnectionStateClosed {
			v.Close()
		}
	})

	return v, nil
}

// SwitchTo changes the source for this viewer.
// Unsubscribes from the previous source and subscribes to the new one.
// The switch is seamless — the viewer's WebRTC tracks continue, just
// the data flowing through them changes.
func (v *Viewer) SwitchTo(sourceID string, src Source) {
	v.mu.Lock()
	defer v.mu.Unlock()

	if v.closed {
		return
	}

	// Unsubscribe from current source
	if v.unsubscribe != nil {
		v.unsubscribe()
		v.unsubscribe = nil
	}

	v.currentSource = sourceID

	// Subscribe to new source — packets will flow to our tracks
	unsub := src.Subscribe(v.id, func(kind string, pkt *rtp.Packet) {
		v.writePacket(kind, pkt)
	})
	v.unsubscribe = unsub

	log.Printf("[viewer:%s] switched to source %s", v.id, sourceID)
}

// DetachSource unsubscribes from the current source (viewer sees black).
func (v *Viewer) DetachSource() {
	v.mu.Lock()
	defer v.mu.Unlock()

	if v.unsubscribe != nil {
		v.unsubscribe()
		v.unsubscribe = nil
	}
	v.currentSource = ""
}

// CurrentSourceID returns the current source this viewer is watching.
func (v *Viewer) CurrentSourceID() string {
	v.mu.RLock()
	defer v.mu.RUnlock()
	return v.currentSource
}

// IsConnected returns whether the WebRTC connection is alive.
func (v *Viewer) IsConnected() bool {
	v.mu.RLock()
	defer v.mu.RUnlock()
	return v.connected
}

// Close terminates the viewer's connection.
func (v *Viewer) Close() {
	v.mu.Lock()
	defer v.mu.Unlock()

	if v.closed {
		return
	}
	v.closed = true

	if v.unsubscribe != nil {
		v.unsubscribe()
	}
	v.pc.Close()
}

// writePacket writes an RTP packet to the appropriate track.
func (v *Viewer) writePacket(kind string, pkt *rtp.Packet) {
	var err error
	switch kind {
	case "video":
		err = v.videoTrack.WriteRTP(pkt)
	case "audio":
		err = v.audioTrack.WriteRTP(pkt)
	}
	if err != nil {
		// Connection closed or buffer full — ignore silently
	}
}
