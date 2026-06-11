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
	// RecordingFailed: the recorder hit a panic or a persistent write
	// error and gave up. The partial .webm is kept on disk for salvage.
	// The string deliberately matches the 'failed' value mm-core's
	// mm_recordings_status_check constraint already permits (V016), so
	// the control plane can adopt it without a schema change.
	RecordingFailed RecordingState = "failed"
)

// MM_SWITCH_RECORDER_ISOLATION selects the recorder write path
// (ADR-04 Phase 1 rollback flag):
//   - "async" (default): packets are handed to a per-recorder writer
//     goroutine through a bounded queue; disk I/O never runs on the
//     fan-out goroutine, write-error streaks fail the recording.
//   - "inline": legacy pre-Phase-1 behavior — assembly + disk writes
//     happen synchronously on the fan-out goroutine, write errors are
//     logged only.
const (
	recorderIsolationEnv = "MM_SWITCH_RECORDER_ISOLATION"
	recorderModeAsync    = "async"
	recorderModeInline   = "inline"
)

func recorderIsolationMode() string {
	if os.Getenv(recorderIsolationEnv) == recorderModeInline {
		return recorderModeInline
	}
	return recorderModeAsync
}

const (
	// recorderQueueSize bounds the async write queue: ~1024 RTP packets
	// is on the order of 1-2 s of typical VP8+Opus media — enough to
	// ride out a short disk stall, small enough to cap memory.
	recorderQueueSize = 1024
	// recorderWriteErrorThreshold: consecutive block-write failures
	// before the recording flips to failed (async mode only).
	recorderWriteErrorThreshold = 10
	// recorderDrainTimeout bounds how long Finalise waits for the
	// writer goroutine to flush the queue — a wedged volume must not
	// hang the finalise HTTP handler (and mm-core's stream-end path).
	recorderDrainTimeout = 5 * time.Second
)

