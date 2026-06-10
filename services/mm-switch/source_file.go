package main

import (
	"bufio"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"strings"
	"sync"
	"time"

	"github.com/pion/rtp"
	"github.com/pion/rtp/codecs"
	"github.com/pion/webrtc/v4/pkg/media/oggreader"
)

// IVF format reference: https://wiki.multimedia.cx/index.php/IVF
// 32-byte file header + per-frame: [4-byte frame size][8-byte raw PTS][frame data]
//
// We parse manually instead of using pion's ivfreader.ParseNextFrame because
// pion applies pts*denominator/numerator which doesn't match standard timebase
// semantics (pts * num / den = seconds).
type ivfHeader struct {
	width   uint16
	height  uint16
	num     uint32 // timebase numerator
	den     uint32 // timebase denominator
	frames  uint32
}

func parseIVFHeader(r io.Reader) (*ivfHeader, error) {
	var buf [32]byte
	if _, err := io.ReadFull(r, buf[:]); err != nil {
		return nil, fmt.Errorf("ivf header: %w", err)
	}
	if string(buf[0:4]) != "DKIF" {
		return nil, fmt.Errorf("ivf: bad signature")
	}
	if string(buf[8:12]) != "VP80" {
		return nil, fmt.Errorf("ivf: codec %q not VP80", string(buf[8:12]))
	}
	return &ivfHeader{
		width:  binary.LittleEndian.Uint16(buf[12:14]),
		height: binary.LittleEndian.Uint16(buf[14:16]),
		den:    binary.LittleEndian.Uint32(buf[16:20]),
		num:    binary.LittleEndian.Uint32(buf[20:24]),
		frames: binary.LittleEndian.Uint32(buf[24:28]),
	}, nil
}

// readIVFFrame returns (frame bytes, raw PTS in timebase ticks, error).
func readIVFFrame(r io.Reader) ([]byte, uint64, error) {
	var hdr [12]byte
	if _, err := io.ReadFull(r, hdr[:]); err != nil {
		return nil, 0, err
	}
	size := binary.LittleEndian.Uint32(hdr[0:4])
	pts := binary.LittleEndian.Uint64(hdr[4:12])
	frame := make([]byte, size)
	if _, err := io.ReadFull(r, frame); err != nil {
		return nil, 0, err
	}
	return frame, pts, nil
}

// FileSource produces VP8 RTP packets from an IVF file.
// Path may be a local filesystem path or an http(s):// URL (for an ad server).
// Each FileSource has its own independent playback state — per-viewer ads
// are achieved by creating one FileSource per viewer.
type FileSource struct {
	id   string
	path string
	loop bool

	mu          sync.RWMutex
	active      bool
	subscribers map[string]PacketHandler
	stopCh      chan struct{}
	stopped     bool

	// Cached first keyframe RTP packets. Delivered immediately to new
	// subscribers so they don't have to wait for the next natural keyframe.
	cachedKF []*rtp.Packet
}

func NewFileSource(id, path string, loop bool) (*FileSource, error) {
	if path == "" {
		return nil, errors.New("file source: path required")
	}
	src := &FileSource{
		id:          id,
		path:        path,
		loop:        loop,
		subscribers: make(map[string]PacketHandler),
		stopCh:      make(chan struct{}),
	}
	// Preload the first keyframe synchronously so it's cached before
	// any viewer subscribes. This eliminates the 4-second black screen
	// on first join (the viewer gets the keyframe instantly from cache).
	// For IVF files, preload the first substantial keyframe synchronously.
	// For WebM files, playWebM handles its own caching internally.
	if !strings.HasSuffix(path, ".webm") {
		if err := src.preloadKeyframe(); err != nil {
			log.Printf("[file-source:%s] preload warning: %v (will retry in playOnce)", id, err)
		}
	}
	go src.run()
	return src, nil
}

