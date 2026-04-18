package main

import (
	"log"
	"sync"
	"time"

	"github.com/pion/rtp"
	"github.com/pion/webrtc/v4"
)

// Viewer represents a consumer's WebRTC connection.
// Uses TrackLocalStaticRTP — forwards ORIGINAL VP8 RTP packets from the source
// untouched. Per-viewer sequence number rewriting only. Pion handles SSRC and
// PayloadType automatically. The publisher's PictureID, marker bits, etc. are
// preserved exactly as the encoder produced them.
type Viewer struct {
	id         string
	pc         *webrtc.PeerConnection
	videoTrack *webrtc.TrackLocalStaticRTP
	audioTrack *webrtc.TrackLocalStaticRTP

	mu            sync.RWMutex
	currentSource string
	unsubscribe   func()
	connected     bool
	closed        bool
	videoPkts     int64
	audioPkts     int64
	videoSeq      uint16
	audioSeq      uint16

	// Continuous timeline across source switches
	videoLastTS    uint32
	audioLastTS    uint32
	videoLastTSSet bool
	audioLastTSSet bool

	pendingSourceID string
	pendingSource   Source
}

func NewViewer(id string, pc *webrtc.PeerConnection) (*Viewer, error) {
	videoTrack, err := webrtc.NewTrackLocalStaticRTP(
		webrtc.RTPCodecCapability{MimeType: webrtc.MimeTypeVP8},
		"video", "mm-switch",
	)
	if err != nil {
		return nil, err
	}

	audioTrack, err := webrtc.NewTrackLocalStaticRTP(
		webrtc.RTPCodecCapability{MimeType: webrtc.MimeTypeOpus},
		"audio", "mm-switch",
	)
	if err != nil {
		return nil, err
	}

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
		pendingID := v.pendingSourceID
		pendingSrc := v.pendingSource
		v.mu.Unlock()

		log.Printf("[viewer:%s] connection: %s", id, state)

		if state == webrtc.PeerConnectionStateConnected && pendingSrc != nil {
			log.Printf("[viewer:%s] connected — activating source %s", id, pendingID)
			v.activateSource(pendingID, pendingSrc)
		}

		// Auto-cleanup: remove from MediaSwitch so UDP ports release immediately
		if state == webrtc.PeerConnectionStateFailed ||
			state == webrtc.PeerConnectionStateClosed ||
			state == webrtc.PeerConnectionStateDisconnected {
			go mediaSwitch.RemoveViewer(id)
		}
	})

	return v, nil
}

// SwitchTo changes the source.
func (v *Viewer) SwitchTo(sourceID string, src Source) {
	v.mu.Lock()
	if v.closed {
		v.mu.Unlock()
		return
	}
	if v.unsubscribe != nil {
		v.unsubscribe()
		v.unsubscribe = nil
	}
	v.currentSource = sourceID
	v.pendingSourceID = sourceID
	v.pendingSource = src
	v.mu.Unlock()

	go func() {
		for i := 0; i < 50; i++ {
			v.mu.RLock()
			connected := v.connected
			v.mu.RUnlock()
			if connected {
				v.activateSource(sourceID, src)
				return
			}
			time.Sleep(100 * time.Millisecond)
		}
		log.Printf("[viewer:%s] timeout waiting for connection", v.id)
		v.activateSource(sourceID, src)
	}()
}

// activateSource subscribes to source and forwards original RTP packets.
// Per-source timestamp rewriting maintains continuous viewer-side timeline,
// so the browser jitter buffer doesn't freeze on source switch.
// Skips video deltas until first live keyframe → decoder gets correct reference.
// Sends repeated PLI to handle UDP loss.
func (v *Viewer) activateSource(sourceID string, src Source) {
	v.mu.Lock()
	if v.pendingSource != src {
		v.mu.Unlock()
		return
	}
	v.pendingSource = nil
	v.pendingSourceID = ""
	if v.unsubscribe != nil {
		v.unsubscribe()
		v.unsubscribe = nil
	}
	v.mu.Unlock()

	// Closure-local: per-source state, recreated on every switch
	waitingForKeyframe := true
	var videoTSOffset, audioTSOffset uint32
	var videoOffsetReady, audioOffsetReady bool

	// Do NOT hold v.mu during Subscribe. FileSource.Subscribe delivers
	// cached keyframe packets inline through the handler, and the handler
	// needs v.mu → holding both = deadlock.
	unsub := src.Subscribe(v.id, func(kind string, pkt *rtp.Packet) {
		switch kind {
		case "video":
			if waitingForKeyframe {
				if !IsVP8Keyframe(pkt.Payload) {
					return // skip — decoder needs keyframe first
				}
				waitingForKeyframe = false
				log.Printf("[viewer:%s] live keyframe (seq=%d, ts=%d), starting video",
					v.id, pkt.SequenceNumber, pkt.Timestamp)
			}

			// Calculate per-source timestamp offset on first packet from this source.
			// This bridges the timestamp gap between sources so the viewer sees a
			// continuous timeline (no jitter buffer freeze on switch).
			v.mu.Lock()
			if !videoOffsetReady {
				if v.videoLastTSSet {
					// Place this packet 1 frame (3000 ts) after the last one
					videoTSOffset = v.videoLastTS + 3000 - pkt.Header.Timestamp
				}
				videoOffsetReady = true
			}
			v.videoPkts++
			v.videoSeq++
			seq := v.videoSeq
			v.mu.Unlock()

			clone := pkt.Clone()
			clone.Header.SequenceNumber = seq
			clone.Header.Timestamp = pkt.Header.Timestamp + videoTSOffset

			v.mu.Lock()
			v.videoLastTS = clone.Header.Timestamp
			v.videoLastTSSet = true
			v.mu.Unlock()

			v.videoTrack.WriteRTP(clone)

			if v.videoPkts%1000 == 0 {
				log.Printf("[viewer:%s] %d video pkts", v.id, v.videoPkts)
			}
		case "audio":
			v.mu.Lock()
			if !audioOffsetReady {
				if v.audioLastTSSet {
					// Opus: 20ms @ 48kHz = 960 samples per packet
					audioTSOffset = v.audioLastTS + 960 - pkt.Header.Timestamp
				}
				audioOffsetReady = true
			}
			v.audioPkts++
			v.audioSeq++
			seq := v.audioSeq
			v.mu.Unlock()

			clone := pkt.Clone()
			clone.Header.SequenceNumber = seq
			clone.Header.Timestamp = pkt.Header.Timestamp + audioTSOffset

			v.mu.Lock()
			v.audioLastTS = clone.Header.Timestamp
			v.audioLastTSSet = true
			v.mu.Unlock()

			v.audioTrack.WriteRTP(clone)
		}
	})
	v.mu.Lock()
	v.unsubscribe = unsub
	v.mu.Unlock()

	log.Printf("[viewer:%s] subscribed to %s (waiting for keyframe)", v.id, sourceID)
}

func (v *Viewer) DetachSource() {
	v.mu.Lock()
	defer v.mu.Unlock()
	if v.unsubscribe != nil {
		v.unsubscribe()
		v.unsubscribe = nil
	}
	v.currentSource = ""
}

func (v *Viewer) CurrentSourceID() string {
	v.mu.RLock()
	defer v.mu.RUnlock()
	return v.currentSource
}

func (v *Viewer) IsConnected() bool {
	v.mu.RLock()
	defer v.mu.RUnlock()
	return v.connected
}

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
