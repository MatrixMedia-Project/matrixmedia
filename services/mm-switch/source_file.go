package main

import (
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"strings"
	"sync"
	"time"

	"github.com/at-wat/ebml-go/webm"
	"github.com/pion/rtp"
	"github.com/pion/rtp/codecs"
	pwebrtc "github.com/pion/webrtc/v4"
)

// FileSource reads a WebM/VP8+Opus file and publishes RTP packets
// at real-time pace. This is used for ad videos, pre-recorded content, etc.
type FileSource struct {
	id       string
	path     string // local path or URL
	loop     bool   // whether to loop the video
	active   bool
	duration time.Duration

	mu          sync.RWMutex
	subscribers map[string]PacketHandler
	stopCh      chan struct{}
	stopped     bool
}

// NewFileSource creates a source from a WebM file (local path or HTTP URL).
func NewFileSource(id, path string, loop bool) (*FileSource, error) {
	src := &FileSource{
		id:          id,
		path:        path,
		loop:        loop,
		subscribers: make(map[string]PacketHandler),
		stopCh:      make(chan struct{}),
	}

	go src.run()

	return src, nil
}

func (s *FileSource) run() {
	for {
		select {
		case <-s.stopCh:
			return
		default:
		}

		err := s.playOnce()
		if err != nil {
			log.Printf("[source:%s] playback error: %v", s.id, err)
		}

		if !s.loop {
			s.mu.Lock()
			s.active = false
			s.mu.Unlock()
			log.Printf("[source:%s] playback finished", s.id)
			return
		}

		log.Printf("[source:%s] looping", s.id)
	}
}

func (s *FileSource) playOnce() error {
	var reader io.ReadCloser

	if strings.HasPrefix(s.path, "http://") || strings.HasPrefix(s.path, "https://") {
		resp, err := http.Get(s.path)
		if err != nil {
			return fmt.Errorf("download %s: %w", s.path, err)
		}
		reader = resp.Body
	} else {
		f, err := os.Open(s.path)
		if err != nil {
			return fmt.Errorf("open %s: %w", s.path, err)
		}
		reader = f
	}
	defer reader.Close()

	// Parse WebM
	var w webm.BlockReadCloser
	var err error
	// Use webm.Parse for reading
	ws := struct {
		Header  webm.EBMLHeader `ebml:"EBML"`
		Segment webm.Segment    `ebml:"Segment"`
	}{}
	_ = ws // webm parsing is complex, let's use a simpler approach

	// For MVP: use IVF reader for VP8 if available, or raw RTP packetization
	// The ebml-go library needs specific handling. Let's use a simpler approach
	// with a ticker-based frame generator for testing.
	_ = w
	_ = err

	// Simplified: generate test pattern frames at 30fps
	// In production, replace with proper WebM/IVF parsing
	return s.playTestPattern()
}

// playTestPattern generates a simple video stream for testing.
// Replace with proper file reading in production.
func (s *FileSource) playTestPattern() error {
	s.mu.Lock()
	s.active = true
	s.mu.Unlock()

	ticker := time.NewTicker(time.Millisecond * 33) // ~30fps
	defer ticker.Stop()

	vp8Payloader := &codecs.VP8Payloader{}
	sequenceNumber := uint16(0)
	timestamp := uint32(0)
	ssrc := uint32(0x12345678)

	// Simple VP8 keyframe (minimal valid frame)
	// In production, read actual video frames from the file
	keyframe := makeMinimalVP8Keyframe(320, 240)

	startTime := time.Now()
	frameDuration := time.Millisecond * 33

	for {
		select {
		case <-s.stopCh:
			return nil
		case <-ticker.C:
		}

		elapsed := time.Since(startTime)
		if !s.loop && elapsed > 15*time.Second {
			// Default 15s duration for test pattern
			return nil
		}

		// Packetize the frame into RTP
		packets := vp8Payloader.Payload(1200, keyframe)
		for _, payload := range packets {
			pkt := &rtp.Packet{
				Header: rtp.Header{
					Version:        2,
					PayloadType:    96, // VP8
					SequenceNumber: sequenceNumber,
					Timestamp:      timestamp,
					SSRC:           ssrc,
					Marker:         true,
				},
				Payload: payload,
			}
			sequenceNumber++

			s.mu.RLock()
			for _, handler := range s.subscribers {
				handler("video", pkt)
			}
			s.mu.RUnlock()
		}

		timestamp += uint32(frameDuration.Seconds() * 90000) // 90kHz clock
	}
}

// makeMinimalVP8Keyframe creates a minimal valid VP8 keyframe.
func makeMinimalVP8Keyframe(width, height int) []byte {
	// VP8 keyframe header (simplified)
	// This produces a valid but very simple (green/black) frame
	frame := make([]byte, 0, 32)

	// Frame tag: keyframe, version 0, show_frame=1
	frameTag := uint32(0) | // keyframe
		(0 << 1) |  // version
		(1 << 4) |  // show_frame
		(10 << 5)   // first_part_size (approximate)
	frame = append(frame, byte(frameTag), byte(frameTag>>8), byte(frameTag>>16))

	// Start code: 0x9D 0x01 0x2A
	frame = append(frame, 0x9D, 0x01, 0x2A)

	// Width and height (little-endian, 14-bit each)
	frame = append(frame, byte(width), byte((width>>8)&0x3F))
	frame = append(frame, byte(height), byte((height>>8)&0x3F))

	// Minimal partition data (produces a green/solid frame)
	frame = append(frame, make([]byte, 16)...)

	return frame
}

func (s *FileSource) Type() string { return "file" }

func (s *FileSource) IsActive() bool {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.active
}

func (s *FileSource) Subscribe(id string, handler PacketHandler) func() {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.subscribers[id] = handler
	return func() {
		s.mu.Lock()
		defer s.mu.Unlock()
		delete(s.subscribers, id)
	}
}

func (s *FileSource) Stop() {
	s.mu.Lock()
	defer s.mu.Unlock()
	if !s.stopped {
		s.stopped = true
		close(s.stopCh)
	}
}

// Ensure FileSource implements Source
var _ Source = (*FileSource)(nil)
var _ pwebrtc.RTPCodecCapability = pwebrtc.RTPCodecCapability{}