// preloadKeyframe opens the file, reads the first frame, caches it as
// VP8 RTP packets, then closes. playOnce() will re-open and play from
// the beginning — the preload only ensures the cache is warm.
func (s *FileSource) preloadKeyframe() error {
	rdr, closer, err := openSource(s.path)
	if err != nil {
		return err
	}
	if closer != nil {
		defer closer.Close()
	}
	buf := bufio.NewReaderSize(rdr, 64*1024)
	_, err = parseIVFHeader(buf)
	if err != nil {
		return err
	}
	// Scan for the first SUBSTANTIAL keyframe (>1KB). VP8 encoders often
	// produce a tiny (<500 byte) initial "setup" keyframe that decodes as
	// a nearly-black image. Browsers need a proper keyframe with enough
	// data to render visible content.
	const minKeyframeSize = 1024
	for i := 0; i < 200; i++ { // scan up to ~8 seconds at 24fps
		frame, _, ferr := readIVFFrame(buf)
		if ferr != nil {
			return ferr
		}
		isKeyframe := len(frame) > 0 && (frame[0]&0x01) == 0
		if isKeyframe && len(frame) >= minKeyframeSize {
			payloader := &codecs.VP8Payloader{EnablePictureID: true}
			rtpPayloads := payloader.Payload(1200, frame)
			for j, payload := range rtpPayloads {
				pkt := &rtp.Packet{
					Header: rtp.Header{
						Version:        2,
						PayloadType:    96,
						SequenceNumber: uint16(j + 1),
						Timestamp:      0,
						Marker:         j == len(rtpPayloads)-1,
						SSRC:           1,
					},
					Payload: payload,
				}
				s.cachedKF = append(s.cachedKF, pkt)
			}
			log.Printf("[file-source:%s] preloaded keyframe at frame %d (%d pkts, %d bytes)",
				s.id, i, len(s.cachedKF), len(frame))
			return nil
		}
	}
	log.Printf("[file-source:%s] no substantial keyframe found in first 200 frames", s.id)
	return nil
}

func (s *FileSource) run() {
	for {
		select {
		case <-s.stopCh:
			return
		default:
		}

		s.mu.Lock()
		s.active = true
		s.mu.Unlock()

		if strings.HasSuffix(s.path, ".webm") {
			s.playWebM() // WebM: demuxes both VP8 video + Opus audio
		} else {
			go s.emitAudio() // IVF: companion audio goroutine
			s.playOnce()     // IVF: video only
		}

		s.mu.Lock()
		s.active = false
		s.mu.Unlock()

		if !s.loop {
			log.Printf("[file-source:%s] finished (no loop)", s.id)
			return
		}
		// brief pause before looping to avoid hot-spinning if open fails
		select {
		case <-s.stopCh:
			return
		case <-time.After(500 * time.Millisecond):
		}
	}
}

var silentOpusFrame = []byte{0xF8}

// emitAudio reads a companion .ogg file (same basename as the IVF, e.g.
// bunny.ivf → bunny.ogg) and emits real Opus RTP alongside the video.
// Falls back to silent Opus if no .ogg file exists.
func (s *FileSource) emitAudio() {
	oggPath := strings.TrimSuffix(s.path, ".ivf") + ".ogg"

	rdr, closer, err := openSource(oggPath)
	if err != nil {
		log.Printf("[file-source:%s] no companion audio %s, emitting silence", s.id, oggPath)
		s.emitSilence()
		return
	}
	if closer != nil {
		defer closer.Close()
	}

	ogg, _, oggErr := oggreader.NewWith(rdr)
	if oggErr != nil {
		log.Printf("[file-source:%s] ogg parse error: %v, falling back to silence", s.id, oggErr)
		s.emitSilence()
		return
	}

	log.Printf("[file-source:%s] playing companion audio %s", s.id, oggPath)

	var seq uint16
	var ts uint32
	startTime := time.Now()
	pktCount := int64(0)
	var firstGranule uint64

	for {
		select {
		case <-s.stopCh:
			return
		default:
		}

		pageData, pageHdr, oggErr := ogg.ParseNextPage()
		if errors.Is(oggErr, io.EOF) {
			log.Printf("[file-source:%s] audio EOF after %d packets", s.id, pktCount)
			s.emitSilence()
			return
		}
		if oggErr != nil {
			log.Printf("[file-source:%s] audio error: %v", s.id, oggErr)
			s.emitSilence()
			return
		}

		// Skip header pages (OpusTags etc.) — granulePosition == 0.
		if pageHdr.GranulePosition == 0 {
			continue
		}
		if firstGranule == 0 {
			firstGranule = pageHdr.GranulePosition
		}

		// Pace by granule position (sample count at 48kHz).
		samplePos := pageHdr.GranulePosition - firstGranule
		presentNs := int64(samplePos) * int64(time.Second) / 48000
		dueAt := startTime.Add(time.Duration(presentNs))
		if delay := time.Until(dueAt); delay > 0 {
			select {
			case <-s.stopCh:
				return
			case <-time.After(delay):
			}
		}

		pktCount++
		seq++
		ts = uint32(samplePos) // use granule as RTP timestamp directly (both 48kHz)

		pkt := &rtp.Packet{
			Header: rtp.Header{
				Version:        2,
				PayloadType:    111,
				SequenceNumber: seq,
				Timestamp:      ts,
				Marker:         true, // each Opus frame is a complete audio unit
				SSRC:           2,
			},
			Payload: pageData,
		}

		s.fanout("audio", pkt)
	}
}

