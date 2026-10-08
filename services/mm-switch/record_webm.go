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
	"github.com/pion/webrtc/v4/pkg/media"
	"github.com/pion/webrtc/v4/pkg/media/samplebuilder"
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

	// VP8 frame assembler. pion's sample builder puts packets back in
	// sequence order, lets a late repair (RTX) fill its gap, and drops a
	// frame that is still incomplete once it is too old to be repaired. The
	// hand-rolled assembler this replaces appended payloads in ARRIVAL order,
	// so every duplicate or late packet was glued into whatever frame was
	// being built (the 2026-10-08 corrupt recordings). nil once finalised.
	vp8Builder   *samplebuilder.SampleBuilder
	vp8FirstSeen bool
	vp8FirstRTP  uint32
	vp8LastRTP   uint32 // last RTP ts written (for pause shift maths)
	vp8WroteKey  bool   // a keyframe has begun the file; until then we drop
	//                     leading inter-frames so the VOD is decodable (a file
	//                     that starts on a P-frame is a black-screen recording)
	// vp8AwaitKey: a frame went missing for good, so every inter-frame after
	// it references a picture the decoder never had. Skip to the next
	// keyframe rather than write garbage.
	vp8AwaitKey   bool
	vp8LastPID    uint16 // PictureID of the last frame built, for spotting a missing one
	vp8HavePID    bool
	lastKeyReq    time.Time // last keyframe request sent on the recording's behalf
	lastVideoTsMs int64     // ms position of the last written video block ≈ duration

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
		vp8Builder: newVP8SampleBuilder(),
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
	r.vp8Builder = nil
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
	// Packets were dropped for the whole pause, so the frames buffered
	// before it can never complete, and the first frames after it
	// reference pictures the file does not have. Start a fresh builder
	// and resume at a keyframe, asked for now rather than at the
	// publisher's next periodic one.
	if r.vp8Builder != nil {
		r.vp8Builder = newVP8SampleBuilder()
		r.vp8HavePID = false
		if r.vp8WroteKey {
			r.vp8AwaitKey = true
			if src := r.source; src != nil {
				r.lastKeyReq = time.Now()
				go src.RequestKeyframe()
			}
		}
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

	// Write out the frames still waiting in the sample builder (it holds
	// each frame until the next packet confirms it complete).
	r.mu.Lock()
	var tail []*media.Sample
	if b := r.vp8Builder; b != nil {
		b.Flush()
		tail = popSamples(b)
		r.vp8Builder = nil
	}
	r.mu.Unlock()
	r.writeVP8Samples(tail)

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
	// Record the finalised file size + playback duration so the status
	// endpoint can hand them to mm-core (size_bytes / duration_ms).
	var sizeBytes int64
	if info, err := os.Stat(r.path); err == nil {
		sizeBytes = info.Size()
	}
	r.mu.Lock()
	durationMs := r.lastVideoTsMs
	r.mu.Unlock()
	setRecordingMeta(r.id, sizeBytes, durationMs)
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
	// Cloned like the async path: the sample builder keeps packets past
	// this call, and the source reuses the buffer they alias.
	r.process(kind, pkt.Clone())
}

// How long the sample builder waits for a missing packet before giving up on
// its frame. The time limit is the one that binds at ordinary bitrates: 500 ms
// of media is several round trips for an RTX repair, and a frame given up on
// costs everything until the next keyframe, so a recording waits rather than
// drops. The packet limit only caps memory on a very high-rate stream.
//
// The time limit also bounds what Finalise can lose: pion's Flush (v4.2.11)
// discards every frame queued behind a gap that is still pending, while the
// ordinary streaming path handles a gap correctly. With the limit, only a
// packet lost in the last 500 ms of a recording can cost its tail.
const (
	recorderVP8MaxDelay = 500 * time.Millisecond
	recorderVP8MaxLate  = 1024
)

// recorderKeyframeRequestInterval rate-limits the keyframe requests the
// recorder sends while it waits out a broken reference chain.
const recorderKeyframeRequestInterval = time.Second

// vp8Head is what the sample builder records from a frame's first packet.
type vp8Head struct {
	hasPID bool
	pid    uint16
}

func newVP8SampleBuilder() *samplebuilder.SampleBuilder {
	return samplebuilder.New(recorderVP8MaxLate, &codecs.VP8Packet{}, 90000,
		samplebuilder.WithMaxTimeDelay(recorderVP8MaxDelay),
		samplebuilder.WithPacketHeadHandler(func(head any) any {
			p, ok := head.(*codecs.VP8Packet)
			if !ok {
				return vp8Head{}
			}
			return vp8Head{hasPID: p.I == 1, pid: p.PictureID}
		}))
}

