// SPDX-License-Identifier: Apache-2.0
//
// WebMRecorder taps a WebRTCSource's RTP fan-out and writes
// VP8 video + Opus audio frames to a single .webm file across an
// entire stream session — including pause/resume.
//
// Why mm-switch (vs. mm-core/LiveKit egress): mm-switch is the only
// component that already touches every RTP packet from the host (it's
// relaying for viewers). Adding a recording subscriber is one tap on
// the existing fan-out, no second media pipeline.
//
// Why WebM: the wire codecs (VP8 + Opus) are exactly what WebM
// natively carries. No transcoding, no second encoder running for the
// recording. WebM also tolerates streaming-write — we don't have to
// rewrite the trailer at finalise like MP4 does. Browsers + ExoPlayer
// + AVFoundation all play VP8/Opus WebM natively.
//
// Pause/resume: the file handle stays open the entire session. While
// paused, packets are dropped and a per-track "pause shift" is
// accumulated. When resuming, the pause shift is added to the
// timestamp baseline so the file's timeline collapses out the gap —
// no "30 second freeze frame" in the output.

package main

import (
	"context"
	"fmt"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/at-wat/ebml-go/webm"
	"github.com/pion/rtp"
	"github.com/pion/rtp/codecs"
)

// RecordingState is the externally-visible state of a recorder.
type RecordingState string

const (
	RecordingActive   RecordingState = "recording"
	RecordingPaused   RecordingState = "paused"
	RecordingFinished RecordingState = "finished"
)

// WebMRecorder writes a single .webm to disk by subscribing to a
// WebRTCSource. One recorder per stream session.
type WebMRecorder struct {
	id       string
	path     string
	source   *WebRTCSource
	unsubFn  func()

	mu      sync.Mutex
	state   RecordingState
	closed  bool

	file       *os.File
	videoTrack webm.BlockWriteCloser
	audioTrack webm.BlockWriteCloser

	// VP8 frame assembler — RTP packets within a frame share a
	// timestamp; the frame ends on a packet with the marker bit set.
	vp8Buf       []byte
	vp8FirstSeen bool
	vp8FirstRTP  uint32
	vp8LastRTP   uint32 // last RTP ts written (for pause shift maths)
	vp8KeyAhead  bool   // VP8 P-bit on next assembled frame's first packet

	// Opus depacketizer — every RTP packet carries one complete frame.
	opusFirstSeen bool
	opusFirstRTP  uint32
	opusLastRTP   uint32

	// Pause-shift accumulator (in RTP units, per-track). When recording
	// is resumed we increment this by (resume_rtp - last_rtp_kept) so
	// the file timeline stays continuous.
	vp8Shift  uint32
	opusShift uint32

	// Track latest RTP seen across the whole pipeline (even while paused)
	// so we can compute the resume shift the moment the next packet
	// arrives.
	lastSeenVideoRTP uint32
	lastSeenAudioRTP uint32
	pausedAtVideo    uint32 // last RTP we kept for video
	pausedAtAudio    uint32 // last RTP we kept for audio
}