// fanout forwards one packet to all subscribers with panic-contained
// dispatch (see dispatch.go).
func (s *FileSource) fanout(kind string, pkt *rtp.Packet) {
	s.mu.RLock()
	panicked := dispatchAll(s.id, s.subscribers, kind, pkt)
	s.mu.RUnlock()
	quarantineSubscribers(s.id, &s.mu, s.subscribers, panicked)
}

// fanoutPackets forwards a burst of packets (one video frame) to all
// subscribers with panic-contained dispatch.
func (s *FileSource) fanoutPackets(kind string, pkts []*rtp.Packet) {
	s.mu.RLock()
	panicked := dispatchAllPackets(s.id, s.subscribers, kind, pkts)
	s.mu.RUnlock()
	quarantineSubscribers(s.id, &s.mu, s.subscribers, panicked)
}

func (s *FileSource) emitSilence() {
	ticker := time.NewTicker(20 * time.Millisecond)
	defer ticker.Stop()

	var seq uint16
	var ts uint32
	for {
		select {
		case <-s.stopCh:
			return
		case <-ticker.C:
		}
		seq++
		ts += 960
		pkt := &rtp.Packet{
			Header: rtp.Header{
				Version: 2, PayloadType: 111,
				SequenceNumber: seq, Timestamp: ts, SSRC: 2,
			},
			Payload: silentOpusFrame,
		}
		s.fanout("audio", pkt)
	}
}

