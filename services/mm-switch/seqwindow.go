// SPDX-License-Identifier: Apache-2.0
//
// seqWindow — per-track duplicate filter for a publisher's RTP.
//
// pion/webrtc v4 unwraps RTX and returns the repaired packets from TrackRemote.Read with
// their ORIGINAL sequence numbers, interleaved with the live stream. Most of that RTX is
// not repair at all: libwebrtc senders (Firefox, Chrome, the Android and iOS apps) probe
// bandwidth by re-sending packets the receiver already has. Measured on 2026-10-08, a
// 1.9 Mbps VP8 publisher with ~1.3% loss sent 1.2 Mbps of RTX, at least 98% of it
// duplicates. Fanned out as-is they corrupted every recording and every viewer's frame
// assembly. The window lets each sequence number through exactly once and still admits a
// late repair for a packet that genuinely went missing.

package main

const (
	// seqWindowSize is how far behind the newest packet a repair may still arrive and be
	// accepted: ~3 s at the ~330 packets/s of a 2 Mbps VP8 stream. Anything older is no
	// use to a viewer's jitter buffer or the recorder's sample builder anyway. Must divide
	// 65536 so that seq % seqWindowSize stays consistent across the 16-bit wrap.
	seqWindowSize = 1024
	// seqResyncRun: this many CONSECUTIVE sequence numbers that are all far outside the
	// window mean the publisher restarted its numbering, not that it is replaying history
	// (probe re-sends are scattered, never a long consecutive run). The window then
	// re-anchors instead of dropping the stream forever.
	seqResyncRun = 32
)

type seqWindow struct {
	started bool
	highest uint16
	seen    [seqWindowSize / 64]uint64

	staleNext uint16
	staleRun  int
}

// accept reports whether seq is new to this track, recording it if so.
func (w *seqWindow) accept(seq uint16) bool {
	if !w.started {
		w.restart(seq)
		return true
	}
	ahead := int(int16(seq - w.highest))
	switch {
	case ahead > 0:
		if ahead >= seqWindowSize {
			w.seen = [seqWindowSize / 64]uint64{}
		} else {
			for s := w.highest + 1; s != seq; s++ {
				w.clear(s)
			}
		}
		w.highest = seq
		w.mark(seq)
		w.staleRun = 0
		return true
	case -ahead < seqWindowSize:
		if w.has(seq) {
			return false
		}
		w.mark(seq)
		return true
	default:
		if w.staleRun > 0 && seq == w.staleNext {
			w.staleRun++
		} else {
			w.staleRun = 1
		}
		w.staleNext = seq + 1
		if w.staleRun >= seqResyncRun {
			w.restart(seq)
			return true
		}
		return false
	}
}

func (w *seqWindow) restart(seq uint16) {
	*w = seqWindow{started: true, highest: seq}
	w.mark(seq)
}

func (w *seqWindow) slot(seq uint16) (int, uint64) {
	i := int(seq) % seqWindowSize
	return i / 64, 1 << (uint(i) % 64)
}

func (w *seqWindow) mark(seq uint16)     { i, b := w.slot(seq); w.seen[i] |= b }
func (w *seqWindow) clear(seq uint16)    { i, b := w.slot(seq); w.seen[i] &^= b }
func (w *seqWindow) has(seq uint16) bool { i, b := w.slot(seq); return w.seen[i]&b != 0 }
