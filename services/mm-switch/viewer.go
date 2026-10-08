package main

import (
	"log"
	"os"
	"sync"
	"sync/atomic"
	"time"

	"github.com/pion/rtcp"
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
	unsubscribe   func() // takes the SOURCE's lock: call it only with mu released (see Close)
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

	// Bumped by everything that takes or clears `unsubscribe` (SwitchTo, DetachSource, an
	// activation claiming the pending source). activateSource subscribes with mu released,
	// and only a generation unchanged since its claim lets it store the result: otherwise
	// the viewer has moved on, and storing would overwrite the newer unsubscribe and leak
	// that subscription.
	sourceGen uint64

	// ── Egress metering (FR-302a/b) ──────────────────────────────────────────
	//
	// Counted per viewer with an atomic add, and aggregated by source only when
	// someone reads. A shared map keyed by source would need a lock or a hash on
	// the packet path, which is the one place in this file that must stay cheap.
	egressBytes atomic.Int64
	// The PROGRAMME this viewer's bytes are billed to, which is not always the
	// source currently being sent. During an ad break `currentSource` is `ad-…`,
	// whose id names the viewer and not the broadcast — so billing that source
	// would produce bytes attributable to nobody. This holds the last `stream-…`
	// source instead, so an ad break bills the broadcast it interrupted.
	billingSource string

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

	// The source this viewer is subscribed to, for forwarding the phone's keyframe
	// requests (PLI/FIR) to the publisher, and when the last one went (rate limit).
	// Guarded by mu.
	activeSource        Source
	lastKeyframeRequest time.Time
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

	videoSender, err := pc.AddTrack(videoTrack)
	if err != nil {
		return nil, err
	}
	audioSender, err := pc.AddTrack(audioTrack)
	if err != nil {
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
	go v.readRTCP(videoSender)
	go v.readRTCP(audioSender)

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
	unsub := v.unsubscribe
	v.unsubscribe = nil
	v.sourceGen++
	v.currentSource = sourceID
	// Ad sources are `ad-{user}-{ts}` — the id names the viewer, not the broadcast —
	// so bytes sent during a break must still be billed to the programme the break
	// interrupted. Only a programme source moves the billing attribution.
	if isProgrammeSourceID(sourceID) {
		v.billingSource = sourceID
	}
	v.pendingSourceID = sourceID
	v.pendingSource = src
	v.mu.Unlock()

	// Before the activation below, so no packet from the old source can reach this viewer
	// after the new one starts.
	if unsub != nil {
		unsub()
	}

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
	prevUnsub := v.unsubscribe
	v.unsubscribe = nil
	v.sourceGen++
	gen := v.sourceGen
	v.mu.Unlock()

	if prevUnsub != nil {
		prevUnsub()
	}

	// Closure-local: per-source state, recreated on every switch
	waitingForKeyframe := true
	var videoTSOffset, audioTSOffset uint32
	var videoOffsetReady, audioOffsetReady bool
	preserveSeq := preservesPublisherSequence(src)
	var videoSeqMap, audioSeqMap seqMap

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
			seq, ok := videoSeqMap.next(&v.videoSeq, pkt.SequenceNumber, preserveSeq)
			if !ok {
				v.mu.Unlock()
				return
			}
			v.videoPkts++
			v.mu.Unlock()

			clone := pkt.Clone()
			clone.Header.SequenceNumber = seq
			clone.Header.Timestamp = pkt.Header.Timestamp + videoTSOffset

			v.mu.Lock()
			advanceLastTS(&v.videoLastTS, &v.videoLastTSSet, clone.Header.Timestamp, preserveSeq)
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
			seq, ok := audioSeqMap.next(&v.audioSeq, pkt.SequenceNumber, preserveSeq)
			if !ok {
				v.mu.Unlock()
				return
			}
			v.audioPkts++
			v.mu.Unlock()

			clone := pkt.Clone()
			clone.Header.SequenceNumber = seq
			clone.Header.Timestamp = pkt.Header.Timestamp + audioTSOffset

			v.mu.Lock()
			advanceLastTS(&v.audioLastTS, &v.audioLastTSSet, clone.Header.Timestamp, preserveSeq)
			v.mu.Unlock()

			v.deliver("audio", clone)
		}
	})
	v.mu.Lock()
	if v.closed || v.sourceGen != gen {
		// The viewer switched, detached or closed while Subscribe ran, and whoever did that
		// has already settled `unsubscribe`. Undo this subscription instead of storing it,
		// with mu released like every other unsubscribe (see Close).
		v.mu.Unlock()
		unsub()
		log.Printf("[viewer:%s] dropped stale subscription to %s", v.id, sourceID)
		return
	}
	v.unsubscribe = unsub
	v.activeSource = src
	v.mu.Unlock()

	log.Printf("[viewer:%s] subscribed to %s (waiting for keyframe)", v.id, sourceID)
}

// preservesPublisherSequence reports whether a source's packets carry the publisher's own
// RTP sequence numbers. Those must reach the viewer with their gaps and order intact: a
// gap is a lost packet the phone's jitter buffer should wait for, and a repair (RTX)
// arrives late and must slot back into its place. Renumbering them contiguously, as this
// file used to, told the phone a stale re-send was the next fresh packet, and frame
// assembly fell apart.
//
// File sources (ads) are packetized here, never lose or re-send anything, and replay
// their cached keyframe as seqs 1..n before restarting at 1, so they keep the plain
// counter that has always worked for them.
func preservesPublisherSequence(src Source) bool {
	return src.Type() == "webrtc"
}