// playOnce opens the IVF source, reads frames pacing them by their per-frame
// presentation timestamps. real_seconds = pts_ticks * num / den.
func (s *FileSource) playOnce() {
	rdr, closer, err := openSource(s.path)
	if err != nil {
		log.Printf("[file-source:%s] open %s failed: %v", s.id, s.path, err)
		time.Sleep(2 * time.Second)
		return
	}
	if closer != nil {
		defer closer.Close()
	}

	buf := bufio.NewReaderSize(rdr, 64*1024)

	header, err := parseIVFHeader(buf)
	if err != nil {
		log.Printf("[file-source:%s] %v", s.id, err)
		return
	}

	num := uint64(header.num)
	den := uint64(header.den)
	if num == 0 || den == 0 {
		num, den = 1, 1000
	}
	log.Printf("[file-source:%s] playing %s (%dx%d, timebase=%d/%d, %d frames)",
		s.id, s.path, header.width, header.height, num, den, header.frames)

	payloader := &codecs.VP8Payloader{EnablePictureID: true}
	var seq uint16
	var firstPTS uint64
	startTime := time.Now()
	frameCount := int64(0)

	for {
		select {
		case <-s.stopCh:
			return
		default:
		}

		frame, pts, ferr := readIVFFrame(buf)
		if errors.Is(ferr, io.EOF) || errors.Is(ferr, io.ErrUnexpectedEOF) {
			log.Printf("[file-source:%s] EOF after %d frames", s.id, frameCount)
			return
		}
		if ferr != nil {
			log.Printf("[file-source:%s] frame read error: %v", s.id, ferr)
			return
		}
		frameCount++

		if frameCount == 1 {
			firstPTS = pts
		}
		var ptsOffset uint64
		if pts >= firstPTS {
			ptsOffset = pts - firstPTS
		}

		// real time in nanoseconds = ptsOffset * num / den * 1e9
		presentNs := int64(ptsOffset) * int64(num) * int64(time.Second) / int64(den)
		dueAt := startTime.Add(time.Duration(presentNs))
		if delay := time.Until(dueAt); delay > 0 {
			select {
			case <-s.stopCh:
				return
			case <-time.After(delay):
			}
		}

		// RTP 90kHz timestamp: ptsOffset_ticks * num/den * 90000
		rtpTs := uint32(ptsOffset * num * 90000 / den)

		isKeyframe := len(frame) > 0 && (frame[0]&0x01) == 0
		rtpPayloads := payloader.Payload(1200, frame)

		var pkts []*rtp.Packet
		for i, payload := range rtpPayloads {
			seq++
			pkt := &rtp.Packet{
				Header: rtp.Header{
					Version:        2,
					PayloadType:    96,
					SequenceNumber: seq,
					Timestamp:      rtpTs,
					Marker:         i == len(rtpPayloads)-1,
					SSRC:           1,
				},
				Payload: payload,
			}
			pkts = append(pkts, pkt)
		}

		// Cache the first keyframe so new subscribers get instant video.
		if isKeyframe && len(s.cachedKF) == 0 {
			for _, p := range pkts {
				s.cachedKF = append(s.cachedKF, p.Clone())
			}
			log.Printf("[file-source:%s] keyframe cached (%d pkts)", s.id, len(s.cachedKF))
		}

		s.fanoutPackets("video", pkts)

		if frameCount == 1 {
			log.Printf("[file-source:%s] first frame (%d bytes, key=%v, pts=%d)", s.id, len(frame), isKeyframe, pts)
		}
		if frameCount%300 == 0 {
			log.Printf("[file-source:%s] %d frames (pts=%d, wallclock=%v)",
				s.id, frameCount, pts, time.Since(startTime).Round(time.Millisecond))
		}
	}
}

// openSource opens the path as a local file or HTTP(S) URL.
// Returns the reader plus an optional closer (caller must close if non-nil).
func openSource(path string) (io.Reader, io.Closer, error) {
	if strings.HasPrefix(path, "http://") || strings.HasPrefix(path, "https://") {
		resp, err := http.Get(path)
		if err != nil {
			return nil, nil, fmt.Errorf("http get: %w", err)
		}
		if resp.StatusCode >= 400 {
			resp.Body.Close()
			return nil, nil, fmt.Errorf("http status %d", resp.StatusCode)
		}
		return resp.Body, resp.Body, nil
	}
	f, err := os.Open(path)
	if err != nil {
		return nil, nil, err
	}
	return f, f, nil
}

func (s *FileSource) RequestKeyframe() {} // file source caches keyframe for new subscribers
func (s *FileSource) Type() string      { return "file" }
func (s *FileSource) IsActive() bool    { s.mu.RLock(); defer s.mu.RUnlock(); return s.active }
func (s *FileSource) Subscribe(id string, handler PacketHandler) func() {
	s.mu.Lock()
	s.subscribers[id] = handler
	// Deliver cached keyframe immediately so the viewer's decoder can start
	// without waiting for the next natural keyframe in the file.
	kf := s.cachedKF
	s.mu.Unlock()

	if len(kf) > 0 {
		for _, pkt := range kf {
			if !safeDispatch(s.id, id, handler, "video", pkt) {
				quarantineSubscribers(s.id, &s.mu, s.subscribers, []string{id})
				break
			}
		}
		log.Printf("[file-source:%s] replayed cached keyframe (%d pkts) to %s", s.id, len(kf), id)
	}

	return func() { s.mu.Lock(); defer s.mu.Unlock(); delete(s.subscribers, id) }
}
func (s *FileSource) Stop() {
	s.mu.Lock()
	defer s.mu.Unlock()
	if !s.stopped {
		s.stopped = true
		close(s.stopCh)
	}
}

var _ Source = (*FileSource)(nil)