func popSamples(b *samplebuilder.SampleBuilder) []*media.Sample {
	var out []*media.Sample
	for s := b.Pop(); s != nil; s = b.Pop() {
		out = append(out, s)
	}
	return out
}

// pictureIDFollows reports whether next is the PictureID right after prev, in
// either the 7-bit or the 15-bit form (RFC 7741 §4.2).
func pictureIDFollows(prev, next uint16) bool {
	return next == (prev+1)&0x7FFF || next == (prev+1)&0x7F
}

// handleVP8 feeds one RTP packet to the sample builder and writes every frame
// it completes. The packet must not be shared: the builder keeps it.
func (r *WebMRecorder) handleVP8(pkt *rtp.Packet) {
	r.mu.Lock()
	b := r.vp8Builder
	if b == nil {
		r.mu.Unlock()
		return
	}
	if !r.vp8FirstSeen {
		r.vp8FirstSeen = true
		r.vp8FirstRTP = pkt.Timestamp
	}
	b.Push(pkt)
	samples := popSamples(b)
	r.mu.Unlock()
	r.writeVP8Samples(samples)
}

// writeVP8Samples writes completed frames in order, each through the gates in
// admitVP8Locked.
func (r *WebMRecorder) writeVP8Samples(samples []*media.Sample) {
	for _, smp := range samples {
		r.mu.Lock()
		keyframe, tsMs, write, askKey := r.admitVP8Locked(smp)
		w := r.videoTrack
		src := r.source
		r.mu.Unlock()
		if askKey && src != nil {
			// Never synchronously: inline mode runs on the fan-out
			// goroutine, which holds the source's read lock, and
			// RequestKeyframe takes it again.
			go src.RequestKeyframe()
		}
		if write && w != nil {
			_, err := w.Write(keyframe, tsMs, smp.Data)
			r.noteWriteResult("vp8", err)
		}
	}
}

// admitVP8Locked decides whether a completed frame is written, and at which
// file time. Caller holds r.mu.
func (r *WebMRecorder) admitVP8Locked(smp *media.Sample) (keyframe bool, tsMs int64, write, askKey bool) {
	if len(smp.Data) == 0 {
		return false, 0, false, false
	}
	// VP8 keyframe iff the frame tag's P-bit (byte 0, bit 0) is clear.
	keyframe = smp.Data[0]&0x01 == 0
	ts := smp.PacketTimestamp

	// A frame at or before the last one written is a re-send that got this
	// far (the source drops most); writing it would put a block back in time.
	if r.vp8WroteKey && int32(ts-r.vp8LastRTP) <= 0 {
		return keyframe, 0, false, false
	}

	// Did a frame go missing before this one? PictureIDs say exactly; a
	// stream without them falls back to the builder's dropped-packet count
	// (which also counts padding, hence only as the fallback).
	head, _ := smp.Metadata.(vp8Head)
	var broken bool
	if head.hasPID {
		broken = r.vp8HavePID && !pictureIDFollows(r.vp8LastPID, head.pid)
		r.vp8LastPID, r.vp8HavePID = head.pid, true
	} else {
		broken = smp.PrevDroppedPackets > 0
		r.vp8HavePID = false
	}
	if keyframe {
		r.vp8AwaitKey = false
	} else if broken && r.vp8WroteKey {
		r.vp8AwaitKey = true
	}
	if r.vp8AwaitKey && time.Since(r.lastKeyReq) >= recorderKeyframeRequestInterval {
		r.lastKeyReq = time.Now()
		askKey = true
	}

	// Keyframe-start gate: a WebM/VP8 file MUST begin on a keyframe. If the
	// first block written is an inter-frame (recording started — or was
	// paused/resumed — mid-GOP, before any keyframe), the decoder has no
	// reference and the whole VOD is undecodable from frame 1 (the reported
	// black screen). Drop every leading P-frame until the first keyframe,
	// then rebaseline the file timeline to it so playback starts at t=0.
	if !r.vp8WroteKey {
		if !keyframe {
			return keyframe, 0, false, askKey
		}
		r.vp8WroteKey = true
		r.vp8FirstRTP = ts
	}
	if r.vp8AwaitKey {
		return keyframe, 0, false, askKey
	}

	r.vp8LastRTP = ts
	// Convert RTP ts (90 kHz) → ms, less the file-baseline + pause shift.
	tsMs = int64(ts-r.vp8FirstRTP-r.vp8Shift) * 1000 / 90000
	r.lastVideoTsMs = tsMs // tracks playback duration for the finalised file
	return keyframe, tsMs, true, askKey
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
	} else if int32(pkt.Timestamp-r.opusLastRTP) <= 0 {
		// A duplicate or a late packet: its slot in the file has passed.
		r.mu.Unlock()
		return
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