// recorderItem is one queued unit of work for the async writer.
type recorderItem struct {
	kind string
	pkt  *rtp.Packet
}

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

	// Async writer (MM_SWITCH_RECORDER_ISOLATION=async, the default).
	// onPacket enqueues; a single writer goroutine drains and performs
	// VP8 assembly + disk writes, so the fan-out goroutine's cost is
	// one non-blocking channel send regardless of disk behavior.
	async      bool
	queue      chan recorderItem
	writerDone chan struct{}

	// Consecutive block-write failures (guarded by mu). Reset on any
	// successful write; reaching recorderWriteErrorThreshold in async
	// mode flips the recording to RecordingFailed.
	writeErrStreak int

	// VP8 frame assembler — RTP packets within a frame share a
	// timestamp; the frame ends on a packet with the marker bit set.
	vp8Buf       []byte
	vp8FirstSeen bool
	vp8FirstRTP  uint32
	vp8LastRTP   uint32 // last RTP ts written (for pause shift maths)
	vp8KeyAhead  bool   // VP8 P-bit on next assembled frame's first packet
	vp8WroteKey  bool   // a keyframe has begun the file; until then we drop
	//                     leading inter-frames so the VOD is decodable (a file
	//                     that starts on a P-frame is a black-screen recording)

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
	// IMPORTANT: ebml-go emits TrackEntry fields in Go struct-literal
	// order. ExoPlayer's MatroskaExtractor is order-sensitive — TrackType
	// must appear before codec-data fields (CodecPrivate, CodecDelay,
	// SeekPreRoll, Audio sub-element) so the parser knows how to
	// interpret them. ffmpeg/Chrome-produced WebMs place TrackType near
	// the top of TrackEntry; we mirror that here.
	tracks := []webm.TrackEntry{
		{
			Name:        "Video",
			TrackNumber: 1,
			TrackUID:    1,
			TrackType:   1, // video — must come before CodecID per ExoPlayer
			CodecID:     "V_VP8",
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
			TrackType:   2, // audio — must precede CodecPrivate so ExoPlayer
			               // knows the track is audio before parsing
			               // Opus-specific data
			CodecID:     "A_OPUS",
			// Required by the WebM/Matroska spec for Opus tracks.
			// Without CodecPrivate, spec-compliant decoders
			// (ExoPlayer, AVFoundation, ffmpeg) refuse the file with
			// "Missing CodecPrivate for codec A_OPUS". Browsers are
			// lenient and play it anyway.
			//
			// Format: 19-byte OpusHead packet (RFC 7845 §5.1):
			//   "OpusHead" magic               (8 bytes)
			//   version = 1                    (1)
			//   channel count = 2 (stereo)     (1)
			//   pre-skip = 312 samples         (2 little-endian)
			//   input sample rate = 48000      (4 little-endian)
			//   output gain = 0                (2 little-endian)
			//   channel mapping family = 0     (1) — mono/stereo
			CodecPrivate: []byte{
				0x4f, 0x70, 0x75, 0x73, 0x48, 0x65, 0x61, 0x64,
				0x01,
				0x02,
				0x38, 0x01,
				0x80, 0xbb, 0x00, 0x00,
				0x00, 0x00,
				0x00,
			},
			// Opus codec delay = pre-skip × (1e9 / 48000) ns ≈ 6.5ms
			// (312 samples / 48 kHz). WebM specifies CodecDelay in ns.
			CodecDelay:  6500000,
			SeekPreRoll: 80000000, // 80 ms — the Opus reference value
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
	mode := recorderIsolationMode()
	r := &WebMRecorder{
		id:         id,
		path:       path,
		source:     src,
		state:      RecordingActive,
		file:       f,
		videoTrack: ws[0],
		audioTrack: ws[1],
		async:      mode == recorderModeAsync,
	}
	if r.async {
		r.queue = make(chan recorderItem, recorderQueueSize)
		r.writerDone = make(chan struct{})
		// The channel is passed by value: markFailed/Finalise nil out
		// r.queue (under r.mu) to stop intake, and that field write must
		// not race with the writer's loop.
		go r.writeLoop(r.queue)
	}
	r.unsubFn = src.Subscribe("recorder-"+id, r.onPacket)
	// Ask the publisher for an immediate keyframe (PLI). The keyframe-start
	// gate in handleVP8 drops video until the first keyframe arrives; without
	// this nudge the recording would begin only at the publisher's next
	// periodic keyframe (potentially seconds of dropped lead). Safe with no
	// video track (returns early).
	src.RequestKeyframe()
	log.Printf("[recorder:%s] started → %s (isolation=%s)", id, path, mode)
	return r, nil
}

// writeLoop is the single consumer of the async write queue. It owns
// all VP8 assembly + disk I/O for the recorder, preserving packet
// order (single channel, single consumer) so the pause-shift timeline
// bookkeeping keeps its existing semantics. A panic anywhere in the
// muxer path is contained here instead of killing the process.
func (r *WebMRecorder) writeLoop(queue <-chan recorderItem) {
	// Registered first so it runs last: the queue-depth series is
	// removed only after the final drain, never resurrected negative.
	defer recorderQueueDepth.DeleteLabelValues(r.id)
	defer close(r.writerDone)
	defer func() {
		if p := recover(); p != nil {
			recorderPanicsTotal.Inc()
			log.Printf("[recorder:%s] writer goroutine panicked: %v", r.id, p)
			r.markFailed(fmt.Sprintf("writer panic: %v", p), true)
		}
	}()
	for item := range queue {
		recorderQueueDepth.WithLabelValues(r.id).Dec()
		r.process(item.kind, item.pkt)
	}
}

// process performs the actual assembly + write for one packet. Runs on
// the writer goroutine in async mode, on the fan-out goroutine inline.
func (r *WebMRecorder) process(kind string, pkt *rtp.Packet) {
	switch kind {
	case "video":
		r.handleVP8(pkt)
	case "audio":
		r.handleOpus(pkt)
	}
}

// markFailed transitions the recorder to RecordingFailed: stops intake,
// unsubscribes from the source, stops the writer goroutine and closes
// the track writers so the partial .webm gets its trailer and the file
// descriptor is released. The partial file is kept on disk for salvage.
// fromWriter must be true when called from the writer goroutine itself
// (write-error threshold, writer panic) so we don't wait on our own
// exit. Idempotent; a no-op after Finalise.
func (r *WebMRecorder) markFailed(reason string, fromWriter bool) {
	r.mu.Lock()
	if r.closed {
		r.mu.Unlock()
		return
	}
	r.closed = true
	r.state = RecordingFailed
	unsub := r.unsubFn
	r.unsubFn = nil
	q := r.queue
	r.queue = nil
	vt, at := r.videoTrack, r.audioTrack
	r.videoTrack, r.audioTrack = nil, nil
	r.file = nil
	r.mu.Unlock()

	// All lock-taking side effects happen outside r.mu: unsub takes the
	// source mutex (fan-out holds source.RLock → r.mu, so nesting the
	// other way would be an ABBA deadlock).
	if unsub != nil {
		unsub()
	}
	if q != nil {
		// Senders are gone (closed=true is set under r.mu before any
		// enqueue attempt), so closing is safe. The writer drains the
		// remainder and exits; queued items see nil tracks and no-op.
		close(q)
		if !fromWriter {
			select {
			case <-r.writerDone:
			case <-time.After(recorderDrainTimeout):
				log.Printf("[recorder:%s] markFailed: writer did not exit within %s",
					r.id, recorderDrainTimeout)
			}
		}
	}
	if vt != nil {
		vt.Close()
	}
	if at != nil {
		at.Close()
	}
	log.Printf("[recorder:%s] FAILED (%s) — partial file kept at %s", r.id, reason, r.path)
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
// source. Idempotent; a no-op (state preserved) if the recorder
// already failed. In async mode the bounded queue is flushed first,
// with a drain timeout so a wedged disk can't hang the HTTP handler.
//
// NOTE: unsubscription deliberately happens *outside* r.mu. The
// fan-out path locks source.RLock → r.mu (onPacket), while unsubFn
// locks the source mutex — running it under r.mu was a latent ABBA
// deadlock in the pre-Phase-1 code.
func (r *WebMRecorder) Finalise() {
	r.mu.Lock()
	if r.closed {
		r.mu.Unlock()
		return
	}
	r.closed = true
	r.state = RecordingFinished
	unsub := r.unsubFn
	r.unsubFn = nil
	q := r.queue
	r.queue = nil
	r.mu.Unlock()

	if unsub != nil {
		unsub()
	}
	if q != nil {
		// Intake is stopped (closed=true under r.mu precedes any
		// enqueue), so close + bounded drain flushes buffered frames.
		close(q)
		select {
		case <-r.writerDone:
		case <-time.After(recorderDrainTimeout):
			log.Printf("[recorder:%s] finalise: writer did not drain within %s",
				r.id, recorderDrainTimeout)
		}
	}

	r.mu.Lock()
	vt, at := r.videoTrack, r.audioTrack
	r.videoTrack, r.audioTrack = nil, nil
	// SimpleBlockWriter closes the underlying writer per its docs —
	// don't close r.file again.
	r.file = nil
	r.mu.Unlock()
	if vt != nil {
		vt.Close()
	}
	if at != nil {
		at.Close()
	}
	log.Printf("[recorder:%s] finalised → %s", r.id, r.path)
	// Best-effort post-processing. Asynchronous so a slow ffmpeg
	// doesn't block the API caller; failure is logged but doesn't
	// surface. Thumbnail first (fast, feeds the VOD tile), then the
	// MP4 rendition (see transcode.go). State is seeded *before* the
	// goroutine so an immediate status poll reads "pending".
	setTranscodeState(r.id, "pending")
	go func() {
		r.generateThumbnail()
		if err := transcodeToMP4(r.id, r.path); err != nil {
			log.Printf("[recorder:%s] mp4 transcode failed: %v", r.id, err)
		}
	}()
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
//
// Async mode: the fan-out goroutine's cost is one packet clone + one
// non-blocking channel send; a full queue drops the packet (counted)
// instead of stalling live delivery. The clone is mandatory: pion's
// rtp.Packet.Unmarshal aliases the track-read buffer, which the source
// reuses for the next packet — handing the pointer across goroutines
// without copying would corrupt frames.
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
	if r.async {
		// Send under r.mu: markFailed/Finalise set closed=true under
		// the same mutex before closing the queue, so a send on a
		// closed channel is impossible by construction.
		select {
		case r.queue <- recorderItem{kind: kind, pkt: pkt.Clone()}:
			recorderQueueDepth.WithLabelValues(r.id).Inc()
		default:
			recorderDroppedPacketsTotal.Inc()
		}
		r.mu.Unlock()
		return
	}
	r.mu.Unlock()

	// Inline (legacy) path: assemble + write on the fan-out goroutine.
	r.process(kind, pkt)
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

	// Keyframe-start gate: a WebM/VP8 file MUST begin on a keyframe. If the
	// first block written is an inter-frame (recording started — or was
	// paused/resumed — mid-GOP, before any keyframe), the decoder has no
	// reference and the whole VOD is undecodable from frame 1 (the reported
	// black screen). Drop every leading P-frame until the first keyframe,
	// then rebaseline the file timeline to it so playback starts at t=0.
	if !r.vp8WroteKey {
		if !keyframe {
			r.mu.Unlock()
			return
		}
		r.vp8WroteKey = true
		r.vp8FirstRTP = pkt.Timestamp
	}

	r.vp8LastRTP = pkt.Timestamp
	// Convert RTP ts (90 kHz) → ms, less the file-baseline + pause shift.
	tsMs := int64(pkt.Timestamp-r.vp8FirstRTP-r.vp8Shift) * 1000 / 90000
	w := r.videoTrack
	r.mu.Unlock()
	if w != nil {
		_, err := w.Write(keyframe, tsMs, frame)
		r.noteWriteResult("vp8", err)
	}
}

// noteWriteResult tracks block-write outcomes. Any error increments the
// write-error counter; in async mode a streak of
// recorderWriteErrorThreshold consecutive failures flips the recording
// to RecordingFailed instead of logging forever (the pre-Phase-1
// behavior let a disk-full recording die silently and still be marked
// 'ready' by the control plane on stream end). Inline mode keeps the
// legacy log-only behavior.
func (r *WebMRecorder) noteWriteResult(track string, err error) {
	if err == nil {
		r.mu.Lock()
		r.writeErrStreak = 0
		r.mu.Unlock()
		return
	}
	recorderWriteErrorsTotal.Inc()
	log.Printf("[recorder:%s] %s write failed: %v", r.id, track, err)
	r.mu.Lock()
	r.writeErrStreak++
	streak := r.writeErrStreak
	async := r.async
	r.mu.Unlock()
	if async && streak >= recorderWriteErrorThreshold {
		// Async writes run exclusively on the writer goroutine.
		r.markFailed(fmt.Sprintf("%d consecutive write errors (last: %v)", streak, err), true)
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
		_, err := w.Write(true, tsMs, frame)
		r.noteWriteResult("opus", err)
	}
}