// NewWebMRecorder opens the output file, writes the WebM header (EBML
// + Tracks), starts subscribed in active state, and returns the
// recorder. The file is held open across pause/resume; only Finalise
// closes it.
func NewWebMRecorder(id, path string, src *WebRTCSource) (*WebMRecorder, error) {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return nil, fmt.Errorf("mkdir recordings: %w", err)
	}
	f, err := os.Create(path)
	if err != nil {
		return nil, fmt.Errorf("create %s: %w", path, err)
	}
	tracks := []webm.TrackEntry{
		{
			Name:        "Video",
			TrackNumber: 1,
			TrackUID:    1,
			CodecID:     "V_VP8",
			TrackType:   1, // video
			Video: &webm.Video{
				// Provisional — most VP8 streams are 720x480 from our
				// host pipeline. Real VP8 keyframes carry resolution in
				// the bitstream so players don't strictly rely on this.
				PixelWidth:  720,
				PixelHeight: 480,
			},
		},
		{
			Name:        "Audio",
			TrackNumber: 2,
			TrackUID:    2,
			CodecID:     "A_OPUS",
			TrackType:   2, // audio
			Audio: &webm.Audio{
				SamplingFrequency: 48000,
				Channels:          2,
			},
		},
	}
	ws, err := webm.NewSimpleBlockWriter(f, tracks)
	if err != nil {
		f.Close()
		return nil, fmt.Errorf("webm writer: %w", err)
	}
	if len(ws) != 2 {
		for _, w := range ws {
			w.Close()
		}
		f.Close()
		return nil, fmt.Errorf("webm: expected 2 tracks, got %d", len(ws))
	}
	r := &WebMRecorder{
		id:         id,
		path:       path,
		source:     src,
		state:      RecordingActive,
		file:       f,
		videoTrack: ws[0],
		audioTrack: ws[1],
	}
	r.unsubFn = src.Subscribe("recorder-"+id, r.onPacket)
	log.Printf("[recorder:%s] started → %s", id, path)
	return r, nil
}

// Pause stops accepting packets without closing the file. Idempotent.
func (r *WebMRecorder) Pause() {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.state != RecordingActive {
		return
	}
	r.state = RecordingPaused
	r.pausedAtVideo = r.vp8LastRTP
	r.pausedAtAudio = r.opusLastRTP
	log.Printf("[recorder:%s] paused", r.id)
}

// Resume continues writing to the same file. The timestamp pause-shift
// is applied so the file timeline collapses out the wall-clock gap.
// Idempotent.
func (r *WebMRecorder) Resume() {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.state != RecordingPaused {
		return
	}
	// Bump shift by the gap between the last accepted frame and the
	// most recent packet seen during pause. If no packets arrived
	// during pause (nothing changed), shift is unchanged.
	if r.vp8FirstSeen {
		r.vp8Shift += r.lastSeenVideoRTP - r.pausedAtVideo
	}
	if r.opusFirstSeen {
		r.opusShift += r.lastSeenAudioRTP - r.pausedAtAudio
	}
	r.state = RecordingActive
	log.Printf("[recorder:%s] resumed (vp8Shift=%d opusShift=%d)",
		r.id, r.vp8Shift, r.opusShift)
}

// Finalise closes the WebM trailer + file and unsubscribes from the
// source. Idempotent.
func (r *WebMRecorder) Finalise() {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.closed {
		return
	}
	r.closed = true
	r.state = RecordingFinished
	if r.unsubFn != nil {
		r.unsubFn()
		r.unsubFn = nil
	}
	if r.videoTrack != nil {
		r.videoTrack.Close()
	}
	if r.audioTrack != nil {
		r.audioTrack.Close()
	}
	// SimpleBlockWriter closes the underlying writer per its docs —
	// don't close r.file again.
	r.file = nil
	log.Printf("[recorder:%s] finalised → %s", r.id, r.path)
	// Best-effort thumbnail. Asynchronous so a slow ffmpeg doesn't
	// block the API caller; failure is logged but doesn't surface.
	go r.generateThumbnail()
}

// generateThumbnail extracts a single JPG frame from the finalised
// .webm via ffmpeg. Tries 3s first (a stable post-keyframe moment),
// falls back to 0s for very short recordings. Output goes alongside
// the .webm so nginx serves it under the same /_mm/recordings prefix
// without any new mount.
func (r *WebMRecorder) generateThumbnail() {
	thumbPath := strings.TrimSuffix(r.path, ".webm") + ".jpg"
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	for _, offset := range []string{"00:00:03", "00:00:00.5"} {
		cmd := exec.CommandContext(ctx,
			"ffmpeg",
			"-loglevel", "error",
			"-y",
			"-ss", offset,
			"-i", r.path,
			"-frames:v", "1",
			"-vf", "scale='min(640,iw)':-1",
			"-q:v", "5",
			thumbPath,
		)
		out, err := cmd.CombinedOutput()
		if err == nil {
			info, statErr := os.Stat(thumbPath)
			if statErr == nil && info.Size() > 0 {
				log.Printf("[recorder:%s] thumbnail at %s (offset=%s, %d bytes)",
					r.id, thumbPath, offset, info.Size())
				return
			}
		}
		log.Printf("[recorder:%s] ffmpeg thumbnail offset=%s failed: %v %s",
			r.id, offset, err, strings.TrimSpace(string(out)))
	}
	log.Printf("[recorder:%s] thumbnail generation gave up", r.id)
}

