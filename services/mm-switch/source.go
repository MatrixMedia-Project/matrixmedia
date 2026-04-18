package main

import "github.com/pion/rtp"

// Source represents an input that produces video/audio RTP packets.
// Sources forward original RTP packets — no depacketization, no conversion.
type Source interface {
	Type() string
	IsActive() bool

	// Subscribe delivers original RTP packets to the handler.
	// The handler MUST NOT modify the packet — clone first if needed.
	// Returns an unsubscribe function.
	Subscribe(id string, handler PacketHandler) func()

	// RequestKeyframe asks the source to produce a keyframe ASAP.
	// May be a no-op if the source doesn't support PLI.
	RequestKeyframe()

	Stop()
}

// PacketHandler receives original RTP packets from a source.
// kind: "video" or "audio"
// pkt: original RTP packet — do NOT modify, clone first
type PacketHandler func(kind string, pkt *rtp.Packet)
