package main

import (
	"log"
	"os"
	"sync"
	"sync/atomic"
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

	// Async delivery (MM_SWITCH_ASYNC_VIEWERS=true). See asyncViewersEnabled.
	async      bool
	queue      chan viewerItem
	stop       chan struct{}
	writerDone chan struct{}
	dropped    atomic.Int64

	// The switch this viewer belongs to, captured at construction.
	//
	// The auto-cleanup callback below used to read the package-level `mediaSwitch`
	// global from a PeerConnection callback goroutine — a goroutine pion spawns during
	// pc.Close(). That is a genuine data race (the race detector flags it), and it is
	// only latent in production because main() happens to assign the global once before
	// serving. Holding the reference removes the global read entirely.
	sw *MediaSwitch
}

// Viewer delivery mode.
//
// The source fans a packet out to every subscriber INLINE, on the goroutine reading the
// publisher's track, while holding the source's read lock. Each viewer's handler ends in
// `track.WriteRTP`, which can block when that viewer's transport is congested. One
// viewer on a bad network therefore stalls the fan-out for EVERY viewer — and for the
// recorder, which subscribes through the same path. The recorder already escaped this
// with a bounded async queue (MM_SWITCH_RECORDER_ISOLATION); this gives viewers the same
// treatment.
//
// Default OFF: this changes the delivery path for live media, and the synchronous path is
// what production has been running. Flip the flag to opt in.
const asyncViewersEnv = "MM_SWITCH_ASYNC_VIEWERS"

func asyncViewersEnabled() bool {
	v := os.Getenv(asyncViewersEnv)
	return v == "true" || v == "1"
}

const (
	// viewerQueueSize bounds a viewer's pending-write queue. ~512 RTP packets is on the
	// order of half a second of VP8+Opus — enough to absorb a brief network hiccup,
	// small enough that a permanently-wedged viewer costs bounded memory rather than
	// unbounded.
	viewerQueueSize = 512
	// viewerDrainTimeout bounds how long Close waits for the writer goroutine. A viewer
	// whose transport is wedged must not hang the caller (which may be the switch's own
	// lock-holding removal path).
	viewerDrainTimeout = 2 * time.Second
)

// viewerItem is one already-rewritten packet awaiting a WriteRTP.
type viewerItem struct {
	kind string
	pkt  *rtp.Packet
}

func NewViewer(id string, pc *webrtc.PeerConnection, sw *MediaSwitch) (*Viewer, error) {
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
		sw:         sw,
		async:      asyncViewersEnabled(),
	}
	if v.async {
		v.queue = make(chan viewerItem, viewerQueueSize)
		v.stop = make(chan struct{})
		v.writerDone = make(chan struct{})
		go v.writeLoop()
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

		// Auto-cleanup: remove from MediaSwitch so UDP ports release immediately.
		// Uses the captured `v.sw`, never the global — see the field comment.
		if state == webrtc.PeerConnectionStateFailed ||
			state == webrtc.PeerConnectionStateClosed ||
			state == webrtc.PeerConnectionStateDisconnected {
			if v.sw != nil {
				// RemoveViewerIf, not RemoveViewer: a stale viewer's PeerConnection can
				// report Closed long after a NEW viewer has taken over the same id, and
				// removing by id alone would close the live one.
				go v.sw.RemoveViewerIf(id, v)
			}
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

			v.deliver("video", clone)

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

			v.deliver("audio", clone)
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

// deliver hands a rewritten packet to this viewer's transport.
//
// Sync mode: WriteRTP inline, exactly as before — so a congested viewer still blocks the
// source's fan-out goroutine.
//
// Async mode: a non-blocking send onto the bounded queue. A full queue DROPS the packet
// (counted) rather than stalling the fan-out. Dropping is the right call for live media:
// a viewer too slow to keep up cannot be helped by making everyone else wait for them,
// and RTP is lossy by design.
//
// Packet ordering is preserved: sequence numbers and timestamps are still rewritten on
// the fan-out goroutine (cheap, non-blocking), the queue is FIFO, and exactly one writer
// goroutine drains it.
func (v *Viewer) deliver(kind string, pkt *rtp.Packet) {
	if !v.async {
		v.writeTrack(kind, pkt)
		return
	}
	select {
	case v.queue <- viewerItem{kind: kind, pkt: pkt}:
	default:
		// The queue is never closed (only `stop` is), so this send can never panic on a
		// closed channel — it just finds a full buffer and gives up.
		n := v.dropped.Add(1)
		if n%100 == 1 {
			log.Printf("[viewer:%s] write queue full — dropped %d packet(s)", v.id, n)
		}
	}
}

func (v *Viewer) writeTrack(kind string, pkt *rtp.Packet) {
	if kind == "video" {
		v.videoTrack.WriteRTP(pkt)
	} else {
		v.audioTrack.WriteRTP(pkt)
	}
}

// writeLoop drains the queue. One goroutine per viewer, so the WriteRTP that used to
// block the shared fan-out goroutine now blocks only this viewer's own.
func (v *Viewer) writeLoop() {
	defer close(v.writerDone)
	for {
		select {
		case <-v.stop:
			return
		case it := <-v.queue:
			v.writeTrack(it.kind, it.pkt)
		}
	}
}

// Dropped reports how many packets this viewer's queue has shed. Async mode only.
func (v *Viewer) Dropped() int64 { return v.dropped.Load() }

// Close tears the viewer down exactly once.
//
// Everything that can block runs with v.mu RELEASED, deliberately:
//
//   - `unsubscribe` takes the SOURCE's lock. The fan-out goroutine holds that same source
//     lock while calling into this viewer's handler, which takes v.mu. Holding v.mu here
//     while reaching for the source lock is a textbook lock-order inversion and deadlocks
//     the source — and with it every other viewer on it.
//   - the async drain waits up to viewerDrainTimeout, and a wedged viewer is exactly the
//     case this exists for.
//
// The `closed` flag is set under the lock before any of it, so a second Close returns
// immediately and `stop` is closed exactly once.
func (v *Viewer) Close() {
	v.mu.Lock()
	if v.closed {
		v.mu.Unlock()
		return
	}
	v.closed = true
	unsub := v.unsubscribe
	v.mu.Unlock()

	if unsub != nil {
		unsub()
	}
	if v.async {
		close(v.stop)
		select {
		case <-v.writerDone:
		case <-time.After(viewerDrainTimeout):
			log.Printf("[viewer:%s] writer goroutine did not stop within %s", v.id, viewerDrainTimeout)
		}
	}
	v.pc.Close()
}