// State reports the current state. Lock-protected.
func (r *WebMRecorder) State() RecordingState {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.state
}

// Path is the output file path on disk.
func (r *WebMRecorder) Path() string { return r.path }

// onPacket is the PacketHandler the source's fan-out calls.
// We always update last-seen so resume can compute a proper shift,
// but we only assemble + write while state == RecordingActive.
func (r *WebMRecorder) onPacket(kind string, pkt *rtp.Packet) {
	r.mu.Lock()
	if r.closed {
		r.mu.Unlock()
		return
	}
	if kind == "video" {
		r.lastSeenVideoRTP = pkt.Timestamp
	} else {
		r.lastSeenAudioRTP = pkt.Timestamp
	}
	if r.state != RecordingActive {
		r.mu.Unlock()
		return
	}
	r.mu.Unlock()

	switch kind {
	case "video":
		r.handleVP8(pkt)
	case "audio":
		r.handleOpus(pkt)
	}
}

// handleVP8 reassembles a VP8 access unit from one or more RTP
// packets sharing the same timestamp; emits a SimpleBlock the moment
// the marker bit fires.
func (r *WebMRecorder) handleVP8(pkt *rtp.Packet) {
	vp8 := &codecs.VP8Packet{}
	payload, err := vp8.Unmarshal(pkt.Payload)
	if err != nil {
		return
	}

	r.mu.Lock()
	if !r.vp8FirstSeen {
		r.vp8FirstSeen = true
		r.vp8FirstRTP = pkt.Timestamp
	}
	// Start of a new frame — flush stale buffer.
	if vp8.S == 1 && vp8.PID == 0 {
		r.vp8Buf = r.vp8Buf[:0]
		// VP8 keyframe iff payload[0] bit 0 == 0 (P-bit clear).
		// Full bitstream check: byte 0 of the frame's first packet.
		if len(payload) > 0 {
			r.vp8KeyAhead = (payload[0] & 0x01) == 0
		}
	}
	r.vp8Buf = append(r.vp8Buf, payload...)
	if !pkt.Marker {
		r.mu.Unlock()
		return
	}
	// End of frame — emit.
	frame := append([]byte(nil), r.vp8Buf...)
	keyframe := r.vp8KeyAhead
	r.vp8Buf = r.vp8Buf[:0]
	r.vp8LastRTP = pkt.Timestamp
	// Convert RTP ts (90 kHz) → ms, less the file-baseline + pause shift.
	tsMs := int64(pkt.Timestamp-r.vp8FirstRTP-r.vp8Shift) * 1000 / 90000
	w := r.videoTrack
	r.mu.Unlock()
	if w != nil {
		if _, err := w.Write(keyframe, tsMs, frame); err != nil {
			log.Printf("[recorder:%s] vp8 write failed: %v", r.id, err)
		}
	}
}

// handleOpus is much simpler — each RTP packet carries one complete
// Opus frame, no assembly needed.
func (r *WebMRecorder) handleOpus(pkt *rtp.Packet) {
	r.mu.Lock()
	if !r.opusFirstSeen {
		r.opusFirstSeen = true
		r.opusFirstRTP = pkt.Timestamp
	}
	r.opusLastRTP = pkt.Timestamp
	tsMs := int64(pkt.Timestamp-r.opusFirstRTP-r.opusShift) * 1000 / 48000
	w := r.audioTrack
	frame := append([]byte(nil), pkt.Payload...)
	r.mu.Unlock()
	if w != nil {
		if _, err := w.Write(true, tsMs, frame); err != nil {
			log.Printf("[recorder:%s] opus write failed: %v", r.id, err)
		}
	}
}
