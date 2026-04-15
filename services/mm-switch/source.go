package main

import (
	"github.com/pion/rtp"
)

// Source represents an input source that produces RTP packets.
// Sources can be live WebRTC streams, video files, URLs, etc.
type Source interface {
	// Type returns the source type: "webrtc", "file", "url"
	Type() string

	// IsActive returns whether the source is currently producing media.
	IsActive() bool

	// Subscribe adds a callback to receive RTP packets from this source.
	// Returns an unsubscribe function.
	Subscribe(id string, handler PacketHandler) func()

	// Stop terminates the source.
	Stop()
}

// PacketHandler receives RTP packets from a source.
// kind is "audio" or "video".
type PacketHandler func(kind string, pkt *rtp.Packet)