// seqMap maps one source's RTP sequence numbers into a viewer's outgoing sequence space,
// so a source switch continues from where the previous source stopped.
type seqMap struct {
	ready  bool
	first  uint16 // outgoing seq of the first packet sent from this source
	offset uint16
	// settled: the viewer is more than a seqWindow past `first`, so the source can no
	// longer deliver anything older than `first` and the guard below is switched off for
	// good. It must not stay on: `out - first` keeps growing, and past 32768 packets
	// (~100 s of video) int16 reads it as negative and every packet would be refused.
	settled bool
}

// next returns the outgoing sequence number for `in` and advances *last (the highest
// number this viewer has sent). With preserve, the publisher's spacing is kept: gaps stay
// gaps and a late packet keeps its place. A packet older than the first one sent from this
// source is refused: its number would land in the previous source's range.
func (m *seqMap) next(last *uint16, in uint16, preserve bool) (uint16, bool) {
	if !preserve {
		*last++
		return *last, true
	}
	if !m.ready {
		m.ready = true
		m.first = *last + 1
		m.offset = m.first - in
	}
	out := in + m.offset
	// The source's seqWindow never lets through a packet a full window behind its newest,
	// so one this far behind what we sent means the publisher restarted its numbering:
	// continue right after the last number sent rather than send ancient history.
	if int(int16(out-*last)) <= -seqWindowSize {
		m.offset = *last + 1 - in
		out = *last + 1
	}
	if !m.settled && int16(out-m.first) < 0 {
		return 0, false
	}
	if int16(out-*last) > 0 {
		*last = out
	}
	if !m.settled && int(int16(*last-m.first)) >= seqWindowSize {
		m.settled = true
	}
	return out, true
}

// advanceLastTS records the timestamp the next source switch continues from. A late packet
// carries an older timestamp than one already sent, so with publisher sequencing only a
// newer timestamp moves it.
func advanceLastTS(last *uint32, set *bool, ts uint32, preserve bool) {
	if preserve && *set && int32(ts-*last) <= 0 {
		return
	}
	*last = ts
	*set = true
}

// readRTCP drains the RTCP the phone sends for one of this viewer's tracks until the
// PeerConnection closes. Reading is what makes pion's interceptors see it: without this
// loop the phone's NACKs were never answered from the retransmission buffer, and its
// keyframe requests went nowhere.
func (v *Viewer) readRTCP(sender *webrtc.RTPSender) {
	for {
		pkts, _, err := sender.ReadRTCP()
		if err != nil {
			return
		}
		v.handleRTCP(pkts)
	}
}

// viewerKeyframeRequestInterval rate-limits the keyframe requests one viewer can pass on
// to the publisher. A phone whose decoder is stuck repeats PLI every few hundred ms, and
// every request costs the publisher a large keyframe.
const viewerKeyframeRequestInterval = time.Second

// handleRTCP passes a phone's keyframe request (PLI or FIR) on to the current source.
func (v *Viewer) handleRTCP(pkts []rtcp.Packet) {
	for _, p := range pkts {
		switch p.(type) {
		case *rtcp.PictureLossIndication, *rtcp.FullIntraRequest:
			v.mu.Lock()
			src := v.activeSource
			now := time.Now()
			if src == nil || now.Sub(v.lastKeyframeRequest) < viewerKeyframeRequestInterval {
				v.mu.Unlock()
				return
			}
			v.lastKeyframeRequest = now
			v.mu.Unlock()
			src.RequestKeyframe()
			return
		}
	}
}

func (v *Viewer) DetachSource() {
	v.mu.Lock()
	unsub := v.unsubscribe
	v.unsubscribe = nil
	v.sourceGen++
	v.currentSource = ""
	v.activeSource = nil
	v.mu.Unlock()

	if unsub != nil {
		unsub()
	}
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
		viewerDroppedPacketsTotal.Inc()
		n := v.dropped.Add(1)
		if n%100 == 1 {
			log.Printf("[viewer:%s] write queue full — dropped %d packet(s)", v.id, n)
		}
	}
}

func (v *Viewer) writeTrack(kind string, pkt *rtp.Packet) {
	// Metered here because this is the ONE place both delivery modes converge:
	// sync calls it from the fan-out goroutine and async from the writer goroutine.
	// Counting in `deliver` instead would double-count nothing but would also miss
	// nothing — except that a dropped packet never reaches a wire, and billing for
	// bytes we did not send is the error that is hardest to defend.
	//
	// MarshalSize is the RTP header plus payload; the transport overhead the
	// provider also bills is added per packet (see egressOverheadBytes).
	n := int64(pkt.MarshalSize()) + egressOverheadBytes
	v.egressBytes.Add(n)
	egressBytesTotal.Add(float64(n))

	if kind == "video" {
		v.videoTrack.WriteRTP(pkt)
	} else {
		v.audioTrack.WriteRTP(pkt)
	}
}

// EgressSnapshot returns the billing source and cumulative bytes for this viewer.
//
// Cumulative, never reset: the reader computes deltas, and a counter this side could
// reset would lose whatever was delivered between a reset and the next poll.
func (v *Viewer) EgressSnapshot() (string, int64) {
	v.mu.RLock()
	src := v.billingSource
	v.mu.RUnlock()
	return src, v.egressBytes.Load()
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
	v.activeSource = nil
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
